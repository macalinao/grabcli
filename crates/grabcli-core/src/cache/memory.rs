//! In-memory [`Cache`] backends: [`MemoryCache`] (everything in maps, for
//! tests and short-lived embedders) and [`NoCache`] (stores nothing).
//!
//! `MemoryCache`'s catalog search is a plain case-insensitive substring
//! match on words, not the FTS ranking of the SQLite backend; "near" uses
//! known coordinates or a sighting within 1 km of the query point.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;

use super::types::{
    CachedImage, CategoryRow, ItemHit, ItemQuery, ItemRow, Near, RestaurantHit, RestaurantQuery,
    RestaurantRow, effective_price, haversine_km, query_terms,
};
use super::{Cache, CacheEntry, CacheKey, hex_sha256};
use crate::error::Result;

/// A sighting within this distance of the query point counts as "seen from
/// this address".
const SIGHTING_RADIUS_KM: f64 = 1.0;

#[derive(Debug, Clone)]
struct Sighting {
    lat: f64,
    lon: f64,
    distance_km: Option<f64>,
    eta: Option<i64>,
    fee: Option<i64>,
    free: Option<bool>,
    fetched_at: i64,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, CacheEntry>,
    restaurants: HashMap<String, (RestaurantRow, i64)>,
    sightings: HashMap<String, Vec<Sighting>>,
    categories: HashMap<String, Vec<CategoryRow>>,
    items: HashMap<(String, String), (ItemRow, i64)>,
    images: HashMap<String, (Vec<u8>, String, i64)>,
}

/// Everything in memory, behind one mutex.
#[derive(Default)]
pub struct MemoryCache {
    state: Mutex<State>,
}

impl std::fmt::Debug for MemoryCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryCache").finish_non_exhaustive()
    }
}

fn merge_opt<T: Clone>(new: &mut Option<T>, old: &Option<T>) {
    if new.is_none() {
        new.clone_from(old);
    }
}

/// Keep known fields when the new row lacks them.
fn merge_restaurant(mut new: RestaurantRow, old: &RestaurantRow) -> RestaurantRow {
    if new.cuisines.is_empty() {
        new.cuisines.clone_from(&old.cuisines);
    }
    if new.name.is_empty() {
        new.name.clone_from(&old.name);
    }
    merge_opt(&mut new.chain_name, &old.chain_name);
    merge_opt(&mut new.branch_name, &old.branch_name);
    merge_opt(&mut new.business_type, &old.business_type);
    merge_opt(&mut new.rating, &old.rating);
    merge_opt(&mut new.vote_count, &old.vote_count);
    merge_opt(&mut new.price_level, &old.price_level);
    merge_opt(&mut new.lat, &old.lat);
    merge_opt(&mut new.lon, &old.lon);
    merge_opt(&mut new.has_promo, &old.has_promo);
    merge_opt(&mut new.promo_text, &old.promo_text);
    merge_opt(&mut new.open_now, &old.open_now);
    merge_opt(&mut new.opening_hours, &old.opening_hours);
    merge_opt(&mut new.photo_url, &old.photo_url);
    new
}

fn words_match(terms: &[String], haystack: &str) -> bool {
    let h = haystack.to_lowercase();
    terms.iter().all(|t| h.contains(t.as_str()))
}

impl MemoryCache {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn upsert_restaurant(st: &mut State, r: &RestaurantRow, seen_from: Option<(f64, f64)>, now: i64) {
        let row = match st.restaurants.get(&r.id) {
            Some((old, _)) => merge_restaurant(r.clone(), old),
            None => r.clone(),
        };
        st.restaurants.insert(r.id.clone(), (row, now));
        if let Some((lat, lon)) = seen_from {
            let list = st.sightings.entry(r.id.clone()).or_default();
            list.retain(|s| haversine_km(s.lat, s.lon, lat, lon) > 0.11);
            list.push(Sighting {
                lat,
                lon,
                distance_km: r.distance_km,
                eta: r.delivery_eta_min,
                fee: r.delivery_fee_minor,
                free: r.free_delivery,
                fetched_at: now,
            });
        }
    }

    fn localize(st: &State, mut hit: RestaurantHit, near: &Near) -> Option<RestaurantHit> {
        let best = st.sightings.get(&hit.row.id).and_then(|list| {
            list.iter()
                .map(|s| (haversine_km(near.lat, near.lon, s.lat, s.lon), s))
                .filter(|(d, _)| *d <= SIGHTING_RADIUS_KM)
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(_, s)| s.clone())
        });
        if let Some(s) = best {
            let r = &mut hit.row;
            r.distance_km = s.distance_km.or(r.distance_km);
            r.delivery_eta_min = s.eta;
            r.delivery_fee_minor = s.fee;
            r.free_delivery = s.free;
            hit.fetched_at = hit.fetched_at.max(s.fetched_at);
            return hit
                .row
                .distance_km
                .is_none_or(|d| d <= near.radius_km)
                .then_some(hit);
        }
        let (lat, lon) = (hit.row.lat?, hit.row.lon?);
        let d = haversine_km(near.lat, near.lon, lat, lon);
        if d > near.radius_km {
            return None;
        }
        hit.row.distance_km = Some(d);
        hit.row.delivery_eta_min = None;
        hit.row.delivery_fee_minor = None;
        hit.row.free_delivery = None;
        Some(hit)
    }

    fn hit(st: &State, id: &str, near: Option<&Near>) -> Option<RestaurantHit> {
        let (row, fetched_at) = st.restaurants.get(id)?;
        let hit = RestaurantHit {
            row: row.clone(),
            fetched_at: *fetched_at,
        };
        match near {
            Some(n) => Self::localize(st, hit, n),
            None => Some(hit),
        }
    }
}

