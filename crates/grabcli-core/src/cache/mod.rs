//! Local cache for read-only browse data, in one SQLite database at
//! `<cache dir>/cache.sqlite` (WAL, mode 0600, inside a 0700 directory).
//!
//! It has three parts:
//!
//! - **Responses** (`entries`): the raw upstream JSON of read-only calls
//!   (search results, menus, store details, category lists), keyed by the
//!   SHA-256 of a canonical request identity that includes the rounded
//!   delivery location, with a per-kind TTL. Raw responses (not parsed
//!   structs) are stored so struct changes never change what an entry means.
//! - **Catalog** ([`catalog`]): normalized, queryable restaurants, the
//!   locations they were seen from, menu categories and menu items, with FTS5
//!   word + trigram indexes. Service crates upsert into it whenever they fetch
//!   list or menu data, and query it for offline (`--local`) search.
//! - **Images** ([`images`]): content-addressed image files under
//!   `<cache dir>/images/`, indexed in SQLite and evicted LRU by total size.
//!
//! The cache is best-effort: if the database can't be opened or a row can't
//! be decoded, reads miss and writes are skipped. A broken cache never fails a
//! browse command.
//!
//! # Safety: what may be cached
//!
//! Only the read-only kinds in [`CacheKind`] exist, and every cached call must
//! pick one. There is deliberately **no** kind for quotes, carts, vouchers,
//! auth/session/token responses, order submission, order history or tracking:
//! those must always be fetched live, and adding a kind for them is a safety
//! regression. The catalog tables likewise only hold restaurant, menu and item
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

pub mod catalog;
pub(crate) mod db;
pub mod images;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use rusqlite::{Connection, OptionalExtension, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::{jwt, paths};

pub use catalog::{
    CategoryRow, ItemHit, ItemQuery, ItemRow, Near, RestaurantHit, RestaurantQuery, RestaurantRow,
};
pub use images::{CachedImage, image_url_allowed};

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
    /// `--refresh`. (`GRAB_NO_CACHE` is applied by [`Cache::from_config`].)
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

fn round_coord_to(c: f64, decimals: usize) -> String {
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

pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
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

fn is_fresh(fetched_at: i64, expires_at: i64, ttl_secs: u64, now: i64) -> bool {
    let ttl = i64::try_from(ttl_secs).unwrap_or(i64::MAX);
    // A timestamp from the future (clock skew) counts as stale. A TTL lowered
    // in config applies to entries written under a longer one.
    now >= fetched_at && now < expires_at && now - fetched_at < ttl
}

/// Per-kind numbers in [`CacheStats`].
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct KindStats {
    pub entries: u64,
    pub bytes: u64,
    pub expired: u64,
}

/// Image store numbers in [`CacheStats`].
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ImageStats {
    /// Distinct files (several URLs may share one).
    pub files: u64,
    pub urls: u64,
    pub bytes: u64,
    pub max_bytes: u64,
}

/// `grab cache stats`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CacheStats {
    /// The cache directory.
    pub path: String,
    pub db_path: String,
    /// Database + WAL size on disk.
    pub db_bytes: u64,
    pub schema_version: u64,
    pub enabled: bool,
    pub entries: u64,
    /// Total size of stored response bodies.
    pub total_bytes: u64,
    pub expired: u64,
    pub kinds: BTreeMap<String, KindStats>,
    /// Row counts of the catalog tables.
    pub tables: BTreeMap<String, u64>,
    pub images: ImageStats,
}

/// What `grab cache prune` removed.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct PruneReport {
    pub entries: u64,
    pub restaurants: u64,
    pub restaurant_locations: u64,
    pub menu_categories: u64,
    pub menu_items: u64,
    pub images_evicted: u64,
    pub image_bytes_freed: u64,
}

/// What `grab cache clear --all` removed.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ClearReport {
    pub entries: u64,
    pub catalog_rows: u64,
    pub images: u64,
}

/// The lazily opened database, shared by clones of a [`Cache`].
#[derive(Default)]
struct DbCell {
    conn: OnceLock<std::result::Result<Mutex<Connection>, String>>,
}

impl std::fmt::Debug for DbCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self.conn.get() {
            None => "unopened",
            Some(Ok(_)) => "open",
            Some(Err(_)) => "failed",
        };
        f.debug_struct("DbCell").field("state", &state).finish()
    }
}

/// A handle on the cache database plus the active mode and TTLs. Cheap to
/// clone (clones share one connection); clone it out of the
/// [`Context`](crate::Context) to use [`get_or_fetch`](Self::get_or_fetch)
/// with a closure that borrows the context. The database is opened on first
/// use.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
    config: CacheConfig,
    mode: CacheMode,
    db: Arc<DbCell>,
}

