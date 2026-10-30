//! The normalized catalog: restaurants/stores, where they were seen from,
//! menu categories and menu items, with FTS5 indexes for offline search.
//!
//! Service crates map their upstream JSON into these rows and upsert them
//! whenever they fetch list or menu data (see `grabfood::index`). Upserts
//! never erase known data with unknowns: a field the new row doesn't carry
//! (e.g. cuisines in a "recommended" response) keeps its stored value.
//!
//! Text search uses two external-content FTS5 tables per indexed table:
//! `unicode61 remove_diacritics 2` (whole words and prefixes; "cafe" finds
//! "Café") and `trigram remove_diacritics 1` (substrings of ≥ 3 characters;
//! "isig" finds "Sisig"). Word matches rank first (bm25), then trigram-only
//! matches.
//!
//! "Near" queries use a bounding box on indexed columns: restaurants whose
//! coordinates are known (from menus), or that were *seen from* a delivery
//! location near the point (`restaurant_locations`), then an exact haversine
//! filter. Distance, ETA and fee come from the nearest sighting when there is
//! one, since they depend on the delivery location.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;

use super::{Cache, round_coord_to};
use crate::error::Result;
use crate::jwt;

/// Catalog tables (for stats/clear).
pub(crate) const TABLES: [&str; 4] = [
    "restaurants",
    "restaurant_locations",
    "menu_categories",
    "menu_items",
];

/// Decimals of the location bucket in `restaurant_locations` (~110 m).
const BUCKET_DECIMALS: usize = 3;

/// A sighting within this distance of the query point counts as "seen from
/// this address".
const SIGHTING_RADIUS_KM: f64 = 1.0;

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

fn opt_json(v: Option<&Value>) -> Option<String> {
    v.map(Value::to_string)
}