#[async_trait]
impl Cache for MemoryCache {
    async fn get_entry(&self, key: &CacheKey) -> Result<Option<CacheEntry>> {
        Ok(self.lock().entries.get(&key.hash()).cloned())
    }

    async fn put_entry(&self, key: &CacheKey, entry: &CacheEntry) -> Result<()> {
        self.lock().entries.insert(key.hash(), entry.clone());
        Ok(())
    }

    async fn index_restaurants(
        &self,
        rows: &[RestaurantRow],
        seen_from: Option<(f64, f64)>,
        now: i64,
    ) -> Result<()> {
        let mut st = self.lock();
        for r in rows.iter().filter(|r| !r.id.is_empty()) {
            Self::upsert_restaurant(&mut st, r, seen_from, now);
        }
        Ok(())
    }

    async fn index_items(&self, items: &[ItemRow], now: i64) -> Result<()> {
        let mut st = self.lock();
        for it in items.iter().filter(|i| !i.id.is_empty()) {
            let k = (it.restaurant_id.clone(), it.id.clone());
            let mut row = it.clone();
            if let Some((old, _)) = st.items.get(&k) {
                merge_opt(&mut row.category_id, &old.category_id);
            }
            st.items.insert(k, (row, now));
        }
        Ok(())
    }

    async fn index_menu(
        &self,
        restaurant: &RestaurantRow,
        categories: &[CategoryRow],
        items: &[ItemRow],
        seen_from: Option<(f64, f64)>,
        now: i64,
    ) -> Result<()> {
        let mut st = self.lock();
        Self::upsert_restaurant(&mut st, restaurant, seen_from, now);
        st.categories
            .insert(restaurant.id.clone(), categories.to_vec());
        st.items.retain(|(rid, _), _| rid != &restaurant.id);
        for it in items.iter().filter(|i| !i.id.is_empty()) {
            st.items.insert(
                (restaurant.id.clone(), it.id.clone()),
                (it.clone(), now),
            );
        }
        Ok(())
    }

    async fn search_restaurants(&self, q: &RestaurantQuery) -> Result<Vec<RestaurantHit>> {
        let st = self.lock();
        let terms = q.text.as_deref().map(query_terms).unwrap_or_default();
        let mut hits: Vec<RestaurantHit> = st
            .restaurants
            .keys()
            .filter_map(|id| Self::hit(&st, id, q.near.as_ref()))
            .filter(|h| q.namespace.as_ref().is_none_or(|ns| &h.row.namespace == ns))
            .filter(|h| {
                let r = &h.row;
                let text = format!(
                    "{} {} {}",
                    r.name,
                    r.chain_name.as_deref().unwrap_or_default(),
                    r.cuisines.join(" ")
                );
                words_match(&terms, &text)
            })
            .collect();
        hits.sort_by(|a, b| a.row.name.cmp(&b.row.name).then(a.row.id.cmp(&b.row.id)));
        if let Some(l) = q.limit {
            hits.truncate(l);
        }
        Ok(hits)
    }

    async fn search_items(&self, q: &ItemQuery) -> Result<Vec<ItemHit>> {
        let st = self.lock();
        let terms = q.text.as_deref().map(query_terms).unwrap_or_default();
        let mut hits: Vec<ItemHit> = st
            .items
            .values()
            .filter(|(it, _)| q.restaurant_id.as_ref().is_none_or(|r| &it.restaurant_id == r))
            .filter(|(it, _)| !q.available_only || it.available != Some(false))
            .filter(|(it, _)| {
                q.max_price_minor
                    .is_none_or(|m| effective_price(it).is_some_and(|p| p <= m))
            })
            .filter(|(it, _)| {
                let text = format!("{} {}", it.name, it.description.as_deref().unwrap_or_default());
                words_match(&terms, &text)
            })
            .filter_map(|(it, at)| {
                let restaurant = Self::hit(&st, &it.restaurant_id, q.near.as_ref());
                if q.near.is_some() && restaurant.is_none() {
                    return None;
                }
                Some(ItemHit {
                    item: it.clone(),
                    restaurant,
                    fetched_at: *at,
                })
            })
            .collect();
        hits.sort_by_key(|h| (effective_price(&h.item).unwrap_or(i64::MAX), h.item.id.clone()));
        if let Some(l) = q.limit {
            hits.truncate(l);
        }
        Ok(hits)
    }

