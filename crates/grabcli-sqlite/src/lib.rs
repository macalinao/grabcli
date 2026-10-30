//! SQLite cache backend for grabcli: [`SqliteCache`] implements
//! [`grabcli_core::cache::Cache`] with one database at
//! `<root>/cache.sqlite` (WAL, mode 0600, inside a 0700 directory):
//!
//! - **Responses** (`entries`): raw upstream JSON by key hash.
//! - **Catalog** ([`catalog`]): restaurants, where they were seen from, menu
//!   categories and items, with FTS5 word + trigram indexes.
//! - **Images** ([`images`]): content-addressed files under `<root>/images/`,
//!   indexed in SQLite and evicted LRU by total size.
//!
//! The database is opened lazily on first use; if it can't be opened every
//! call errors, which [`CacheHandle`](grabcli_core::cache::CacheHandle)
//! treats as a miss. Housekeeping (`stats`, `clear`, `prune`) is only on
//! this type, for `grab cache`. Schema migrations live in `db.rs`: append to
//! `MIGRATIONS`, never edit a shipped one.

pub mod catalog;
mod db;
mod err;
pub mod images;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use async_trait::async_trait;
use grabcli_core::cache::{
    Cache, CacheConfig, CacheEntry, CacheKey, CacheKind, CachedImage, CategoryRow, ItemHit,
    ItemQuery, ItemRow, RestaurantHit, RestaurantQuery, RestaurantRow, is_fresh,
};
use grabcli_core::error::{Error, ErrorKind};
use grabcli_core::jwt;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::err::{DbError, Result};

pub use db::DB_FILE;

type CoreResult<T> = grabcli_core::error::Result<T>;

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

/// The SQLite cache. Cheap to clone (clones share one connection).
#[derive(Debug, Clone)]
pub struct SqliteCache {
    root: PathBuf,
    config: CacheConfig,
    db: Arc<DbCell>,
}