fn cuisines_text(c: &[String]) -> Option<String> {
    let parts: Vec<&str> = c
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

pub(crate) fn bucket(lat: f64, lon: f64) -> String {
    format!(
        "{},{}",
        round_coord_to(lat, BUCKET_DECIMALS),
        round_coord_to(lon, BUCKET_DECIMALS)
    )
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

/// `(min_lat, max_lat, min_lon, max_lon)` around a point.
fn bbox(lat: f64, lon: f64, km: f64) -> (f64, f64, f64, f64) {
    let dlat = km / 111.32;
    let dlon = km / (111.32 * lat.to_radians().cos().abs().max(0.01));
    (lat - dlat, lat + dlat, lon - dlon, lon + dlon)
}

/// Words of a free-text query (letters/digits only; FTS syntax is never
/// passed through).
fn query_terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Word-index query: every term as a prefix.
fn fts_word_query(terms: &[String]) -> String {
    terms
        .iter()
        .map(|t| format!("\"{t}\"*"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Trigram query: every term as a substring. Needs every term ≥ 3 chars.
fn fts_trigram_query(terms: &[String]) -> Option<String> {
    terms.iter().all(|t| t.chars().count() >= 3).then(|| {
        terms
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(" AND ")
    })
}

/// Row ids matching `text` in `<table>_fts` (ranked) then `<table>_tri`.
fn text_rids(conn: &Connection, table: &str, text: &str) -> Result<Vec<i64>> {
    let terms = query_terms(text);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let mut out: Vec<i64> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let sql = format!(
        "SELECT rowid FROM {table}_fts WHERE {table}_fts MATCH ?1 ORDER BY bm25({table}_fts)"
    );
    let mut stmt = conn.prepare(&sql)?;
    for rid in stmt.query_map([fts_word_query(&terms)], |r| r.get::<_, i64>(0))? {
        let rid = rid?;
        if seen.insert(rid) {
            out.push(rid);
        }
    }
    if let Some(q) = fts_trigram_query(&terms) {
        let sql = format!(
            "SELECT rowid FROM {table}_tri WHERE {table}_tri MATCH ?1 ORDER BY bm25({table}_tri)"
        );
        let mut stmt = conn.prepare(&sql)?;
        for rid in stmt.query_map([q], |r| r.get::<_, i64>(0))? {
            let rid = rid?;
            if seen.insert(rid) {
                out.push(rid);
            }
        }
    }
    Ok(out)
}

fn upsert_restaurant(conn: &Connection, r: &RestaurantRow, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO restaurants (id, namespace, name, chain_name, branch_name, business_type,
            cuisines, rating, vote_count, price_level, lat, lon, delivery_fee_minor,
            delivery_eta_min, has_promo, promo_text, free_delivery, open_now, opening_hours,
            distance_km, photo_url, fetched_at, raw_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
            ?18, ?19, ?20, ?21, ?22, ?23)
         ON CONFLICT(id) DO UPDATE SET
            namespace = excluded.namespace,
            name = CASE WHEN excluded.name <> '' THEN excluded.name ELSE name END,
            chain_name = COALESCE(excluded.chain_name, chain_name),
            branch_name = COALESCE(excluded.branch_name, branch_name),
            business_type = COALESCE(excluded.business_type, business_type),
            cuisines = COALESCE(excluded.cuisines, cuisines),
            rating = COALESCE(excluded.rating, rating),
            vote_count = COALESCE(excluded.vote_count, vote_count),
            price_level = COALESCE(excluded.price_level, price_level),
            lat = COALESCE(excluded.lat, lat),
            lon = COALESCE(excluded.lon, lon),
            delivery_fee_minor = COALESCE(excluded.delivery_fee_minor, delivery_fee_minor),
            delivery_eta_min = COALESCE(excluded.delivery_eta_min, delivery_eta_min),
            has_promo = COALESCE(excluded.has_promo, has_promo),
            promo_text = COALESCE(excluded.promo_text, promo_text),
            free_delivery = COALESCE(excluded.free_delivery, free_delivery),
            open_now = COALESCE(excluded.open_now, open_now),
            opening_hours = COALESCE(excluded.opening_hours, opening_hours),
            distance_km = COALESCE(excluded.distance_km, distance_km),
            photo_url = COALESCE(excluded.photo_url, photo_url),
            fetched_at = excluded.fetched_at,
            raw_json = COALESCE(excluded.raw_json, raw_json)",
        params![
            r.id,
            r.namespace,
            r.name,
            r.chain_name,
            r.branch_name,
            r.business_type,
            cuisines_text(&r.cuisines),
            r.rating,
            r.vote_count,
            r.price_level,
            r.lat,
            r.lon,
            r.delivery_fee_minor,
            r.delivery_eta_min,
            r.has_promo,
            r.promo_text,
            r.free_delivery,
            r.open_now,
            opt_json(r.opening_hours.as_ref()),
            r.distance_km,
            r.photo_url,
            now,
            opt_json(r.raw.as_ref()),
        ],
    )?;
    Ok(())
}

fn upsert_sighting(
    conn: &Connection,
    r: &RestaurantRow,
    lat: f64,
    lon: f64,
    now: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO restaurant_locations (restaurant_id, bucket, search_lat, search_lon,
            distance_km, delivery_eta_min, delivery_fee_minor, free_delivery, fetched_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(restaurant_id, bucket) DO UPDATE SET
            search_lat = excluded.search_lat,
            search_lon = excluded.search_lon,
            distance_km = COALESCE(excluded.distance_km, distance_km),
            delivery_eta_min = COALESCE(excluded.delivery_eta_min, delivery_eta_min),
            delivery_fee_minor = COALESCE(excluded.delivery_fee_minor, delivery_fee_minor),
            free_delivery = COALESCE(excluded.free_delivery, free_delivery),
            fetched_at = excluded.fetched_at",
        params![
            r.id,
            bucket(lat, lon),
            lat,
            lon,
            r.distance_km,
            r.delivery_eta_min,
            r.delivery_fee_minor,
            r.free_delivery,
            now
        ],
    )?;
    Ok(())
}

fn upsert_item(conn: &Connection, it: &ItemRow, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO menu_items (id, restaurant_id, category_id, name, description, price_minor,
            discounted_price_minor, available, image_url, fetched_at, raw_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(id) DO UPDATE SET
            restaurant_id = excluded.restaurant_id,
            category_id = COALESCE(excluded.category_id, category_id),
            name = CASE WHEN excluded.name <> '' THEN excluded.name ELSE name END,
            description = COALESCE(excluded.description, description),
            price_minor = COALESCE(excluded.price_minor, price_minor),
            discounted_price_minor = excluded.discounted_price_minor,
            available = COALESCE(excluded.available, available),
            image_hash = CASE WHEN excluded.image_url IS image_url THEN image_hash ELSE NULL END,
            image_url = COALESCE(excluded.image_url, image_url),
            fetched_at = excluded.fetched_at,
            raw_json = COALESCE(excluded.raw_json, raw_json)",
        params![
            it.id,
            it.restaurant_id,
            it.category_id,
            it.name,
            it.description.as_deref().filter(|d| !d.is_empty()),
            it.price_minor,
            it.discounted_price_minor,
            it.available,
            it.image_url,
            now,
            opt_json(it.raw.as_ref()),
        ],
    )?;
    Ok(())
}

const RESTAURANT_COLS: &str = "rid, id, namespace, name, chain_name, branch_name, business_type, \
    cuisines, rating, vote_count, price_level, lat, lon, delivery_fee_minor, delivery_eta_min, \
    has_promo, promo_text, free_delivery, open_now, opening_hours, distance_km, photo_url, \
    fetched_at";

fn restaurant_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, RestaurantHit)> {
    let cuisines: Option<String> = r.get(7)?;
    let hours: Option<String> = r.get(19)?;
    let row = RestaurantRow {
        id: r.get(1)?,
        namespace: r.get(2)?,
        name: r.get(3)?,
        chain_name: r.get(4)?,
        branch_name: r.get(5)?,
        business_type: r.get(6)?,
        cuisines: cuisines
            .map(|c| c.split(", ").map(ToString::to_string).collect())
            .unwrap_or_default(),
        rating: r.get(8)?,
        vote_count: r.get(9)?,
        price_level: r.get(10)?,
        lat: r.get(11)?,
        lon: r.get(12)?,
        delivery_fee_minor: r.get(13)?,
        delivery_eta_min: r.get(14)?,
        has_promo: r.get(15)?,
        promo_text: r.get(16)?,
        free_delivery: r.get(17)?,
        open_now: r.get(18)?,
        opening_hours: hours.and_then(|h| serde_json::from_str(&h).ok()),
        distance_km: r.get(20)?,
        photo_url: r.get(21)?,
        raw: None,
    };
    Ok((
        r.get(0)?,
        RestaurantHit {
            row,
            fetched_at: r.get(22)?,
        },
    ))
}

/// Location-dependent fields of the sighting nearest `near`, if one is
/// within [`SIGHTING_RADIUS_KM`].
#[derive(Debug, Clone, Copy)]
struct Sighting {
    distance_km: Option<f64>,
    eta: Option<i64>,
    fee: Option<i64>,
    free: Option<bool>,
    fetched_at: i64,
}

fn nearest_sighting(conn: &Connection, id: &str, near: &Near) -> Result<Option<Sighting>> {
    let (a, b, c, d) = bbox(near.lat, near.lon, SIGHTING_RADIUS_KM);
    let mut stmt = conn.prepare_cached(
        "SELECT search_lat, search_lon, distance_km, delivery_eta_min, delivery_fee_minor,
                free_delivery, fetched_at
         FROM restaurant_locations
         WHERE restaurant_id = ?1 AND search_lat BETWEEN ?2 AND ?3 AND search_lon BETWEEN ?4 AND ?5",
    )?;
    let rows = stmt.query_map(params![id, a, b, c, d], |r| {
        Ok((
            r.get::<_, f64>(0)?,
            r.get::<_, f64>(1)?,
            Sighting {
                distance_km: r.get(2)?,
                eta: r.get(3)?,
                fee: r.get(4)?,
                free: r.get(5)?,
                fetched_at: r.get(6)?,
            },
        ))
    })?;
    let mut best: Option<(f64, Sighting)> = None;
    for row in rows {
        let (lat, lon, s) = row?;
        let d = haversine_km(near.lat, near.lon, lat, lon);
        if d <= SIGHTING_RADIUS_KM && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, s));
        }
    }
    Ok(best.map(|(_, s)| s))
}

