//! Mapping GrabFood responses into the core catalog (restaurants, sightings,
//! menu categories, items) and catalog rows back into [`Merchant`]s for
//! `--local` results. Indexing is best-effort: callers ignore its errors.

use grabcli_core::cache::{
    CacheHandle, CategoryRow, ItemHit, ItemRow, RestaurantHit, RestaurantRow,
};
use grabcli_core::money::Money;
use serde_json::Value;

use crate::discover::{Dish, Merchant};

/// Catalog namespace for restaurants found through GrabFood.
pub const NAMESPACE: &str = "food";

pub fn restaurant_row(m: &Merchant) -> RestaurantRow {
    RestaurantRow {
        id: m.id.clone(),
        namespace: NAMESPACE.into(),
        name: m.name.clone(),
        chain_name: m.chain.clone(),
        branch_name: m.branch.clone(),
        business_type: m.business_type.clone(),
        cuisines: m.cuisines.clone(),
        rating: m.rating,
        vote_count: m.votes,
        price_level: m.price_level,
        lat: None,
        lon: None,
        has_promo: Some(m.has_promo),
        promo_text: m.promo_tags.first().cloned().or_else(|| m.promo.clone()),
        open_now: m.open,
        opening_hours: m.opening_hours.clone(),
        photo_url: m.photo_url.clone(),
        distance_km: m.distance_km,
        delivery_eta_min: m.eta_min,
        delivery_fee_minor: m.fee.map(Money::minor),
        free_delivery: m.fee.map(|_| m.free_delivery),
        raw: m.raw.clone(),
    }
}

pub fn item_row(d: &Dish) -> ItemRow {
    ItemRow {
        id: d.id.clone(),
        restaurant_id: d.merchant_id.clone(),
        category_id: None,
        name: d.name.clone(),
        description: Some(d.description.clone()).filter(|s| !s.is_empty()),
        price_minor: d.price.map(Money::minor),
        discounted_price_minor: d.discounted.map(Money::minor),
        available: Some(d.available),
        image_url: d.image_url.clone(),
        raw: None,
    }
}

/// Upsert a fetched list page: merchants (and chain branches) with a sighting
/// from the delivery location, and their dish hits as menu items.
pub async fn index_merchants(cache: &CacheHandle, merchants: &[Merchant], seen_from: (f64, f64)) {
    let mut rows = Vec::new();
    let mut items = Vec::new();
    for m in merchants {
        rows.push(restaurant_row(m));
        rows.extend(m.branches.iter().map(restaurant_row));
        items.extend(m.dishes.iter().map(item_row));
    }
    let _ = cache.index_restaurants(&rows, Some(seen_from)).await;
    let _ = cache.index_items(&items).await;
}

