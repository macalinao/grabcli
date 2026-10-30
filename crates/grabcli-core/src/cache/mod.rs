//! The read-only browse cache: the [`Cache`] storage trait plus the
//! [`CacheHandle`] every service call goes through.
//!
//! The library owns no storage. An embedder passes an `Arc<dyn Cache>` to
//! [`Context::new`](crate::Context::new): the CLI uses `SqliteCache` (crate
//! `grabcli-sqlite`, one SQLite database with FTS5 under the cache dir),
//! tests use [`MemoryCache`], and an embedder that wants no cache uses
//! [`NoCache`]. A cache stores three things:
//!
//! - **Responses**: the raw upstream JSON of read-only calls (search
//!   results, menus, store details, category lists), keyed by the SHA-256 of
//!   a canonical request identity ([`CacheKey`]) that includes the rounded
//!   delivery location. Raw responses (not parsed structs) are stored so
//!   struct changes never change what an entry means. TTLs, modes and
//!   freshness are applied by [`CacheHandle`], not by the backend.
//! - **Catalog**: normalized, queryable restaurants, where they were seen
//!   from, menu categories and menu items ([`types`]). Service crates upsert
//!   into it whenever they fetch list or menu data, and query it for offline
//!   (`--local`) search.
//! - **Images**: item images by URL ([`CachedImage`]), optional.
//!
//! The cache is best-effort: a backend error on a read is a miss and a
//! failed write is skipped. A broken cache never fails a browse command.
//!
//! The trait is async (`async_trait`) for the same reason as
//! [`SessionStore`](crate::store::SessionStore): the context is async and a
//! backend may be an async database client.
//!
//! # Safety: what may be cached
//!
//! Only the read-only kinds in [`CacheKind`] exist, and every cached call must
//! pick one. There is deliberately **no** kind for quotes, carts, vouchers,
//! auth/session/token responses, order submission, order history or tracking:
//! those must always be fetched live, and adding a kind for them is a safety
//! regression. The catalog likewise only holds restaurant, menu and item
//! data. Commands on the cart → quote → order path use [`CacheMode::Off`] (or
//! call the API directly) so prices and availability are always fresh before
//! money moves; with the mode off nothing is read from or written to any part
//! of the cache.
//!
//! # Usage from a service crate
//!
//! ```ignore
//! let key = CacheKey::new("food", CacheKind::Menu, "merchant")
//!     .param("id", merchant_id)
//!     .location(loc.latitude, loc.longitude);
//! let raw: serde_json::Value = ctx.cached_call(&key, &req).await?;
//! // with the entry's age (for "(cached 4m ago)" markers):
//! let hit = ctx.cached_call_meta(&key, &req).await?;
//! // or, for anything that isn't a single request:
//! let cache = ctx.cache().clone();
//! let raw: serde_json::Value = cache.get_or_fetch(&key, || ctx.call(&req)).await?;
//! ```

pub mod memory;
pub mod types;

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, ErrorKind, Result};
use crate::jwt;

pub use memory::{MemoryCache, NoCache};
pub use types::{
    CachedImage, CategoryRow, ItemHit, ItemQuery, ItemRow, Near, RestaurantHit, RestaurantQuery,
    RestaurantRow, effective_price, haversine_km, image_url_allowed, query_terms,
};

/// Environment variable the CLI reads to disable the cache (truthy value).
/// The library itself never reads the environment.
/// Environment variable that disables the cache when set to a truthy value.
pub const NO_CACHE_ENV: &str = "GRAB_NO_CACHE";

/// What is being cached. Only read-only browse data has a kind; see the module
/// docs before adding one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheKind {
    /// Restaurant/store lists and search results (default 10 min).
    List,
    /// Menus / store catalogues (default 30 min).
    Menu,
    /// Static restaurant/store details (default 6 h).
    Details,
    /// Cuisine/category lists (default 24 h).
    Categories,
}