impl SqliteCache {
    /// A cache rooted at `root` (the database is `root/cache.sqlite`).
    /// `config` supplies the image cap, the catalog max age and the TTLs used
    /// by [`stats`](Self::stats) and [`prune`](Self::prune).
    pub fn new(root: impl Into<PathBuf>, config: CacheConfig) -> Self {
        Self {
            root: root.into(),
            config,
            db: Arc::new(DbCell::default()),
        }
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

    /// The open connection, opening it on first use.
    fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        if self.root.as_os_str().is_empty() {
            return Err(DbError(Error::invalid("no cache directory")));
        }
        let cell = self.db.conn.get_or_init(|| {
            db::open(&self.db_path(), jwt::now())
                .map(Mutex::new)
                .map_err(|e| e.0.message)
        });
        match cell {
            Ok(m) => Ok(m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)),
            Err(msg) => Err(DbError(Error::new(ErrorKind::Io, msg.clone()))),
        }
    }

    fn get_entry_sync(&self, key: &CacheKey) -> Result<Option<CacheEntry>> {
        let conn = self.conn()?;
        Ok(conn
            .query_row(
                "SELECT identity, body, fetched_at, expires_at FROM entries WHERE key = ?1",
                [key.hash()],
                |r| {
                    Ok(CacheEntry {
                        identity: r.get(0)?,
                        body: r.get(1)?,
                        fetched_at: r.get(2)?,
                        expires_at: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    fn put_entry_sync(&self, key: &CacheKey, e: &CacheEntry) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT OR REPLACE INTO entries \
             (key, namespace, kind, identity, body, fetched_at, expires_at, etag) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
            params![
                key.hash(),
                key.namespace(),
                key.kind().as_str(),
                e.identity,
                e.body,
                e.fetched_at,
                e.expires_at
            ],
        )?;
        Ok(())
    }

    pub fn stats(&self) -> CoreResult<CacheStats> {
        Ok(self.stats_at(jwt::now())?)
    }

    #[doc(hidden)]
    pub fn stats_at(&self, now: i64) -> Result<CacheStats> {
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
    pub fn clear(&self, kind: Option<CacheKind>, expired_only: bool) -> CoreResult<u64> {
        Ok(self.clear_at(kind, expired_only, jwt::now())?)
    }

    #[doc(hidden)]
    pub fn clear_at(&self, kind: Option<CacheKind>, expired_only: bool, now: i64) -> Result<u64> {
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
    pub fn clear_all(&self) -> CoreResult<ClearReport> {
        Ok(self.clear_all_inner()?)
    }

    fn clear_all_inner(&self) -> Result<ClearReport> {
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
    pub fn prune(&self) -> CoreResult<PruneReport> {
        Ok(self.prune_at(jwt::now())?)
    }

    #[doc(hidden)]
    pub fn prune_at(&self, now: i64) -> Result<PruneReport> {
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

fn core<T>(r: Result<T>) -> CoreResult<T> {
    r.map_err(Into::into)
}

#[async_trait]
impl Cache for SqliteCache {
    async fn get_entry(&self, key: &CacheKey) -> CoreResult<Option<CacheEntry>> {
        core(self.get_entry_sync(key))
    }

    async fn put_entry(&self, key: &CacheKey, entry: &CacheEntry) -> CoreResult<()> {
        core(self.put_entry_sync(key, entry))
    }

    async fn index_restaurants(
        &self,
        rows: &[RestaurantRow],
        seen_from: Option<(f64, f64)>,
        now: i64,
    ) -> CoreResult<()> {
        core(self.index_restaurants_at(rows, seen_from, now))
    }

    async fn index_items(&self, items: &[ItemRow], now: i64) -> CoreResult<()> {
        core(self.index_items_at(items, now))
    }

    async fn index_menu(
        &self,
        restaurant: &RestaurantRow,
        categories: &[CategoryRow],
        items: &[ItemRow],
        seen_from: Option<(f64, f64)>,
        now: i64,
    ) -> CoreResult<()> {
        core(self.index_menu_at(restaurant, categories, items, seen_from, now))
    }

    async fn search_restaurants(&self, q: &RestaurantQuery) -> CoreResult<Vec<RestaurantHit>> {
        core(SqliteCache::search_restaurants(self, q))
    }

    async fn search_items(&self, q: &ItemQuery) -> CoreResult<Vec<ItemHit>> {
        core(SqliteCache::search_items(self, q))
    }

    async fn restaurant(&self, id: &str) -> CoreResult<Option<RestaurantHit>> {
        core(SqliteCache::restaurant(self, id))
    }

    async fn image(&self, url: &str, now: i64) -> CoreResult<Option<CachedImage>> {
        Ok(self.cached_image_at(url, now))
    }

    async fn put_image(
        &self,
        url: &str,
        bytes: &[u8],
        content_type: &str,
        now: i64,
    ) -> CoreResult<CachedImage> {
        core(self.store_image_at(url, bytes, content_type, now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grabcli_core::cache::{CacheHandle, CacheMode};
    use serde_json::{Value, json};

    fn cache(dir: &Path) -> SqliteCache {
        SqliteCache::new(dir, CacheConfig::default())
    }

    fn handle(c: &SqliteCache) -> CacheHandle {
        CacheHandle::new(Arc::new(c.clone()), c.config.clone(), CacheMode::Normal)
    }

    fn key() -> CacheKey {
        CacheKey::new("food", CacheKind::Menu, "merchant")
            .param("id", "2-EXAMPLE")
            .location(1.23, 103.81)
    }

    async fn get_at(c: &SqliteCache, k: &CacheKey, now: i64) -> Option<Value> {
        handle(c).lookup_at::<Value>(k, now).await.map(|f| f.value)
    }

    #[tokio::test]
    async fn roundtrip_through_handle() -> CoreResult<()> {
        let dir = tempfile::tempdir()?;
        let c = cache(dir.path());
        let h = handle(&c);
        h.put_at(&key(), &json!({"a": 1}), 1_000).await?;
        assert_eq!(get_at(&c, &key(), 1_000).await, Some(json!({"a": 1})));
        // A tampered identity is a miss.
        c.conn()?
            .execute("UPDATE entries SET identity = 'x'", [])
            .map_err(DbError::from)?;
        assert_eq!(get_at(&c, &key(), 1_000).await, None);

        // An unusable cache dir: reads miss, get_or_fetch still works.
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x")?;
        let broken = handle(&cache(&file));
        assert_eq!(broken.get::<Value>(&key()).await, None);
        assert!(broken.put(&key(), &json!(1)).await.is_err());
        let v: Value = broken
            .get_or_fetch(&key(), || async { Ok(json!(2)) })
            .await?;
        assert_eq!(v, json!(2));
        Ok(())
    }

    #[tokio::test]
    async fn stats_clear_and_prune() -> CoreResult<()> {
        let dir = tempfile::tempdir()?;
        let c = cache(dir.path());
        let h = handle(&c);
        let menu = key();
        let list = CacheKey::new("food", CacheKind::List, "search").param("keyword", "x");
        let mart = CacheKey::new("mart", CacheKind::Categories, "categories");
        h.put_at(&menu, &json!({"a": 1}), 0).await?; // expired by `now` below
        h.put_at(&list, &json!([1, 2]), 10_000).await?;
        h.put_at(&mart, &json!([]), 10_000).await?;
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
        assert_eq!(get_at(&c, &list, now).await, None);
        assert_eq!(get_at(&c, &mart, now).await, Some(json!([])));

        // Prune removes what has expired by then.
        let later = 10_000 + CacheKind::Categories.default_ttl() as i64;
        let r = c.prune_at(later)?;
        assert_eq!(r.entries, 1);
        assert_eq!(c.stats_at(later)?.entries, 0);

        // Expired rows are also pruned (boundedly) when the db is opened.
        h.put_at(&menu, &json!(1), 0).await?;
        let reopened = cache(dir.path());
        assert_eq!(reopened.stats()?.entries, 0);
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn private_permissions() -> CoreResult<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("grabcli");
        let c = cache(&root);
        handle(&c).put(&key(), &json!(1)).await?;
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
}
