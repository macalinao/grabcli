//! Plain data the [`Cache`](super::Cache) trait stores and returns: catalog
//! rows, queries and hits, and cached images. No storage here.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

/// A restaurant or store, as seen in one response.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RestaurantRow {
    pub id: String,
    /// Service namespace (`"food"`, `"mart"`).
    pub namespace: String,
    pub name: String,
    pub chain_name: Option<String>,
    pub branch_name: Option<String>,
    /// Upstream `businessType` (`FOOD` / `MART`).
    pub business_type: Option<String>,
    pub cuisines: Vec<String>,
    pub rating: Option<f64>,
    pub vote_count: Option<i64>,
    /// 1–3 (Grab's `priceTag`).
    pub price_level: Option<i64>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub has_promo: Option<bool>,
    pub promo_text: Option<String>,
    pub open_now: Option<bool>,
    pub opening_hours: Option<Value>,
    pub photo_url: Option<String>,
    // Location-dependent: also stored per sighting.
    pub distance_km: Option<f64>,
    pub delivery_eta_min: Option<i64>,
    pub delivery_fee_minor: Option<i64>,
    pub free_delivery: Option<bool>,
    #[serde(skip)]
    pub raw: Option<Value>,
}

/// A menu category / department.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CategoryRow {
    pub restaurant_id: String,
    pub id: String,
    pub name: String,
    pub sort: i64,
    #[serde(skip)]
    pub raw: Option<Value>,
}

/// A menu item (from a full menu, or a dish hit in search results).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ItemRow {
    pub id: String,
    pub restaurant_id: String,
    pub category_id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub price_minor: Option<i64>,
    pub discounted_price_minor: Option<i64>,
    pub available: Option<bool>,
    pub image_url: Option<String>,
    #[serde(skip)]
    pub raw: Option<Value>,
}

/// A point and radius for "near" queries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Near {
    pub lat: f64,
    pub lon: f64,
    pub radius_km: f64,
}

/// A restaurant query. All fields are optional filters.
#[derive(Debug, Clone, Default)]
pub struct RestaurantQuery {
    /// Free text matched against name and cuisines.
    pub text: Option<String>,
    pub near: Option<Near>,
    pub namespace: Option<String>,
    pub limit: Option<usize>,
}

/// An item query.
#[derive(Debug, Clone, Default)]
pub struct ItemQuery {
    /// Free text matched against item name and description.
    pub text: Option<String>,
    pub near: Option<Near>,
    /// Effective (discounted if any) price at most this, in minor units.
    pub max_price_minor: Option<i64>,
    pub available_only: bool,
    pub restaurant_id: Option<String>,
    pub limit: Option<usize>,
}

/// A restaurant from the catalog. Location-dependent fields come from the
/// sighting nearest the query point when there is one.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RestaurantHit {
    #[serde(flatten)]
    pub row: RestaurantRow,
    /// When this row (or the sighting used) was last fetched.
    pub fetched_at: i64,
}

/// An item from the catalog, with its restaurant.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ItemHit {
    #[serde(flatten)]
    pub item: ItemRow,
    pub restaurant: Option<RestaurantHit>,
    pub fetched_at: i64,
}

/// A cached image file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CachedImage {
    pub url: String,
    /// Local file, for backends that keep images on disk.
    pub path: Option<PathBuf>,
    pub hash: String,
    pub bytes: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub content_type: String,
    /// True if served from the store without downloading.
    pub cached: bool,
}

/// Only `https://` URLs on `grab.com` or a subdomain (the CDNs:
/// `food-cms.grab.com`, `huawei-food-cms.grab.com`, `assets-mobile.grab.com`,
/// …), with no credentials or custom port.
pub fn image_url_allowed(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else {
        return false;
    };
    let host_ok = u
        .host_str()
        .map(str::to_ascii_lowercase)
        .is_some_and(|h| h == "grab.com" || h.ends_with(".grab.com"));
    u.scheme() == "https"
        && host_ok
        && u.username().is_empty()
        && u.password().is_none()
        && u.port().is_none()
}

/// Great-circle distance in km.
pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371.0_f64;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

/// Words of a free-text query (letters/digits only; FTS syntax is never
/// passed through).
pub fn query_terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// An item's effective price: the discounted price when there is one.
pub fn effective_price(it: &ItemRow) -> Option<i64> {
    match (it.discounted_price_minor, it.price_minor) {
        (Some(d), Some(p)) if d > 0 => Some(d.min(p)),
        (Some(d), None) => Some(d),
        (_, p) => p,
    }
}