impl CacheKind {
    pub const ALL: [CacheKind; 4] = [Self::List, Self::Menu, Self::Details, Self::Categories];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Menu => "menu",
            Self::Details => "details",
            Self::Categories => "categories",
        }
    }

    /// Default time-to-live in seconds.
    pub fn default_ttl(self) -> u64 {
        match self {
            Self::List => 10 * 60,
            Self::Menu => 30 * 60,
            Self::Details => 6 * 60 * 60,
            Self::Categories => 24 * 60 * 60,
        }
    }
}

impl std::fmt::Display for CacheKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for CacheKind {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let s = s.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| format!("unknown cache kind `{s}` (list|menu|details|categories)"))
    }
}

/// How a command uses the cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CacheMode {
    /// Serve fresh entries, store new responses.
    #[default]
    Normal,
    /// Skip reads, but store the fresh responses (`--refresh`).
    Refresh,
    /// No reads, no writes (`--no-cache`, `GRAB_NO_CACHE=1`, ordering paths).
    Off,
}

impl CacheMode {
    /// Resolve the mode from the global CLI flags. `--no-cache` wins over
    /// `--refresh`. (`GRAB_NO_CACHE` is applied by the CLI.)
    pub fn from_flags(no_cache: bool, refresh: bool) -> Self {
        if no_cache {
            Self::Off
        } else if refresh {
            Self::Refresh
        } else {
            Self::Normal
        }
    }

    pub fn reads(self) -> bool {
        self == Self::Normal
    }

    pub fn writes(self) -> bool {
        self != Self::Off
    }
}

/// Default image store cap (`[cache] image_max_mb`).
pub const DEFAULT_IMAGE_MAX_MB: u64 = 500;
/// Default age after which `grab cache prune` drops catalog rows.
pub const DEFAULT_INDEX_MAX_AGE_DAYS: u64 = 30;

/// `[cache]` in config.toml. Every key is optional.
///
/// ```toml
/// [cache]
/// enabled = true
/// image_max_mb = 500          # image store cap, LRU-evicted
/// index_max_age_days = 30     # `grab cache prune` drops older catalog rows
/// [cache.ttl]                 # seconds; 0 disables caching for that kind
/// list = 600
/// menu = 1800
/// details = 21600
/// categories = 86400
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    pub enabled: Option<bool>,
    pub ttl: CacheTtls,
    pub image_max_mb: Option<u64>,
    pub index_max_age_days: Option<u64>,
}

/// Per-kind TTL overrides in seconds.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheTtls {
    pub list: Option<u64>,
    pub menu: Option<u64>,
    pub details: Option<u64>,
    pub categories: Option<u64>,
}

impl CacheConfig {
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn ttl(&self, kind: CacheKind) -> u64 {
        let o = match kind {
            CacheKind::List => self.ttl.list,
            CacheKind::Menu => self.ttl.menu,
            CacheKind::Details => self.ttl.details,
            CacheKind::Categories => self.ttl.categories,
        };
        o.unwrap_or_else(|| kind.default_ttl())
    }

    pub fn image_max_bytes(&self) -> u64 {
        self.image_max_mb
            .unwrap_or(DEFAULT_IMAGE_MAX_MB)
            .saturating_mul(1024 * 1024)
    }

    pub fn index_max_age_secs(&self) -> u64 {
        self.index_max_age_days
            .unwrap_or(DEFAULT_INDEX_MAX_AGE_DAYS)
            .saturating_mul(24 * 60 * 60)
    }
}

/// The identity of a cached request: service namespace, kind, endpoint name
/// and normalized parameters. Two keys with the same identity share an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    namespace: String,
    kind: CacheKind,
    endpoint: String,
    params: BTreeMap<String, String>,
}

/// Decimal places kept for coordinates in keys (~11 m), so nearby repeat
/// lookups share entries while different places don't.
const COORD_DECIMALS: usize = 4;

