//! Content-addressed image store: `<cache dir>/images/ab/cd/<sha256>.<ext>`
//! (files 0600, dirs 0700), indexed in the `images` table by URL. Several URLs
//! with identical bytes share one file. When the total size of distinct files
//! exceeds `[cache] image_max_mb` (default 500), the least recently used files
//! are evicted.
//!
//! Images are fetched without cookies or any session headers, and only from
//! Grab's CDN hosts ([`image_url_allowed`]). Originals are stored as-is: Grab
//! mostly serves AVIF/WebP, which the pure-Rust `image` crate can't decode
//! (AVIF) or only encode losslessly (WebP), so there is no thumbnailing;
//! width/height are recorded for PNG/JPEG only.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use super::db::create_private_dirs;
use super::{Cache, ImageStats, hex_sha256};
use crate::error::{Error, ErrorKind, Result};
use crate::jwt;

/// Largest image we will download.
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// A cached image file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CachedImage {
    pub url: String,
    pub path: PathBuf,
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

fn ext_for(content_type: &str) -> &'static str {
    match content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/avif" => "avif",
        "image/gif" => "gif",
        _ => "img",
    }
}

fn file_path(root: &Path, hash: &str, content_type: &str) -> PathBuf {
    let a = hash.get(0..2).unwrap_or("00");
    let b = hash.get(2..4).unwrap_or("00");
    root.join("images")
        .join(a)
        .join(b)
        .join(format!("{hash}.{}", ext_for(content_type)))
}

fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