/// Apply `near` to a hit: use the nearest sighting's distance/ETA/fee, or the
/// haversine distance from known coordinates. `None` if it's not near.
fn localize(
    conn: &Connection,
    mut hit: RestaurantHit,
    near: &Near,
) -> Result<Option<RestaurantHit>> {
    if let Some(s) = nearest_sighting(conn, &hit.row.id, near)? {
        let r = &mut hit.row;
        r.distance_km = s.distance_km.or(r.distance_km);
        r.delivery_eta_min = s.eta;
        r.delivery_fee_minor = s.fee;
        r.free_delivery = s.free;
        hit.fetched_at = hit.fetched_at.max(s.fetched_at);
        let ok = hit.row.distance_km.is_none_or(|d| d <= near.radius_km);
        return Ok(ok.then_some(hit));
    }
    match (hit.row.lat, hit.row.lon) {
        (Some(lat), Some(lon)) => {
            let d = haversine_km(near.lat, near.lon, lat, lon);
            if d > near.radius_km {
                return Ok(None);
            }
            // Seen from elsewhere: ETA/fee from another address don't apply.
            hit.row.distance_km = Some(d);
            hit.row.delivery_eta_min = None;
            hit.row.delivery_fee_minor = None;
            hit.row.free_delivery = None;
            Ok(Some(hit))
        }
        _ => Ok(None),
    }
}