fn normalize_value(v: &str) -> String {
    v.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn round_coord_to(c: f64, decimals: usize) -> String {
    let s = format!("{c:.decimals$}");
    // Avoid "-0.0000" vs "0.0000" splitting entries.
    if s.trim_start_matches('-')
        .chars()
        .all(|ch| ch == '0' || ch == '.')
    {
        s.trim_start_matches('-').to_string()
    } else {
        s
    }
}

fn round_coord(c: f64) -> String {
    round_coord_to(c, COORD_DECIMALS)
}

impl CacheKey {
    /// `namespace` is the service (`"food"`, `"mart"`); `endpoint` names the
    /// call (`"search"`, `"merchant"`), not the full URL.
    pub fn new(namespace: impl Into<String>, kind: CacheKind, endpoint: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            kind,
            endpoint: endpoint.into(),
            params: BTreeMap::new(),
        }
    }

    /// Add a parameter. Order doesn't matter; surrounding/repeated whitespace
    /// in the value is collapsed. Values are otherwise case-sensitive —
    /// lowercase them first if the upstream is case-insensitive.
    #[must_use]
    pub fn param(mut self, name: impl Into<String>, value: impl std::fmt::Display) -> Self {
        self.params
            .insert(name.into(), normalize_value(&value.to_string()));
        self
    }

    /// Add the delivery location, rounded to 4 decimals (~11 m).
    #[must_use]
    pub fn location(mut self, latitude: f64, longitude: f64) -> Self {
        self.params.insert("lat".into(), round_coord(latitude));
        self.params.insert("lng".into(), round_coord(longitude));
        self
    }

    pub fn kind(&self) -> CacheKind {
        self.kind
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Canonical identity string (JSON, params sorted by name).
    pub fn identity(&self) -> String {
        serde_json::json!({
            "ns": self.namespace,
            "kind": self.kind.as_str(),
            "endpoint": self.endpoint,
            "params": self.params,
        })
        .to_string()
    }

    /// Hex SHA-256 of [`identity`](Self::identity); the entry's primary key.
    pub fn hash(&self) -> String {
        hex_sha256(self.identity().as_bytes())
    }
}

pub fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A value plus where it came from: `cached_at` is the time the cached
/// response was fetched, or `None` if it was just fetched live.
#[derive(Debug, Clone, PartialEq)]
pub struct Fetched<T> {
    pub value: T,
    pub cached_at: Option<i64>,
}

impl<T> Fetched<T> {
    pub fn live(value: T) -> Self {
        Self {
            value,
            cached_at: None,
        }
    }

    pub fn is_cached(&self) -> bool {
        self.cached_at.is_some()
    }

    /// Seconds since the cached response was fetched (`None` if live).
    pub fn age_secs(&self) -> Option<i64> {
        self.cached_at.map(|t| (jwt::now() - t).max(0))
    }
}

/// `4m`, `2h`, `3d` — for "(cached 4m ago)" markers.
pub fn human_age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

pub fn is_fresh(fetched_at: i64, expires_at: i64, ttl_secs: u64, now: i64) -> bool {
    let ttl = i64::try_from(ttl_secs).unwrap_or(i64::MAX);
    // A timestamp from the future (clock skew) counts as stale. A TTL lowered
    // in config applies to entries written under a longer one.
    now >= fetched_at && now < expires_at && now - fetched_at < ttl
}


/// One stored response, as a backend keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    /// [`CacheKey::identity`] of the request; a mismatch on read is a miss.
    pub identity: String,
    /// The raw response, serialized JSON.
    pub body: String,
    pub fetched_at: i64,
    pub expires_at: i64,
}

/// Storage for the read-only browse cache. Object-safe: use it as
/// `Arc<dyn Cache>`. Backends store and return data; TTLs, modes and the
/// read-only safety rules are applied by [`CacheHandle`].
///
/// Responses are keyed by [`CacheKey::hash`] (namespace and kind are on the
/// key for housekeeping). Catalog upserts must not erase known fields with
/// unknowns (a row without cuisines keeps the stored ones). Images are
/// optional: the default methods store nothing.
#[async_trait]
pub trait Cache: Send + Sync {
    /// The stored entry for `key` (expired or not), if any.
    async fn get_entry(&self, key: &CacheKey) -> Result<Option<CacheEntry>>;
    /// Insert or replace the entry for `key`.
    async fn put_entry(&self, key: &CacheKey, entry: &CacheEntry) -> Result<()>;