fn env_disables_cache() -> bool {
    std::env::var(NO_CACHE_ENV).is_ok_and(|v| {
        let v = v.trim().to_ascii_lowercase();
        !(v.is_empty() || v == "0" || v == "false" || v == "no")
    })
}

impl Cache {
    /// A cache rooted at `root` (the database is `root/cache.sqlite`).
    /// `config.enabled == false` forces [`CacheMode::Off`].
    pub fn new(root: impl Into<PathBuf>, config: CacheConfig, mode: CacheMode) -> Self {
        let mode = if config.enabled() {
            mode
        } else {
            CacheMode::Off
        };
        Self {
            root: root.into(),
            config,
            mode,
            db: Arc::new(DbCell::default()),
        }
    }

    /// The production cache: [`paths::cache_dir`], the `[cache]` config, and
    /// `GRAB_NO_CACHE=1` forcing it off. If the cache dir can't be resolved the
    /// cache is simply off.
    pub fn from_config(config: &CacheConfig, mode: CacheMode) -> Self {
        let mode = if env_disables_cache() {
            CacheMode::Off
        } else {
            mode
        };
        match paths::cache_dir() {
            Ok(root) => Self::new(root, config.clone(), mode),
            Err(_) => Self::new(PathBuf::new(), config.clone(), CacheMode::Off),
        }
    }

    /// The same cache (sharing its connection) with another mode; still off if
    /// disabled in config.
    #[must_use]
    pub fn with_mode(&self, mode: CacheMode) -> Self {
        Self {
            root: self.root.clone(),
            config: self.config.clone(),
            mode: if self.config.enabled() {
                mode
            } else {
                CacheMode::Off
            },
            db: Arc::clone(&self.db),
        }
    }

    pub fn mode(&self) -> CacheMode {
        self.mode
    }