/// Restaurant row ids inside the bounding box of `near` (by coordinates or by
/// a sighting from near the point).
fn near_rids(conn: &Connection, near: &Near) -> Result<Vec<i64>> {
    let (a, b, c, d) = bbox(near.lat, near.lon, near.radius_km);
    let (e, f, g, h) = bbox(near.lat, near.lon, SIGHTING_RADIUS_KM);
    let mut stmt = conn.prepare(
        "SELECT rid FROM restaurants WHERE lat BETWEEN ?1 AND ?2 AND lon BETWEEN ?3 AND ?4
         UNION
         SELECT r.rid FROM restaurant_locations l JOIN restaurants r ON r.id = l.restaurant_id
         WHERE l.search_lat BETWEEN ?5 AND ?6 AND l.search_lon BETWEEN ?7 AND ?8",
    )?;
    let rids = stmt
        .query_map(params![a, b, c, d, e, f, g, h], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rids)
}

fn load_restaurant(conn: &Connection, rid: i64) -> Result<Option<RestaurantHit>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {RESTAURANT_COLS} FROM restaurants WHERE rid = ?1"
    ))?;
    Ok(stmt
        .query_row([rid], restaurant_from_row)
        .optional()?
        .map(|(_, h)| h))
}

fn load_restaurant_by_id(conn: &Connection, id: &str) -> Result<Option<RestaurantHit>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {RESTAURANT_COLS} FROM restaurants WHERE id = ?1"
    ))?;
    Ok(stmt
        .query_row([id], restaurant_from_row)
        .optional()?
        .map(|(_, h)| h))
}