    /// Upsert restaurants seen in a list response. `seen_from` is the
    /// delivery location of the request; when given, each row's distance /
    /// ETA / fee are also recorded for that location.
    async fn index_restaurants(
        &self,
        rows: &[RestaurantRow],
        seen_from: Option<(f64, f64)>,
        now: i64,
    ) -> Result<()>;
    /// Upsert individual items (e.g. dish hits), keeping known categories.
    async fn index_items(&self, items: &[ItemRow], now: i64) -> Result<()>;
    /// Store a full menu: the restaurant, its categories (replacing the old
    /// ones) and items; items no longer on the menu are removed.
    async fn index_menu(
        &self,
        restaurant: &RestaurantRow,
        categories: &[CategoryRow],
        items: &[ItemRow],
        seen_from: Option<(f64, f64)>,
        now: i64,
    ) -> Result<()>;
    /// Query restaurants in the catalog.
    async fn search_restaurants(&self, q: &RestaurantQuery) -> Result<Vec<RestaurantHit>>;
    /// Query menu items in the catalog.
    async fn search_items(&self, q: &ItemQuery) -> Result<Vec<ItemHit>>;
    /// One restaurant by id.
    async fn restaurant(&self, id: &str) -> Result<Option<RestaurantHit>>;

    /// The stored image for `url`, marking it recently used.
    async fn image(&self, _url: &str, _now: i64) -> Result<Option<CachedImage>> {
        Ok(None)
    }
    /// Store downloaded image bytes for `url`.
    async fn put_image(
        &self,
        _url: &str,
        _bytes: &[u8],
        _content_type: &str,
        _now: i64,
    ) -> Result<CachedImage> {
        Err(Error::invalid("this cache does not store images"))
    }
}

/// A [`Cache`] backend plus the active [`CacheMode`] and TTLs. Cheap to clone
/// (clones share the backend); clone it out of the
/// [`Context`](crate::Context) to call [`get_or_fetch`](Self::get_or_fetch)
/// with a closure that borrows the context.
#[derive(Clone)]
pub struct CacheHandle {
    backend: Arc<dyn Cache>,
    config: CacheConfig,
    mode: CacheMode,
}

impl std::fmt::Debug for CacheHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheHandle")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

/// Largest image we will download.
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

impl CacheHandle {
    /// `config.enabled == false` forces [`CacheMode::Off`].
    pub fn new(backend: Arc<dyn Cache>, config: CacheConfig, mode: CacheMode) -> Self {
        let mode = if config.enabled() {
            mode
        } else {
            CacheMode::Off
        };
        Self {
            backend,
            config,
            mode,
        }
    }

    /// The same backend with another mode; still off if disabled in config.
    #[must_use]
    pub fn with_mode(&self, mode: CacheMode) -> Self {
        Self::new(Arc::clone(&self.backend), self.config.clone(), mode)
    }

    pub fn mode(&self) -> CacheMode {
        self.mode
    }

    pub fn config(&self) -> &CacheConfig {
        &self.config
    }

    pub fn ttl(&self, kind: CacheKind) -> u64 {
        self.config.ttl(kind)
    }

    /// The backend, for housekeeping outside the mode rules.
    pub fn backend(&self) -> &Arc<dyn Cache> {
        &self.backend
    }

    /// A fresh cached value, if reads are enabled and one exists. Any problem
    /// reading or decoding the entry is a miss.
    pub async fn get<T: DeserializeOwned>(&self, key: &CacheKey) -> Option<T> {
        self.lookup(key).await.map(|f| f.value)
    }