    async fn restaurant(&self, id: &str) -> Result<Option<RestaurantHit>> {
        Ok(Self::hit(&self.lock(), id, None))
    }

    async fn image(&self, url: &str, now: i64) -> Result<Option<CachedImage>> {
        let mut st = self.lock();
        Ok(st.images.get_mut(url).map(|(bytes, ct, at)| {
            *at = now;
            CachedImage {
                url: url.to_string(),
                path: None,
                hash: hex_sha256(bytes),
                bytes: bytes.len() as u64,
                width: None,
                height: None,
                content_type: ct.clone(),
                cached: true,
            }
        }))
    }

    async fn put_image(
        &self,
        url: &str,
        bytes: &[u8],
        content_type: &str,
        now: i64,
    ) -> Result<CachedImage> {
        self.lock().images.insert(
            url.to_string(),
            (bytes.to_vec(), content_type.to_string(), now),
        );
        Ok(CachedImage {
            url: url.to_string(),
            path: None,
            hash: hex_sha256(bytes),
            bytes: bytes.len() as u64,
            width: None,
            height: None,
            content_type: content_type.to_string(),
            cached: false,
        })
    }
}

/// A cache that stores nothing: every read misses, every write succeeds.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoCache;

#[async_trait]
impl Cache for NoCache {
    async fn get_entry(&self, _key: &CacheKey) -> Result<Option<CacheEntry>> {
        Ok(None)
    }
    async fn put_entry(&self, _key: &CacheKey, _entry: &CacheEntry) -> Result<()> {
        Ok(())
    }
    async fn index_restaurants(
        &self,
        _rows: &[RestaurantRow],
        _seen_from: Option<(f64, f64)>,
        _now: i64,
    ) -> Result<()> {
        Ok(())
    }
    async fn index_items(&self, _items: &[ItemRow], _now: i64) -> Result<()> {
        Ok(())
    }
    async fn index_menu(
        &self,
        _restaurant: &RestaurantRow,
        _categories: &[CategoryRow],
        _items: &[ItemRow],
        _seen_from: Option<(f64, f64)>,
        _now: i64,
    ) -> Result<()> {
        Ok(())
    }
    async fn search_restaurants(&self, _q: &RestaurantQuery) -> Result<Vec<RestaurantHit>> {
        Ok(Vec::new())
    }
    async fn search_items(&self, _q: &ItemQuery) -> Result<Vec<ItemHit>> {
        Ok(Vec::new())
    }
    async fn restaurant(&self, _id: &str) -> Result<Option<RestaurantHit>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rest(id: &str, name: &str, cuisines: &[&str]) -> RestaurantRow {
        RestaurantRow {
            id: id.into(),
            namespace: "food".into(),
            name: name.into(),
            cuisines: cuisines.iter().map(ToString::to_string).collect(),
            ..RestaurantRow::default()
        }
    }

    #[tokio::test]
    async fn catalog_search_and_near() -> Result<()> {
        let c = MemoryCache::default();
        let mut a = rest("A", "Sisig Hooray", &["Filipino"]);
        a.distance_km = Some(1.5);
        c.index_restaurants(&[a], Some((14.55, 121.02)), 10).await?;
        let mut b = rest("B", "Ramen Bar", &["Japanese"]);
        b.lat = Some(1.30);
        b.lon = Some(103.8);
        c.index_restaurants(&[b], None, 10).await?;
        // Re-index without cuisines keeps the known ones.
        c.index_restaurants(&[rest("A", "Sisig Hooray", &[])], None, 20)
            .await?;

        let q = |text: &str, near: Option<Near>| RestaurantQuery {
            text: Some(text.into()),
            near,
            ..RestaurantQuery::default()
        };
        let hits = c.search_restaurants(&q("filipino", None)).await?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].row.id, "A");
        let near = Near {
            lat: 14.5501,
            lon: 121.0201,
            radius_km: 6.0,
        };
        let hits = c.search_restaurants(&q("sisig", Some(near))).await?;
        assert_eq!(hits[0].row.distance_km, Some(1.5));
        assert!(c.search_restaurants(&q("ramen", Some(near))).await?.is_empty());

        let item = |id: &str, price: i64| ItemRow {
            id: id.into(),
            restaurant_id: "A".into(),
            name: format!("Pork Sisig {id}"),
            price_minor: Some(price),
            ..ItemRow::default()
        };
        c.index_menu(
            &rest("A", "Sisig Hooray", &[]),
            &[],
            &[item("1", 15_000), item("2", 30_000)],
            None,
            30,
        )
        .await?;
        let items = c
            .search_items(&ItemQuery {
                text: Some("sisig".into()),
                max_price_minor: Some(20_000),
                ..ItemQuery::default()
            })
            .await?;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item.id, "1");
        assert!(items[0].restaurant.is_some());
        // A new menu drops items no longer on it.
        c.index_menu(&rest("A", "Sisig Hooray", &[]), &[], &[item("2", 30_000)], None, 40)
            .await?;
        let all = c.search_items(&ItemQuery::default()).await?;
        assert_eq!(all.len(), 1);
        Ok(())
    }
}