pub(crate) fn search_restaurants(
    conn: &Connection,
    q: &RestaurantQuery,
) -> Result<Vec<RestaurantHit>> {
    let text = q.text.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let rids: Vec<i64> = match (text, &q.near) {
        (Some(t), _) => text_rids(conn, "restaurants", t)?,
        (None, Some(n)) => near_rids(conn, n)?,
        (None, None) => {
            let mut stmt = conn.prepare("SELECT rid FROM restaurants ORDER BY fetched_at DESC")?;
            stmt.query_map([], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        }
    };
    let mut out = Vec::new();
    for rid in rids {
        let Some(hit) = load_restaurant(conn, rid)? else {
            continue;
        };
        if q.namespace
            .as_deref()
            .is_some_and(|ns| ns != hit.row.namespace)
        {
            continue;
        }
        let hit = match &q.near {
            Some(n) => match localize(conn, hit, n)? {
                Some(h) => h,
                None => continue,
            },
            None => hit,
        };
        out.push(hit);
    }
    // Without text there's no relevance order: nearest first.
    if text.is_none() && q.near.is_some() {
        out.sort_by(|a, b| {
            let da = a.row.distance_km.unwrap_or(f64::MAX);
            let db = b.row.distance_km.unwrap_or(f64::MAX);
            da.total_cmp(&db)
        });
    }
    if let Some(l) = q.limit {
        out.truncate(l);
    }
    Ok(out)
}

const ITEM_COLS: &str = "rid, id, restaurant_id, category_id, name, description, price_minor, \
    discounted_price_minor, available, image_url, fetched_at";

fn item_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(ItemRow, i64)> {
    Ok((
        ItemRow {
            id: r.get(1)?,
            restaurant_id: r.get(2)?,
            category_id: r.get(3)?,
            name: r.get(4)?,
            description: r.get(5)?,
            price_minor: r.get(6)?,
            discounted_price_minor: r.get(7)?,
            available: r.get(8)?,
            image_url: r.get(9)?,
            raw: None,
        },
        r.get(10)?,
    ))
}

fn effective_price(it: &ItemRow) -> Option<i64> {
    match (it.discounted_price_minor, it.price_minor) {
        (Some(d), Some(p)) if d > 0 => Some(d.min(p)),
        (Some(d), None) => Some(d),
        (_, p) => p,
    }
}

pub(crate) fn search_items(conn: &Connection, q: &ItemQuery) -> Result<Vec<ItemHit>> {
    let text = q.text.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let mut stmt_rid = conn.prepare_cached(&format!(
        "SELECT {ITEM_COLS} FROM menu_items WHERE rid = ?1"
    ))?;
    let mut items: Vec<(ItemRow, i64)> = Vec::new();
    match (text, &q.restaurant_id) {
        (Some(t), _) => {
            for rid in text_rids(conn, "menu_items", t)? {
                if let Some(it) = stmt_rid.query_row([rid], item_from_row).optional()? {
                    items.push(it);
                }
            }
        }
        (None, Some(rest)) => {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ITEM_COLS} FROM menu_items WHERE restaurant_id = ?1 ORDER BY rid"
            ))?;
            for it in stmt.query_map([rest], item_from_row)? {
                items.push(it?);
            }
        }
        (None, None) => {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ITEM_COLS} FROM menu_items ORDER BY fetched_at DESC"
            ))?;
            for it in stmt.query_map([], item_from_row)? {
                items.push(it?);
            }
        }
    }
    drop(stmt_rid);

    let mut restaurants: HashMap<String, Option<RestaurantHit>> = HashMap::new();
    let mut out = Vec::new();
    for (item, fetched_at) in items {
        if q.restaurant_id
            .as_deref()
            .is_some_and(|r| r != item.restaurant_id)
        {
            continue;
        }
        if q.available_only && item.available == Some(false) {
            continue;
        }
        if let Some(max) = q.max_price_minor
            && effective_price(&item).is_none_or(|p| p > max)
        {
            continue;
        }
        let rest = match restaurants.get(&item.restaurant_id) {
            Some(r) => r.clone(),
            None => {
                let mut r = load_restaurant_by_id(conn, &item.restaurant_id)?;
                if let Some(n) = &q.near {
                    r = match r {
                        Some(hit) => localize(conn, hit, n)?,
                        None => None,
                    };
                }
                restaurants.insert(item.restaurant_id.clone(), r.clone());
                r
            }
        };
        if q.near.is_some() && rest.is_none() {
            continue;
        }
        out.push(ItemHit {
            item,
            restaurant: rest,
            fetched_at,
        });
        if q.limit.is_some_and(|l| out.len() >= l) {
            break;
        }
    }
    Ok(out)
}

/// Delete catalog rows last fetched before `cutoff`. Returns counts for
/// restaurants, restaurant_locations, menu_categories, menu_items.
pub(crate) fn prune_older_than(conn: &Connection, cutoff: i64) -> Result<[u64; 4]> {
    let mut out = [0u64; 4];
    for (i, t) in [
        "restaurants",
        "restaurant_locations",
        "menu_categories",
        "menu_items",
    ]
    .iter()
    .enumerate()
    {
        out[i] = conn.execute(&format!("DELETE FROM {t} WHERE fetched_at < ?1"), [cutoff])? as u64;
    }
    Ok(out)
}

impl Cache {
    /// Upsert restaurants seen in a list response. `seen_from` is the
    /// delivery location of the request; when given, each row's distance /
    /// ETA / fee are also recorded for that location. No-op when writes are
    /// off.
    pub fn index_restaurants(
        &self,
        rows: &[RestaurantRow],
        seen_from: Option<(f64, f64)>,
    ) -> Result<()> {
        self.index_restaurants_at(rows, seen_from, jwt::now())
    }