    /// Like [`get`](Self::get), with the time the entry was fetched.
    pub async fn lookup<T: DeserializeOwned>(&self, key: &CacheKey) -> Option<Fetched<T>> {
        self.lookup_at(key, jwt::now()).await
    }

    #[doc(hidden)]
    pub async fn lookup_at<T: DeserializeOwned>(
        &self,
        key: &CacheKey,
        now: i64,
    ) -> Option<Fetched<T>> {
        if !self.mode.reads() {
            return None;
        }
        let (value, fetched_at, expires_at) = self.read_entry(key).await?;
        is_fresh(fetched_at, expires_at, self.ttl(key.kind()), now).then_some(Fetched {
            value,
            cached_at: Some(fetched_at),
        })
    }

    /// The stored value for `key` even if it has expired (but not yet been
    /// pruned), for falling back when the live call fails. `None` when the
    /// mode is off.
    pub async fn lookup_stale<T: DeserializeOwned>(&self, key: &CacheKey) -> Option<Fetched<T>> {
        if !self.mode.writes() {
            return None;
        }
        let (value, fetched_at, _) = self.read_entry(key).await?;
        Some(Fetched {
            value,
            cached_at: Some(fetched_at),
        })
    }

    async fn read_entry<T: DeserializeOwned>(&self, key: &CacheKey) -> Option<(T, i64, i64)> {
        let e = self.backend.get_entry(key).await.ok()??;
        // A hash collision or tampered row is a miss.
        if e.identity != key.identity() {
            return None;
        }
        let value = serde_json::from_str(&e.body).ok()?;
        Some((value, e.fetched_at, e.expires_at))
    }

    /// Store a value (no-op when writes are off or the kind's TTL is 0).
    pub async fn put<T: Serialize>(&self, key: &CacheKey, value: &T) -> Result<()> {
        self.put_at(key, value, jwt::now()).await
    }

    #[doc(hidden)]
    pub async fn put_at<T: Serialize>(&self, key: &CacheKey, value: &T, now: i64) -> Result<()> {
        let ttl = self.ttl(key.kind());
        if !self.mode.writes() || ttl == 0 {
            return Ok(());
        }
        let entry = CacheEntry {
            identity: key.identity(),
            body: serde_json::to_string(value)?,
            fetched_at: now,
            expires_at: now.saturating_add(i64::try_from(ttl).unwrap_or(i64::MAX)),
        };
        self.backend.put_entry(key, &entry).await
    }