fn s(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn i(v: &Value, ptr: &str) -> Option<i64> {
    v.pointer(ptr).and_then(Value::as_i64)
}

/// Rows for a merchant/menu response (`{"merchant": {...}}`): the
/// restaurant (with coordinates), its categories (or mart departments) and
/// items. `None` if there is no merchant.
pub fn menu_rows(root: &Value) -> Option<(RestaurantRow, Vec<CategoryRow>, Vec<ItemRow>)> {
    let m = root.get("merchant").filter(|m| m.is_object())?;
    let id = s(m, "/ID")?;
    let cuisines = match m.get("cuisine") {
        Some(Value::String(c)) => c
            .split(',')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(ToString::to_string)
            .collect(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect(),
        _ => Vec::new(),
    };
    let restaurant = RestaurantRow {
        id: id.clone(),
        namespace: NAMESPACE.into(),
        name: s(m, "/name").unwrap_or_else(|| id.clone()),
        chain_name: s(m, "/chainName"),
        branch_name: s(m, "/branchName"),
        cuisines,
        rating: m.get("rating").and_then(Value::as_f64).filter(|r| *r > 0.0),
        vote_count: i(m, "/voteCount"),
        lat: m.pointer("/latlng/latitude").and_then(Value::as_f64),
        lon: m.pointer("/latlng/longitude").and_then(Value::as_f64),
        open_now: m.pointer("/openingHours/open").and_then(Value::as_bool),
        opening_hours: m.get("openingHours").cloned(),
        photo_url: s(m, "/photoHref"),
        distance_km: m.get("distanceInKm").and_then(Value::as_f64),
        delivery_eta_min: i(m, "/ETA"),
        ..RestaurantRow::default()
    };
    let cats = m
        .pointer("/menu/categories")
        .or_else(|| m.pointer("/menu/departments"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut categories = Vec::new();
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (sort, cat) in cats.iter().enumerate() {
        let cat_id = s(cat, "/ID").unwrap_or_else(|| format!("#{sort}"));
        let mut raw = cat.clone();
        if let Some(o) = raw.as_object_mut() {
            o.remove("items");
        }
        categories.push(CategoryRow {
            restaurant_id: id.clone(),
            id: cat_id.clone(),
            name: s(cat, "/name").unwrap_or_default(),
            sort: i64::try_from(sort).unwrap_or(0),
            raw: Some(raw),
        });
        for it in cat
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(item_id) = s(it, "/ID") else {
                continue;
            };
            // An item can appear in several categories ("Best sellers");
            // keep its first one.
            if !seen.insert(item_id.clone()) {
                continue;
            }
            let price = i(it, "/priceInMinorUnit").or_else(|| i(it, "/priceV2/amountInMinor"));
            let discounted = i(it, "/discountedPriceInMin")
                .or_else(|| i(it, "/discountedPriceV2/amountInMinor"))
                .filter(|d| Some(*d) != price);
            items.push(ItemRow {
                id: item_id,
                restaurant_id: id.clone(),
                category_id: Some(cat_id.clone()),
                name: s(it, "/name").unwrap_or_default(),
                description: s(it, "/description"),
                price_minor: price,
                discounted_price_minor: discounted,
                available: it.get("available").and_then(Value::as_bool),
                image_url: s(it, "/imgHref").or_else(|| s(it, "/images/0")),
                raw: Some(it.clone()),
            });
        }
    }
    Some((restaurant, categories, items))
}

/// Upsert a fetched menu.
pub async fn index_menu(cache: &CacheHandle, root: &Value, seen_from: (f64, f64)) {
    if let Some((r, cats, items)) = menu_rows(root) {
        let _ = cache.index_menu(&r, &cats, &items, Some(seen_from)).await;
    }
}

/// A catalog restaurant as a list [`Merchant`].
pub fn merchant_from_hit(h: &RestaurantHit) -> Merchant {
    let r = &h.row;
    let tags: Vec<String> = r.promo_text.iter().cloned().collect();
    Merchant {
        id: r.id.clone(),
        name: r.name.clone(),
        chain: r.chain_name.clone(),
        branch: r.branch_name.clone(),
        business_type: r.business_type.clone(),
        cuisines: r.cuisines.clone(),
        rating: r.rating,
        votes: r.vote_count,
        price_level: r.price_level,
        eta_min: r.delivery_eta_min,
        distance_km: r.distance_km,
        fee: r.delivery_fee_minor.map(Money),
        fee_before: None,
        free_delivery: r.free_delivery.unwrap_or(false),
        has_promo: r.has_promo.unwrap_or(false),
        promo: None,
        promo_tags: tags,
        open: r.open_now,
        status: None,
        hours: r
            .opening_hours
            .as_ref()
            .and_then(|h| h.get("displayedHours"))
            .and_then(Value::as_str)
            .map(ToString::to_string),
        opening_hours: r.opening_hours.clone(),
        photo_url: r.photo_url.clone(),
        dishes: Vec::new(),
        branches: Vec::new(),
        fetched_at: Some(h.fetched_at),
        raw: None,
    }
}

/// A catalog item as a [`Dish`].
pub fn dish_from_hit(h: &ItemHit) -> Dish {
    let it = &h.item;
    Dish {
        id: it.id.clone(),
        merchant_id: it.restaurant_id.clone(),
        name: it.name.clone(),
        description: it.description.clone().unwrap_or_default(),
        price: it.price_minor.map(Money),
        discounted: it.discounted_price_minor.map(Money),
        available: it.available.unwrap_or(true),
        image_url: it.image_url.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::{parse_recommended, parse_search_page};
    use grabcli_core::cache::memory::MemoryCache;
    use grabcli_core::cache::{CacheConfig, CacheMode, ItemQuery, Near, RestaurantQuery};
    use grabcli_core::error::Result;
    use serde_json::json;
    use std::sync::Arc;

    const HOME: (f64, f64) = (1.23, 103.8);

    fn fixture(s: &str) -> Value {
        serde_json::from_str(s).unwrap_or(Value::Null)
    }

    fn near(km: f64) -> Option<Near> {
        Some(Near {
            lat: HOME.0,
            lon: HOME.1,
            radius_km: km,
        })
    }

    #[tokio::test]
    async fn fixtures_index_into_catalog() -> Result<()> {
        let cache = CacheHandle::new(
            Arc::new(MemoryCache::default()),
            CacheConfig::default(),
            CacheMode::Normal,
        );
        for raw in [
            include_str!("../../../docs/fixtures/discovery_search.json"),
            include_str!("../../../docs/fixtures/discovery_search_dishes.json"),
            include_str!("../../../docs/fixtures/discovery_category.json"),
        ] {
            index_merchants(&cache, &parse_search_page(&fixture(raw)).merchants, HOME).await;
        }
        let rec = parse_recommended(&fixture(include_str!(
            "../../../docs/fixtures/discovery_recommended.json"
        )));
        index_merchants(&cache, &rec.merchants, HOME).await;

        let all = cache
            .search_restaurants(&RestaurantQuery::default())
            .await?;
        assert!(all.len() >= 10, "{}", all.len());
        assert!(!cache.search_items(&ItemQuery::default()).await?.is_empty());

        // Restaurant text search (cuisine) near home.
        let japanese = cache
            .search_restaurants(&RestaurantQuery {
                text: Some("japanese".into()),
                near: near(6.0),
                ..RestaurantQuery::default()
            })
            .await?;
        assert!(japanese.iter().any(|h| h.row.id == "2-C4CXGAC2N6TDRA"));
        // Recommended rows kept the richer fields from search (rating) and
        // gained nothing wrong; recommended-only rows exist too.
        assert!(cache.restaurant("2-CYM3EZLHTNVGNT").await?.is_some());

        // Dishes under ₱300 near home, partial match.
        let cheap = cache
            .search_items(&ItemQuery {
                text: Some("isig".into()),
                near: near(6.0),
                max_price_minor: Some(30_000),
                ..ItemQuery::default()
            })
            .await?;
        assert!(!cheap.is_empty());
        for hit in &cheap {
            let d = dish_from_hit(hit);
            assert!(d.effective_price().is_some_and(|p| p.minor() <= 30_000));
            assert!(d.name.to_lowercase().contains("sisig") || d.description.contains("isig"));
        }
        let m = merchant_from_hit(
            cheap[0]
                .restaurant
                .as_ref()
                .ok_or_else(|| grabcli_core::Error::not_found("restaurant for dish"))?,
        );
        assert!(m.distance_km.is_some());
        Ok(())
    }

    #[test]
    fn menu_rows_from_merchant_response() {
        let root = json!({"merchant": {
            "ID": "2-EX", "name": "Example", "cuisine": "Filipino, Sisig",
            "latlng": {"latitude": 1.2301, "longitude": 103.7949},
            "ETA": 30, "openingHours": {"open": true, "displayedHours": "10:00-22:00"},
            "menu": {"categories": [
                {"ID": "C1", "name": "Best", "items": [
                    {"ID": "I1", "name": "Pork Sisig", "priceInMinorUnit": 25000,
                     "discountedPriceInMin": 20000, "available": true,
                     "imgHref": "https://food-cms.grab.com/i1.webp"}]},
                {"ID": "C2", "name": "Mains", "items": [
                    {"ID": "I1", "name": "Pork Sisig", "priceInMinorUnit": 25000},
                    {"ID": "I2", "name": "Rice", "priceInMinorUnit": 3000,
                     "description": "steamed", "available": false}]}
            ]}
        }});
        let (r, cats, items) = menu_rows(&root).unwrap_or_default();
        assert_eq!(r.id, "2-EX");
        assert_eq!(r.cuisines, vec!["Filipino", "Sisig"]);
        assert_eq!((r.lat, r.lon), (Some(1.2301), Some(103.7949)));
        assert_eq!(cats.len(), 2);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].category_id.as_deref(), Some("C1"));
        assert_eq!(items[0].discounted_price_minor, Some(20000));
        assert_eq!(items[1].available, Some(false));
        assert!(menu_rows(&json!({"merchant": null})).is_none());
    }
}