    /// The cache directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db_path(&self) -> PathBuf {
        self.root.join(db::DB_FILE)
    }

    pub fn config(&self) -> &CacheConfig {
        &self.config
    }

    pub fn ttl(&self, kind: CacheKind) -> u64 {
        self.config.ttl(kind)
    }

    /// The open connection, opening it on first use. `Err` carries why the
    /// database is unavailable.
    fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        if self.root.as_os_str().is_empty() {
            return Err(Error::invalid("no cache directory"));
        }
        let cell = self.db.conn.get_or_init(|| {
            db::open(&self.db_path(), jwt::now())
                .map(Mutex::new)
                .map_err(|e| e.message)
        });
        match cell {
            Ok(m) => Ok(m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)),
            Err(msg) => Err(Error::new(crate::error::ErrorKind::Io, msg.clone())),
        }
    }

    /// A fresh cached value, if reads are enabled and one exists. Any problem
    /// reading or decoding the entry is a miss.
    pub fn get<T: DeserializeOwned>(&self, key: &CacheKey) -> Option<T> {
        self.lookup(key).map(|f| f.value)
    }

    /// Like [`get`](Self::get), with the time the entry was fetched.
    pub fn lookup<T: DeserializeOwned>(&self, key: &CacheKey) -> Option<Fetched<T>> {
        self.lookup_at(key, jwt::now())
    }

    fn lookup_at<T: DeserializeOwned>(&self, key: &CacheKey, now: i64) -> Option<Fetched<T>> {
        if !self.mode.reads() {
            return None;
        }
        let (value, fetched_at, expires_at) = self.read_entry(key)?;
        is_fresh(fetched_at, expires_at, self.ttl(key.kind()), now).then_some(Fetched {
            value,
            cached_at: Some(fetched_at),
        })
    }

    /// The stored value for `key` even if it has expired (but not yet been
    /// pruned), for falling back when the live call fails. `None` when the
    /// mode is off.
    pub fn lookup_stale<T: DeserializeOwned>(&self, key: &CacheKey) -> Option<Fetched<T>> {
        if !self.mode.writes() {
            return None;
        }
        let (value, fetched_at, _) = self.read_entry(key)?;
        Some(Fetched {
            value,
            cached_at: Some(fetched_at),
        })
    }

    fn read_entry<T: DeserializeOwned>(&self, key: &CacheKey) -> Option<(T, i64, i64)> {
        let conn = self.conn().ok()?;
        let row: (String, String, i64, i64) = conn
            .query_row(
                "SELECT identity, body, fetched_at, expires_at FROM entries WHERE key = ?1",
                [key.hash()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .ok()??;
        drop(conn);
        let (identity, body, fetched_at, expires_at) = row;
        // A hash collision or tampered row is a miss.
        if identity != key.identity() {
            return None;
        }
        let value = serde_json::from_str(&body).ok()?;
        Some((value, fetched_at, expires_at))
    }

    /// Store a value (no-op when writes are off or the kind's TTL is 0).
    pub fn put<T: Serialize>(&self, key: &CacheKey, value: &T) -> Result<()> {
        self.put_at(key, value, jwt::now())
    }

    fn put_at<T: Serialize>(&self, key: &CacheKey, value: &T, now: i64) -> Result<()> {
        let ttl = self.ttl(key.kind());
        if !self.mode.writes() || ttl == 0 {
            return Ok(());
        }
        let body = serde_json::to_string(value)?;
        let expires_at = now.saturating_add(i64::try_from(ttl).unwrap_or(i64::MAX));
        let conn = self.conn()?;
        conn.execute(
            "INSERT OR REPLACE INTO entries \
             (key, namespace, kind, identity, body, fetched_at, expires_at, etag) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
            params![
                key.hash(),
                key.namespace(),
                key.kind().as_str(),
                key.identity(),
                body,
                now,
                expires_at
            ],
        )?;
        Ok(())
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
        if let Some(hit) = self.lookup(key) {
            return Ok(hit);
        }
        let value = fetch().await?;
        let _ = self.put(key, &value);
        Ok(Fetched::live(value))
    }

    pub fn stats(&self) -> Result<CacheStats> {
        self.stats_at(jwt::now())
    }

    fn stats_at(&self, now: i64) -> Result<CacheStats> {
        let mut s = CacheStats {
            path: self.root.display().to_string(),
            db_path: self.db_path().display().to_string(),
            enabled: self.config.enabled(),
            ..CacheStats::default()
        };
        for kind in CacheKind::ALL {
            s.kinds.insert(kind.as_str().into(), KindStats::default());
        }
        {
            let conn = self.conn()?;
            s.schema_version = u64::try_from(db::schema_version(&conn)?).unwrap_or(0);
            let mut stmt = conn.prepare(
                "SELECT kind, fetched_at, expires_at, length(CAST(body AS BLOB)) FROM entries",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?;
            for row in rows {
                let (kind, fetched_at, expires_at, bytes) = row?;
                let bytes = u64::try_from(bytes).unwrap_or(0);
                let ttl = kind.parse::<CacheKind>().map(|k| self.ttl(k)).unwrap_or(0);
                let k = s.kinds.entry(kind).or_default();
                k.entries += 1;
                k.bytes += bytes;
                s.entries += 1;
                s.total_bytes += bytes;
                if !is_fresh(fetched_at, expires_at, ttl, now) {
                    k.expired += 1;
                    s.expired += 1;
                }
            }
            drop(stmt);
            for table in catalog::TABLES {
                let n: i64 =
                    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
                s.tables
                    .insert(table.to_string(), u64::try_from(n).unwrap_or(0));
            }
            s.images = images::stats(&conn, self.config.image_max_bytes())?;
        }
        s.db_bytes = db::db_size(&self.db_path());
        Ok(s)
    }

    /// Remove response entries, optionally only of one kind and/or only
    /// expired ones. Returns how many were removed. Catalog rows and images are
    /// kept (see [`clear_all`](Self::clear_all) / [`prune`](Self::prune)).
    pub fn clear(&self, kind: Option<CacheKind>, expired_only: bool) -> Result<u64> {
        self.clear_at(kind, expired_only, jwt::now())
    }

    fn clear_at(&self, kind: Option<CacheKind>, expired_only: bool, now: i64) -> Result<u64> {
        let conn = self.conn()?;
        let mut removed = 0;
        let kinds: Vec<CacheKind> = kind.map_or_else(|| CacheKind::ALL.to_vec(), |k| vec![k]);
        for k in kinds {
            let n = if expired_only {
                let ttl = i64::try_from(self.ttl(k)).unwrap_or(i64::MAX);
                conn.execute(
                    "DELETE FROM entries WHERE kind = ?1 AND \
                     (expires_at <= ?2 OR fetched_at > ?2 OR ?2 - fetched_at >= ?3)",
                    params![k.as_str(), now, ttl],
                )?
            } else {
                conn.execute("DELETE FROM entries WHERE kind = ?1", [k.as_str()])?
            };
            removed += n as u64;
        }
        if kind.is_none() && !expired_only {
            // Rows of kinds this binary doesn't know (written by another
            // version) go too.
            removed += conn.execute("DELETE FROM entries", [])? as u64;
        }
        Ok(removed)
    }

    /// Remove everything: responses, catalog rows and image files.
    pub fn clear_all(&self) -> Result<ClearReport> {
        let mut report = ClearReport::default();
        {
            let conn = self.conn()?;
            report.entries = conn.execute("DELETE FROM entries", [])? as u64;
            for table in catalog::TABLES {
                report.catalog_rows += conn.execute(&format!("DELETE FROM {table}"), [])? as u64;
            }
            report.images = images::clear(&conn, &self.root)?;
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        }
        Ok(report)
    }

    /// Housekeeping: drop expired responses, catalog rows older than
    /// `index_max_age_days`, and evict images over `image_max_mb` (LRU), then
    /// compact the database.
    pub fn prune(&self) -> Result<PruneReport> {
        self.prune_at(jwt::now())
    }

    fn prune_at(&self, now: i64) -> Result<PruneReport> {
        let mut report = PruneReport {
            entries: self.clear_at(None, true, now)?,
            ..PruneReport::default()
        };
        let conn = self.conn()?;
        // Entries of unknown kinds past their stored expiry.
        report.entries += conn.execute("DELETE FROM entries WHERE expires_at <= ?1", [now])? as u64;
        let cutoff =
            now.saturating_sub(i64::try_from(self.config.index_max_age_secs()).unwrap_or(i64::MAX));
        let c = catalog::prune_older_than(&conn, cutoff)?;
        report.restaurants = c[0];
        report.restaurant_locations = c[1];
        report.menu_categories = c[2];
        report.menu_items = c[3];
        let (n, bytes) = images::evict(&conn, &self.root, self.config.image_max_bytes())?;
        report.images_evicted = n;
        report.image_bytes_freed = bytes;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM; PRAGMA optimize;")?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn cache(dir: &Path, mode: CacheMode) -> Cache {
        Cache::new(dir, CacheConfig::default(), mode)
    }

    fn key() -> CacheKey {
        CacheKey::new("food", CacheKind::Menu, "merchant")
            .param("id", "2-EXAMPLE")
            .location(1.23, 103.81)
    }

    fn get_at(c: &Cache, k: &CacheKey, now: i64) -> Option<Value> {
        c.lookup_at::<Value>(k, now).map(|f| f.value)
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

    #[test]
    fn roundtrip_and_ttl_expiry() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let c = cache(dir.path(), CacheMode::Normal);
        let k = key();
        let v = json!({"merchant": {"ID": "2-EXAMPLE"}});
        c.put_at(&k, &v, 1_000)?;
        assert_eq!(get_at(&c, &k, 1_000), Some(v.clone()));
        let ttl = CacheKind::Menu.default_ttl() as i64;
        let hit = c.lookup_at::<Value>(&k, 1_000 + ttl - 1);
        assert_eq!(hit.map(|h| h.cached_at), Some(Some(1_000)));
        assert_eq!(get_at(&c, &k, 1_000 + ttl), None);
        // Clock skew: an entry from the future is stale.
        assert_eq!(get_at(&c, &k, 999), None);
        // Expired but not pruned: still available as a stale fallback.
        assert!(c.lookup_stale::<Value>(&k).is_some());
        Ok(())
    }

    #[test]
    fn config_ttl_override_and_disable() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let cfg: CacheConfig = toml::from_str("[ttl]\nmenu = 5\nlist = 0\n")
            .map_err(|e| Error::invalid(e.to_string()))?;
        let c = Cache::new(dir.path(), cfg, CacheMode::Normal);
        let k = key();
        c.put_at(&k, &json!(1), 100)?;
        assert_eq!(get_at(&c, &k, 104), Some(json!(1)));
        assert_eq!(get_at(&c, &k, 105), None);
        // TTL 0: never written.
        let lk = CacheKey::new("food", CacheKind::List, "search");
        c.put_at(&lk, &json!(1), 100)?;
        assert_eq!(c.stats_at(100)?.kinds["list"].entries, 0);

        // A lowered TTL applies to entries written under a longer one.
        let long = cache(dir.path(), CacheMode::Normal);
        long.put_at(&k, &json!(2), 100)?;
        assert_eq!(get_at(&c, &k, 110), None);
        assert_eq!(get_at(&long, &k, 110), Some(json!(2)));

        let off: CacheConfig =
            toml::from_str("enabled = false").map_err(|e| Error::invalid(e.to_string()))?;
        assert_eq!(
            Cache::new(dir.path(), off.clone(), CacheMode::Normal).mode(),
            CacheMode::Off
        );
        assert_eq!(
            Cache::new(dir.path(), off, CacheMode::Normal)
                .with_mode(CacheMode::Refresh)
                .mode(),
            CacheMode::Off
        );
        Ok(())
    }

    #[test]
    fn bad_rows_are_misses() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let c = cache(dir.path(), CacheMode::Normal);
        let k = key();
        // Wrong payload type for T: a miss, not an error.
        c.put(&k, &json!("a string"))?;
        assert_eq!(c.get::<Vec<u32>>(&k), None);
        // A hash collision / tampered identity is a miss.
        c.conn()?
            .execute("UPDATE entries SET identity = 'something else'", [])?;
        assert_eq!(c.get::<Value>(&k), None);
        // Unparseable body: a miss.
        c.put(&k, &json!(1))?;
        c.conn()?.execute("UPDATE entries SET body = '{nope'", [])?;
        assert_eq!(c.get::<Value>(&k), None);

        // An unusable cache dir: reads miss, get_or_fetch still works.
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x")?;
        let broken = cache(&file, CacheMode::Normal);
        assert_eq!(broken.get::<Value>(&k), None);
        assert!(broken.put(&k, &json!(1)).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn modes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let k = key();
        let normal = cache(dir.path(), CacheMode::Normal);

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
        assert_eq!(normal.get::<Value>(&k), Some(json!(1)));
        assert!(off.lookup_stale::<Value>(&k).is_none());

        // Refresh: always fetches, overwrites the entry.
        let refresh = normal.with_mode(CacheMode::Refresh);
        let v: Value = refresh.get_or_fetch(&k, || async { Ok(json!(3)) }).await?;
        assert_eq!(v, json!(3));
        assert_eq!(normal.get::<Value>(&k), Some(json!(3)));

        // Errors propagate and are not cached.
        let k2 = key().param("id", "other");
        let r: Result<Value> = normal
            .get_or_fetch(&k2, || async { Err(Error::invalid("boom")) })
            .await;
        assert!(r.is_err());
        assert_eq!(normal.get::<Value>(&k2), None);

        assert_eq!(CacheMode::from_flags(true, true), CacheMode::Off);
        assert_eq!(CacheMode::from_flags(false, true), CacheMode::Refresh);
        assert_eq!(CacheMode::from_flags(false, false), CacheMode::Normal);
        Ok(())
    }

    #[test]
    fn stats_clear_and_prune() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let c = cache(dir.path(), CacheMode::Normal);
        let menu = key();
        let list = CacheKey::new("food", CacheKind::List, "search").param("keyword", "x");
        let mart = CacheKey::new("mart", CacheKind::Categories, "categories");
        c.put_at(&menu, &json!({"a": 1}), 0)?; // expired by `now` below
        c.put_at(&list, &json!([1, 2]), 10_000)?;
        c.put_at(&mart, &json!([]), 10_000)?;
        let now = 10_001;

        let s = c.stats_at(now)?;
        assert_eq!(s.entries, 3);
        assert_eq!(s.expired, 1);
        assert!(s.total_bytes > 0);
        assert!(s.db_bytes > 0);
        assert_eq!(s.schema_version, db::MIGRATIONS.len() as u64);
        assert_eq!(s.kinds["menu"].expired, 1);
        assert_eq!(s.kinds["list"].entries, 1);
        assert_eq!(s.kinds["categories"].entries, 1);
        assert_eq!(s.kinds["details"].entries, 0);
        assert_eq!(s.tables["restaurants"], 0);

        assert_eq!(c.clear_at(None, true, now)?, 1);
        assert_eq!(c.stats_at(now)?.entries, 2);
        assert_eq!(c.clear_at(Some(CacheKind::List), false, now)?, 1);
        assert_eq!(get_at(&c, &list, now), None);
        assert_eq!(get_at(&c, &mart, now), Some(json!([])));

        // Prune removes what has expired by then.
        let later = 10_000 + CacheKind::Categories.default_ttl() as i64;
        let r = c.prune_at(later)?;
        assert_eq!(r.entries, 1);
        assert_eq!(c.stats_at(later)?.entries, 0);

        // Expired rows are also pruned (boundedly) when the db is opened.
        c.put_at(&menu, &json!(1), 0)?;
        let reopened = cache(dir.path(), CacheMode::Normal);
        assert_eq!(reopened.stats()?.entries, 0);
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn private_permissions() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("grabcli");
        let c = cache(&root, CacheMode::Normal);
        c.put(&key(), &json!(1))?;
        assert_eq!(
            std::fs::metadata(c.db_path())?.permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&root)?.permissions().mode() & 0o777,
            0o700
        );
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