    /// Return the cached value for `key`, or run `fetch`, store its result
    /// (best-effort) and return it. Errors from `fetch` are returned and never
    /// cached. Prefer `T = serde_json::Value` holding the raw upstream response.
    pub async fn get_or_fetch<T, F, Fut>(&self, key: &CacheKey, fetch: F) -> Result<T>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        Ok(self.get_or_fetch_meta(key, fetch).await?.value)
    }

    /// [`get_or_fetch`](Self::get_or_fetch) that also says whether the value
    /// came from the cache and how old it is.
    pub async fn get_or_fetch_meta<T, F, Fut>(&self, key: &CacheKey, fetch: F) -> Result<Fetched<T>>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        if let Some(hit) = self.lookup(key).await {
            return Ok(hit);
        }
        let value = fetch().await?;
        let _ = self.put(key, &value).await;
        Ok(Fetched::live(value))
    }

    /// Upsert restaurants seen in a list response (no-op when writes are off).
    pub async fn index_restaurants(
        &self,
        rows: &[RestaurantRow],
        seen_from: Option<(f64, f64)>,
    ) -> Result<()> {
        if !self.mode.writes() || rows.is_empty() {
            return Ok(());
        }
        self.backend
            .index_restaurants(rows, seen_from, jwt::now())
            .await
    }

    /// Upsert individual items (no-op when writes are off).
    pub async fn index_items(&self, items: &[ItemRow]) -> Result<()> {
        if !self.mode.writes() || items.is_empty() {
            return Ok(());
        }
        self.backend.index_items(items, jwt::now()).await
    }

    /// Store a full menu (no-op when writes are off).
    pub async fn index_menu(
        &self,
        restaurant: &RestaurantRow,
        categories: &[CategoryRow],
        items: &[ItemRow],
        seen_from: Option<(f64, f64)>,
    ) -> Result<()> {
        if !self.mode.writes() || restaurant.id.is_empty() {
            return Ok(());
        }
        self.backend
            .index_menu(restaurant, categories, items, seen_from, jwt::now())
            .await
    }

    /// Query restaurants in the catalog (works in every mode: it only reads
    /// what earlier fetches stored).
    pub async fn search_restaurants(&self, q: &RestaurantQuery) -> Result<Vec<RestaurantHit>> {
        self.backend.search_restaurants(q).await
    }

    /// Query menu items in the catalog.
    pub async fn search_items(&self, q: &ItemQuery) -> Result<Vec<ItemHit>> {
        self.backend.search_items(q).await
    }

    /// One restaurant by id.
    pub async fn restaurant(&self, id: &str) -> Result<Option<RestaurantHit>> {
        self.backend.restaurant(id).await
    }

    /// The cached image for `url`, if present. `None` when the mode is off.
    pub async fn cached_image(&self, url: &str) -> Option<CachedImage> {
        if !self.mode.reads() && !self.mode.writes() {
            return None;
        }
        self.backend.image(url, jwt::now()).await.ok().flatten()
    }

    /// Store downloaded image bytes (refused when writes are off).
    pub async fn store_image(
        &self,
        url: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<CachedImage> {
        if !self.mode.writes() {
            return Err(Error::invalid("the cache is off; images are not stored"));
        }
        self.backend
            .put_image(url, bytes, content_type, jwt::now())
            .await
    }

    /// The cached image for `url`, downloading it first if needed. Only Grab
    /// CDN URLs are fetched ([`image_url_allowed`]); no cookies are sent.
    pub async fn fetch_image(&self, url: &str) -> Result<CachedImage> {
        if !image_url_allowed(url) {
            return Err(Error::new(
                ErrorKind::UrlNotAllowed,
                "only https image URLs on grab.com hosts can be cached",
            ));
        }
        if let Some(hit) = self.cached_image(url).await {
            return Ok(hit);
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let resp = client.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::new(
                if status.as_u16() == 404 {
                    ErrorKind::NotFound
                } else {
                    ErrorKind::Api
                },
                format!("{} fetching image", status.as_u16()),
            ));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !content_type.to_ascii_lowercase().starts_with("image/") {
            return Err(Error::new(
                ErrorKind::Api,
                format!("not an image (content-type `{content_type}`)"),
            ));
        }
        if resp
            .content_length()
            .is_some_and(|l| l > MAX_IMAGE_BYTES as u64)
        {
            return Err(Error::invalid("image is too large"));
        }
        let bytes = resp.bytes().await?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(Error::invalid("image is too large"));
        }
        self.store_image(url, &bytes, &content_type).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn handle(mode: CacheMode) -> (Arc<MemoryCache>, CacheHandle) {
        let mem = Arc::new(MemoryCache::default());
        let h = CacheHandle::new(mem.clone(), CacheConfig::default(), mode);
        (mem, h)
    }

    fn key() -> CacheKey {
        CacheKey::new("food", CacheKind::Menu, "merchant")
            .param("id", "2-EXAMPLE")
            .location(1.23, 103.81)
    }

    async fn get_at(c: &CacheHandle, k: &CacheKey, now: i64) -> Option<Value> {
        c.lookup_at::<Value>(k, now).await.map(|f| f.value)
    }

    #[test]
    fn key_normalization() {
        let a = CacheKey::new("food", CacheKind::List, "search")
            .param("keyword", "  fried   chicken ")
            .param("page", 0)
            .location(1.234_512_3, 103.812_34);
        let b = CacheKey::new("food", CacheKind::List, "search")
            .location(1.234_499, 103.812_251)
            .param("page", "0")
            .param("keyword", "fried chicken");
        assert_eq!(a.identity(), b.identity());
        assert_eq!(a.hash(), b.hash());
        assert_eq!(a.hash().len(), 64);

        // A different place (> ~11 m), kind or namespace is a different key.
        let far = b.clone().location(1.2358, 103.8123);
        assert_ne!(far.hash(), b.hash());
        let mart = CacheKey::new("mart", CacheKind::List, "search")
            .param("keyword", "fried chicken")
            .param("page", 0)
            .location(1.2345, 103.8123);
        assert_ne!(mart.hash(), a.hash());
        // -0.0 and 0.0 are the same place.
        assert_eq!(round_coord(-0.000_01), round_coord(0.0));
    }

    #[tokio::test]
    async fn roundtrip_and_ttl_expiry() -> Result<()> {
        let (_, c) = handle(CacheMode::Normal);
        let k = key();
        let v = json!({"merchant": {"ID": "2-EXAMPLE"}});
        c.put_at(&k, &v, 1_000).await?;
        assert_eq!(get_at(&c, &k, 1_000).await, Some(v.clone()));
        let ttl = CacheKind::Menu.default_ttl() as i64;
        let hit = c.lookup_at::<Value>(&k, 1_000 + ttl - 1).await;
        assert_eq!(hit.map(|h| h.cached_at), Some(Some(1_000)));
        assert_eq!(get_at(&c, &k, 1_000 + ttl).await, None);
        // Clock skew: an entry from the future is stale.
        assert_eq!(get_at(&c, &k, 999).await, None);
        // Expired but not pruned: still available as a stale fallback.
        assert!(c.lookup_stale::<Value>(&k).await.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn config_ttl_override_and_disable() -> Result<()> {
        let mem = Arc::new(MemoryCache::default());
        let cfg: CacheConfig = toml::from_str("[ttl]\nmenu = 5\nlist = 0\n")
            .map_err(|e| Error::invalid(e.to_string()))?;
        let c = CacheHandle::new(mem.clone(), cfg, CacheMode::Normal);
        let k = key();
        c.put_at(&k, &json!(1), 100).await?;
        assert_eq!(get_at(&c, &k, 104).await, Some(json!(1)));
        assert_eq!(get_at(&c, &k, 105).await, None);
        // TTL 0: never written.
        let lk = CacheKey::new("food", CacheKind::List, "search");
        c.put_at(&lk, &json!(1), 100).await?;
        assert!(mem.get_entry(&lk).await?.is_none());

        // A lowered TTL applies to entries written under a longer one.
        let long = CacheHandle::new(mem.clone(), CacheConfig::default(), CacheMode::Normal);
        long.put_at(&k, &json!(2), 100).await?;
        assert_eq!(get_at(&c, &k, 110).await, None);
        assert_eq!(get_at(&long, &k, 110).await, Some(json!(2)));

        let off: CacheConfig =
            toml::from_str("enabled = false").map_err(|e| Error::invalid(e.to_string()))?;
        let h = CacheHandle::new(mem, off, CacheMode::Normal);
        assert_eq!(h.mode(), CacheMode::Off);
        assert_eq!(h.with_mode(CacheMode::Refresh).mode(), CacheMode::Off);
        Ok(())
    }

    #[tokio::test]
    async fn bad_rows_are_misses() -> Result<()> {
        let (mem, c) = handle(CacheMode::Normal);
        let k = key();
        // Wrong payload type for T: a miss, not an error.
        c.put(&k, &json!("a string")).await?;
        assert_eq!(c.get::<Vec<u32>>(&k).await, None);
        // A hash collision / tampered identity is a miss.
        let mut e = mem.get_entry(&k).await?.ok_or_else(|| Error::invalid("none"))?;
        e.identity = "something else".into();
        mem.put_entry(&k, &e).await?;
        assert_eq!(c.get::<Value>(&k).await, None);
        // Unparseable body: a miss.
        c.put(&k, &json!(1)).await?;
        let mut e = mem.get_entry(&k).await?.ok_or_else(|| Error::invalid("none"))?;
        e.body = "{nope".into();
        mem.put_entry(&k, &e).await?;
        assert_eq!(c.get::<Value>(&k).await, None);
        Ok(())
    }

    #[tokio::test]
    async fn modes() -> Result<()> {
        let k = key();
        let (_, normal) = handle(CacheMode::Normal);

        // Normal: miss → fetch + store; then hit without fetching.
        let v = normal
            .get_or_fetch_meta(&k, || async { Ok(json!(1)) })
            .await?;
        assert_eq!((v.value, v.cached_at), (json!(1), None));
        let v = normal
            .get_or_fetch_meta::<Value, _, _>(&k, || async {
                Err(Error::invalid("must not fetch"))
            })
            .await?;
        assert_eq!(v.value, json!(1));
        assert!(v.is_cached());
        assert!(v.age_secs().is_some_and(|a| a >= 0));

        // Off: always fetches, never writes, no stale fallback.
        let off = normal.with_mode(CacheMode::Off);
        let v: Value = off.get_or_fetch(&k, || async { Ok(json!(2)) }).await?;
        assert_eq!(v, json!(2));
        assert_eq!(normal.get::<Value>(&k).await, Some(json!(1)));
        assert!(off.lookup_stale::<Value>(&k).await.is_none());
        assert!(off.store_image("https://food-cms.grab.com/x.png", b"x", "image/png").await.is_err());

        // Refresh: always fetches, overwrites the entry.
        let refresh = normal.with_mode(CacheMode::Refresh);
        let v: Value = refresh.get_or_fetch(&k, || async { Ok(json!(3)) }).await?;
        assert_eq!(v, json!(3));
        assert_eq!(normal.get::<Value>(&k).await, Some(json!(3)));

        // Errors propagate and are not cached.
        let k2 = key().param("id", "other");
        let r: Result<Value> = normal
            .get_or_fetch(&k2, || async { Err(Error::invalid("boom")) })
            .await;
        assert!(r.is_err());
        assert_eq!(normal.get::<Value>(&k2).await, None);

        assert_eq!(CacheMode::from_flags(true, true), CacheMode::Off);
        assert_eq!(CacheMode::from_flags(false, true), CacheMode::Refresh);
        assert_eq!(CacheMode::from_flags(false, false), CacheMode::Normal);
        Ok(())
    }

    #[tokio::test]
    async fn off_mode_skips_catalog_writes() -> Result<()> {
        let (mem, normal) = handle(CacheMode::Normal);
        let off = normal.with_mode(CacheMode::Off);
        let row = RestaurantRow {
            id: "R1".into(),
            namespace: "food".into(),
            name: "Sisig Place".into(),
            ..RestaurantRow::default()
        };
        off.index_restaurants(std::slice::from_ref(&row), None).await?;
        assert!(mem.restaurant("R1").await?.is_none());
        normal.index_restaurants(&[row], None).await?;
        assert!(off.restaurant("R1").await?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn no_cache_never_hits() -> Result<()> {
        let h = CacheHandle::new(Arc::new(NoCache), CacheConfig::default(), CacheMode::Normal);
        h.put(&key(), &json!(1)).await?;
        assert_eq!(h.get::<Value>(&key()).await, None);
        Ok(())
    }

    #[test]
    fn kind_parses() {
        assert_eq!("Menu".parse::<CacheKind>(), Ok(CacheKind::Menu));
        assert!("quote".parse::<CacheKind>().is_err());
    }

    #[test]
    fn ages() {
        assert_eq!(human_age(5), "5s");
        assert_eq!(human_age(240), "4m");
        assert_eq!(human_age(7200), "2h");
        assert_eq!(human_age(3 * 86_400), "3d");
    }
}