fn write_private_file(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        create_private_dirs(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

type ImageRow = (String, i64, Option<i64>, Option<i64>, String);

fn row_for(conn: &Connection, url: &str) -> Result<Option<ImageRow>> {
    Ok(conn
        .query_row(
            "SELECT hash, bytes, width, height, content_type FROM images WHERE url = ?1",
            [url],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?)
}

fn to_cached(root: &Path, url: &str, row: ImageRow, cached: bool) -> CachedImage {
    let (hash, bytes, w, h, ct) = row;
    CachedImage {
        url: url.to_string(),
        path: file_path(root, &hash, &ct),
        hash,
        bytes: u64::try_from(bytes).unwrap_or(0),
        width: w.and_then(|v| u32::try_from(v).ok()),
        height: h.and_then(|v| u32::try_from(v).ok()),
        content_type: ct,
        cached,
    }
}

pub(crate) fn stats(conn: &Connection, max_bytes: u64) -> Result<ImageStats> {
    let (files, bytes): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(bytes), 0) FROM
            (SELECT hash, MAX(bytes) AS bytes FROM images GROUP BY hash)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let urls: i64 = conn.query_row("SELECT COUNT(*) FROM images", [], |r| r.get(0))?;
    Ok(ImageStats {
        files: u64::try_from(files).unwrap_or(0),
        urls: u64::try_from(urls).unwrap_or(0),
        bytes: u64::try_from(bytes).unwrap_or(0),
        max_bytes,
    })
}

/// Evict least-recently-used files until the store is within `max_bytes`.
/// Returns `(files evicted, bytes freed)`.
pub(crate) fn evict(conn: &Connection, root: &Path, max_bytes: u64) -> Result<(u64, u64)> {
    let mut stmt = conn.prepare(
        "SELECT hash, MAX(bytes), MAX(last_access), MIN(content_type)
         FROM images GROUP BY hash ORDER BY MAX(last_access) ASC, hash ASC",
    )?;
    let files: Vec<(String, i64, i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    let mut total: u64 = files.iter().map(|f| u64::try_from(f.1).unwrap_or(0)).sum();
    let (mut n, mut freed) = (0u64, 0u64);
    for (hash, bytes, _, ct) in files {
        if total <= max_bytes {
            break;
        }
        let bytes = u64::try_from(bytes).unwrap_or(0);
        let _ = fs::remove_file(file_path(root, &hash, &ct));
        conn.execute("DELETE FROM images WHERE hash = ?1", [&hash])?;
        conn.execute(
            "UPDATE menu_items SET image_hash = NULL WHERE image_hash = ?1",
            [&hash],
        )?;
        total = total.saturating_sub(bytes);
        n += 1;
        freed += bytes;
    }
    Ok((n, freed))
}

/// Remove every image row and file. Returns the number of URLs removed.
pub(crate) fn clear(conn: &Connection, root: &Path) -> Result<u64> {
    let n = conn.execute("DELETE FROM images", [])? as u64;
    conn.execute("UPDATE menu_items SET image_hash = NULL", [])?;
    let dir = root.join("images");
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    Ok(n)
}

impl Cache {
    /// The cached file for `url`, if present (and still on disk). Marks it
    /// recently used.
    pub fn cached_image(&self, url: &str) -> Option<CachedImage> {
        self.cached_image_at(url, jwt::now())
    }

    fn cached_image_at(&self, url: &str, now: i64) -> Option<CachedImage> {
        if !self.mode.reads() && !self.mode.writes() {
            return None;
        }
        let conn = self.conn().ok()?;
        let row = row_for(&conn, url).ok()??;
        let img = to_cached(&self.root, url, row, true);
        if !img.path.exists() {
            let _ = conn.execute("DELETE FROM images WHERE url = ?1", [url]);
            return None;
        }
        let _ = conn.execute(
            "UPDATE images SET last_access = ?1 WHERE hash = ?2",
            params![now, img.hash],
        );
        Some(img)
    }

    /// Store downloaded image bytes for `url` (dedupes by content hash), then
    /// evict over the size cap.
    pub fn store_image(&self, url: &str, bytes: &[u8], content_type: &str) -> Result<CachedImage> {
        self.store_image_at(url, bytes, content_type, jwt::now())
    }

    pub fn store_image_at(
        &self,
        url: &str,
        bytes: &[u8],
        content_type: &str,
        now: i64,
    ) -> Result<CachedImage> {
        if !self.mode.writes() {
            return Err(Error::invalid("the cache is off; images are not stored"));
        }
        let hash = hex_sha256(bytes);
        let path = file_path(&self.root, &hash, content_type);
        if !path.exists() {
            write_private_file(&path, bytes)?;
        }
        let dims = dimensions(bytes);
        let conn = self.conn()?;
        conn.execute(
            "INSERT OR REPLACE INTO images
                (url, hash, bytes, width, height, content_type, last_access, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![
                url,
                hash,
                i64::try_from(bytes.len()).unwrap_or(i64::MAX),
                dims.map(|d| d.0),
                dims.map(|d| d.1),
                content_type,
                now
            ],
        )?;
        // Touch every URL sharing this file so LRU sees the file as fresh.
        conn.execute(
            "UPDATE images SET last_access = ?1 WHERE hash = ?2",
            params![now, hash],
        )?;
        conn.execute(
            "UPDATE menu_items SET image_hash = ?1 WHERE image_url = ?2",
            params![hash, url],
        )?;
        evict(&conn, &self.root, self.config.image_max_bytes())?;
        let row = row_for(&conn, url)?
            .ok_or_else(|| Error::invalid("image is larger than the image cache cap"))?;
        Ok(to_cached(&self.root, url, row, false))
    }

    /// The cached file for `url`, downloading it first if needed. Only Grab
    /// CDN URLs are fetched ([`image_url_allowed`]); no cookies are sent.
    pub async fn fetch_image(&self, url: &str) -> Result<CachedImage> {
        if !image_url_allowed(url) {
            return Err(Error::new(
                ErrorKind::UrlNotAllowed,
                "only https image URLs on grab.com hosts can be cached",
            ));
        }
        if let Some(hit) = self.cached_image(url) {
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
        self.store_image(url, &bytes, &content_type)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{CacheConfig, CacheMode};

    #[test]
    fn url_allowlist() {
        assert!(image_url_allowed(
            "https://food-cms.grab.com/compressed_webp/x.webp"
        ));
        assert!(image_url_allowed(
            "https://huawei-food-cms.grab.com/a/b.avif"
        ));
        assert!(!image_url_allowed("http://food-cms.grab.com/x.png"));
        assert!(!image_url_allowed("https://evil.com/x.png"));
        assert!(!image_url_allowed("https://grab.com.evil.com/x.png"));
        assert!(!image_url_allowed("https://user:pw@food-cms.grab.com/x"));
        assert!(!image_url_allowed("https://food-cms.grab.com:8443/x"));
        assert!(!image_url_allowed("not a url"));
    }

    fn png(w: u32, h: u32) -> Result<Vec<u8>> {
        let img = image::RgbImage::new(w, h);
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png)
            .map_err(|e| Error::invalid(e.to_string()))?;
        Ok(out.into_inner())
    }

    #[test]
    fn store_dedupes_and_records_dimensions() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let c = Cache::new(dir.path(), CacheConfig::default(), CacheMode::Normal);
        let bytes = png(3, 2)?;
        let a = c.store_image_at("https://food-cms.grab.com/a.png", &bytes, "image/png", 10)?;
        let b = c.store_image_at("https://food-cms.grab.com/b.png", &bytes, "image/png", 11)?;
        assert_eq!(a.path, b.path);
        assert_eq!((a.width, a.height), (Some(3), Some(2)));
        assert!(a.path.exists());
        assert!(a.path.to_string_lossy().contains(&format!(
            "images/{}/{}/{}.png",
            &a.hash[0..2],
            &a.hash[2..4],
            a.hash
        )));
        let s = c.stats_at(11)?;
        assert_eq!((s.images.files, s.images.urls), (1, 2));
        assert_eq!(s.images.bytes, bytes.len() as u64);
        let hit = c
            .cached_image("https://food-cms.grab.com/a.png")
            .ok_or_else(|| Error::not_found("a"))?;
        assert!(hit.cached);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&a.path)?.permissions().mode() & 0o777, 0o600);
        }
        Ok(())
    }

    #[test]
    fn lru_eviction_by_total_size() -> Result<()> {
        let dir = tempfile::tempdir()?;
        // 1 MiB cap; each "image" is 400 KiB of distinct bytes.
        let cfg = CacheConfig {
            image_max_mb: Some(1),
            ..CacheConfig::default()
        };
        let c = Cache::new(dir.path(), cfg, CacheMode::Normal);
        let blob = |n: u8| vec![n; 400 * 1024];
        let url = |n: u8| format!("https://food-cms.grab.com/{n}.webp");
        let first = c.store_image_at(&url(1), &blob(1), "image/webp", 100)?;
        c.store_image_at(&url(2), &blob(2), "image/webp", 200)?;
        // Touch #1 so #2 becomes the least recently used.
        c.cached_image_at(&url(1), 300)
            .ok_or_else(|| Error::not_found("1"))?;
        c.store_image_at(&url(3), &blob(3), "image/webp", 400)?;
        assert!(c.cached_image(&url(1)).is_some());
        assert!(c.cached_image(&url(2)).is_none());
        assert!(c.cached_image(&url(3)).is_some());
        assert!(first.path.exists());
        let s = c.stats_at(400)?;
        assert_eq!(s.images.files, 2);
        assert!(s.images.bytes <= 1024 * 1024);

        // A cap below one image: prune evicts down to it.
        let tiny = Cache::new(
            dir.path(),
            CacheConfig {
                image_max_mb: Some(0),
                ..CacheConfig::default()
            },
            CacheMode::Normal,
        );
        let r = tiny.prune_at(500)?;
        assert_eq!(r.images_evicted, 2);
        assert!(!first.path.exists());
        Ok(())
    }

    #[test]
    fn off_mode_stores_nothing() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let c = Cache::new(dir.path(), CacheConfig::default(), CacheMode::Off);
        assert!(
            c.store_image("https://food-cms.grab.com/a.png", b"x", "image/png")
                .is_err()
        );
        assert!(c.cached_image("https://food-cms.grab.com/a.png").is_none());
        Ok(())
    }
}