    pub fn index_restaurants_at(
        &self,
        rows: &[RestaurantRow],
        seen_from: Option<(f64, f64)>,
        now: i64,
    ) -> Result<()> {
        if !self.mode.writes() || rows.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        for r in rows.iter().filter(|r| !r.id.is_empty()) {
            upsert_restaurant(&tx, r, now)?;
            if let Some((lat, lon)) = seen_from {
                upsert_sighting(&tx, r, lat, lon, now)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Upsert individual items (e.g. dish hits in search results), keeping
    /// any category already known. No-op when writes are off.
    pub fn index_items(&self, items: &[ItemRow]) -> Result<()> {
        self.index_items_at(items, jwt::now())
    }

    pub fn index_items_at(&self, items: &[ItemRow], now: i64) -> Result<()> {
        if !self.mode.writes() || items.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        for it in items.iter().filter(|i| !i.id.is_empty()) {
            upsert_item(&tx, it, now)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Store a full menu: the restaurant, its categories (replacing the old
    /// ones) and items; items no longer on the menu are removed. No-op when
    /// writes are off.
    pub fn index_menu(
        &self,
        restaurant: &RestaurantRow,
        categories: &[CategoryRow],
        items: &[ItemRow],
        seen_from: Option<(f64, f64)>,
    ) -> Result<()> {
        self.index_menu_at(restaurant, categories, items, seen_from, jwt::now())
    }

    pub fn index_menu_at(
        &self,
        restaurant: &RestaurantRow,
        categories: &[CategoryRow],
        items: &[ItemRow],
        seen_from: Option<(f64, f64)>,
        now: i64,
    ) -> Result<()> {
        if !self.mode.writes() || restaurant.id.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        upsert_restaurant(&tx, restaurant, now)?;
        if let Some((lat, lon)) = seen_from {
            upsert_sighting(&tx, restaurant, lat, lon, now)?;
        }
        tx.execute(
            "DELETE FROM menu_categories WHERE restaurant_id = ?1",
            [&restaurant.id],
        )?;
        for c in categories {
            tx.execute(
                "INSERT OR REPLACE INTO menu_categories
                    (restaurant_id, id, name, sort, fetched_at, raw_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    restaurant.id,
                    c.id,
                    c.name,
                    c.sort,
                    now,
                    opt_json(c.raw.as_ref())
                ],
            )?;
        }
        for it in items.iter().filter(|i| !i.id.is_empty()) {
            upsert_item(&tx, it, now)?;
        }
        tx.execute(
            "DELETE FROM menu_items WHERE restaurant_id = ?1 AND fetched_at < ?2",
            params![restaurant.id, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Query restaurants in the catalog (works in every mode: it only reads
    /// what earlier fetches stored).
    pub fn search_restaurants(&self, q: &RestaurantQuery) -> Result<Vec<RestaurantHit>> {
        search_restaurants(&*self.conn()?, q)
    }

    /// Query menu items in the catalog.
    pub fn search_items(&self, q: &ItemQuery) -> Result<Vec<ItemHit>> {
        search_items(&*self.conn()?, q)
    }

    /// One restaurant by id.
    pub fn restaurant(&self, id: &str) -> Result<Option<RestaurantHit>> {
        load_restaurant_by_id(&*self.conn()?, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{CacheConfig, CacheMode};
    use serde_json::json;

    fn cache(dir: &std::path::Path) -> Cache {
        Cache::new(dir, CacheConfig::default(), CacheMode::Normal)
    }

    fn rest(id: &str, name: &str, cuisines: &[&str]) -> RestaurantRow {
        RestaurantRow {
            id: id.into(),
            namespace: "food".into(),
            name: name.into(),
            cuisines: cuisines.iter().map(ToString::to_string).collect(),
            ..RestaurantRow::default()
        }
    }

    fn item(id: &str, rest: &str, name: &str, price: i64) -> ItemRow {
        ItemRow {
            id: id.into(),
            restaurant_id: rest.into(),
            name: name.into(),
            price_minor: Some(price),
            available: Some(true),
            ..ItemRow::default()
        }
    }

    fn ids(h: &[RestaurantHit]) -> Vec<&str> {
        h.iter().map(|h| h.row.id.as_str()).collect()
    }

    #[test]
    fn upsert_keeps_known_fields_and_fts_follows_renames() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let c = cache(dir.path());
        let mut a = rest("A", "Café Kanto", &["Filipino", "Sisig"]);
        a.rating = Some(4.5);
        c.index_restaurants_at(&[a], None, 100)?;
        // A sparser row (no cuisines/rating) doesn't erase them.
        c.index_restaurants_at(&[rest("A", "Cafe Kanto Branch", &[])], None, 200)?;
        let hit = c
            .restaurant("A")?
            .ok_or_else(|| crate::Error::not_found("A"))?;
        assert_eq!(hit.row.name, "Cafe Kanto Branch");
        assert_eq!(hit.row.cuisines, vec!["Filipino", "Sisig"]);
        assert_eq!(hit.row.rating, Some(4.5));
        assert_eq!(hit.fetched_at, 200);
        // The FTS index follows the rename: old-only words no longer match.
        let q = |t: &str| RestaurantQuery {
            text: Some(t.into()),
            ..RestaurantQuery::default()
        };
        assert_eq!(ids(&c.search_restaurants(&q("branch"))?), vec!["A"]);
        assert_eq!(c.search_restaurants(&q("café"))?.len(), 1);
        Ok(())
    }

    #[test]
    fn fts_diacritics_prefix_and_substring() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let c = cache(dir.path());
        c.index_restaurants_at(
            &[
                rest("A", "Café Kanto", &["Filipino"]),
                rest(
                    "B",
                    "Jollibee - Example Branch",
                    &["Fried Chicken", "Burger"],
                ),
                rest("C", "Kyu Kyu Ramen", &["Japanese", "Ramen"]),
            ],
            None,
            100,
        )?;
        c.index_items_at(
            &[
                item("I1", "A", "Pork Sisig", 25_000),
                item("I2", "A", "Tofu Sisig", 33_000),
                item("I3", "C", "Tonkotsu Ramen", 39_000),
                ItemRow {
                    description: Some("crispy pork with calamansi".into()),
                    ..item("I4", "B", "Chickenjoy", 15_000)
                },
            ],
            100,
        )?;
        let r = |t: &str| -> Result<Vec<String>> {
            Ok(c.search_restaurants(&RestaurantQuery {
                text: Some(t.into()),
                ..RestaurantQuery::default()
            })?
            .into_iter()
            .map(|h| h.row.id)
            .collect())
        };
        let i = |t: &str, max: Option<i64>| -> Result<Vec<String>> {
            Ok(c.search_items(&ItemQuery {
                text: Some(t.into()),
                max_price_minor: max,
                ..ItemQuery::default()
            })?
            .into_iter()
            .map(|h| h.item.id)
            .collect())
        };
        // Diacritics both ways.
        assert_eq!(r("cafe")?, vec!["A"]);
        assert_eq!(r("CAFÉ")?, vec!["A"]);
        // Prefix, cuisine, multi-word AND.
        assert_eq!(r("jolli")?, vec!["B"]);
        assert_eq!(r("japanese")?, vec!["C"]);
        assert_eq!(r("fried chicken")?, vec!["B"]);
        assert!(r("fried ramen")?.is_empty());
        // Substring via trigram ("ibee" is inside "Jollibee").
        assert_eq!(r("ibee")?, vec!["B"]);
        // FTS syntax in user input is neutralized.
        assert!(r("\"* OR NOT (")?.is_empty());
        // Items: name, description, partial, price filter.
        assert_eq!(i("sisig", None)?.len(), 2);
        assert_eq!(i("isig", None)?.len(), 2);
        assert_eq!(i("sisig", Some(30_000))?, vec!["I1"]);
        assert_eq!(i("calamansi", None)?, vec!["I4"]);
        let hit = c.search_items(&ItemQuery {
            text: Some("tonkotsu".into()),
            ..ItemQuery::default()
        })?;
        assert_eq!(
            hit[0].restaurant.as_ref().map(|r| r.row.name.as_str()),
            Some("Kyu Kyu Ramen")
        );
        Ok(())
    }

    #[test]
    fn near_uses_sightings_and_coordinates() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let c = cache(dir.path());
        let home = (1.2300, 103.8000);
        let far_home = (1.3300, 103.7800); // ~11 km away
        let mut seen = rest("SEEN", "Seen From Home", &[]);
        seen.distance_km = Some(0.4);
        seen.delivery_eta_min = Some(25);
        seen.delivery_fee_minor = Some(4900);
        c.index_restaurants_at(&[seen.clone()], Some(home), 100)?;
        // Same restaurant seen from elsewhere with other numbers.
        let mut seen_far = seen.clone();
        seen_far.distance_km = Some(9.0);
        seen_far.delivery_eta_min = Some(60);
        c.index_restaurants_at(&[seen_far], Some(far_home), 110)?;
        // Known coordinates ~1.1 km away, never seen from home.
        let mut coords = rest("COORDS", "Has Coordinates", &[]);
        coords.lat = Some(1.2400);
        coords.lon = Some(103.8000);
        // Only seen from the far address.
        let mut other = rest("OTHER", "Other Side", &[]);
        other.distance_km = Some(0.5);
        c.index_restaurants_at(&[coords], None, 100)?;
        c.index_restaurants_at(&[other], Some(far_home), 100)?;

        let near = |km: f64| RestaurantQuery {
            near: Some(Near {
                lat: home.0 + 0.0003, // a few metres off: same address
                lon: home.1,
                radius_km: km,
            }),
            ..RestaurantQuery::default()
        };
        let hits = c.search_restaurants(&near(6.0))?;
        assert_eq!(ids(&hits), vec!["SEEN", "COORDS"]);
        // Home's sighting numbers, not the far address's.
        assert_eq!(hits[0].row.distance_km, Some(0.4));
        assert_eq!(hits[0].row.delivery_eta_min, Some(25));
        assert_eq!(hits[0].row.delivery_fee_minor, Some(4900));
        let d = hits[1].row.distance_km.unwrap_or(0.0);
        assert!((1.0..1.3).contains(&d), "{d}");
        assert_eq!(hits[1].row.delivery_eta_min, None);
        // Radius filter.
        assert_eq!(ids(&c.search_restaurants(&near(1.0))?), vec!["SEEN"]);

        // Item near filter follows its restaurant.
        c.index_items_at(
            &[
                item("I1", "SEEN", "Sisig", 20_000),
                item("I2", "OTHER", "Sisig", 10_000),
            ],
            100,
        )?;
        let items = c.search_items(&ItemQuery {
            text: Some("sisig".into()),
            near: near(6.0).near,
            ..ItemQuery::default()
        })?;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item.id, "I1");
        Ok(())
    }

    #[test]
    fn menu_replaces_categories_and_removed_items() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let c = cache(dir.path());
        let r = rest("R", "Resto", &[]);
        let cat = |id: &str| CategoryRow {
            restaurant_id: "R".into(),
            id: id.into(),
            name: id.into(),
            sort: 0,
            raw: Some(json!({})),
        };
        c.index_menu_at(
            &r,
            &[cat("C1"), cat("C2")],
            &[
                item("I1", "R", "Old Dish", 100),
                item("I2", "R", "Kept", 100),
            ],
            None,
            100,
        )?;
        let kept = ItemRow {
            category_id: Some("C1".into()),
            ..item("I2", "R", "Kept", 200)
        };
        c.index_menu_at(&r, &[cat("C1")], &[kept], None, 200)?;
        let s = c.stats_at(200)?;
        assert_eq!(s.tables["menu_categories"], 1);
        assert_eq!(s.tables["menu_items"], 1);
        let all = c.search_items(&ItemQuery {
            restaurant_id: Some("R".into()),
            ..ItemQuery::default()
        })?;
        assert_eq!(all[0].item.price_minor, Some(200));
        // The removed item is gone from the FTS index too.
        let gone = c.search_items(&ItemQuery {
            text: Some("old".into()),
            ..ItemQuery::default()
        })?;
        assert!(gone.is_empty());
        // Dish hits keep the category the menu assigned.
        c.index_items_at(&[item("I2", "R", "Kept", 150)], 300)?;
        let conn = c.conn()?;
        let cat: Option<String> = conn.query_row(
            "SELECT category_id FROM menu_items WHERE id = 'I2'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(cat.as_deref(), Some("C1"));
        Ok(())
    }

    #[test]
    fn writes_off_and_prune() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let c = cache(dir.path());
        let off = c.with_mode(CacheMode::Off);
        off.index_restaurants_at(&[rest("X", "X", &[])], Some((1.0, 1.0)), 100)?;
        off.index_items_at(&[item("I", "X", "x", 1)], 100)?;
        assert_eq!(c.stats_at(100)?.tables["restaurants"], 0);

        c.index_restaurants_at(&[rest("OLD", "Old", &[])], Some((1.0, 1.0)), 100)?;
        c.index_restaurants_at(&[rest("NEW", "New", &[])], Some((1.0, 1.0)), 10_000_000)?;
        c.index_items_at(&[item("I", "OLD", "x", 1)], 100)?;
        let max_age = i64::try_from(c.config.index_max_age_secs()).unwrap_or(0);
        let r = c.prune_at(100 + max_age + 1)?;
        assert_eq!(
            (r.restaurants, r.restaurant_locations, r.menu_items),
            (1, 1, 1)
        );
        assert!(c.restaurant("OLD")?.is_none());
        assert!(c.restaurant("NEW")?.is_some());
        Ok(())
    }

    #[test]
    fn helpers() {
        assert_eq!(bucket(1.23049, 103.8), "1.230,103.800");
        assert_eq!(fts_trigram_query(&["ab".into()]), None);
        let d = haversine_km(1.23, 103.8, 1.24, 103.8);
        assert!((d - 1.112).abs() < 0.01);
    }
}
