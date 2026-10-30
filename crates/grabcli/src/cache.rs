//! `grab cache …`: inspect, prune and clear the local cache database.
//!
//! The cache only ever holds read-only browse data (search results, menus,
//! store details, category lists, the restaurant/item catalog and images);
//! see `grabcli_core::cache`. The database is `grabcli_sqlite::SqliteCache`.

use bpaf::Bpaf;
use std::sync::Arc;

use grabcli_core::cache::{CacheHandle, CacheKind, CacheMode};
use grabcli_core::error::Result;
use grabcli_sqlite::{CacheStats, SqliteCache};
use serde_json::json;

use crate::output::Output;
use crate::{local, paths};

/// `grab cache <cmd>`.
#[derive(Debug, Clone, Bpaf)]
pub enum CacheCmd {
    /// Show response entries per kind, catalog row counts, images and sizes.
    #[bpaf(command("stats"))]
    Stats {
        #[bpaf(long("json"), switch)]
        json: bool,
    },

    /// Remove cached responses (all, one kind, and/or only expired ones), or
    /// everything with --all.
    #[bpaf(command("clear"))]
    Clear {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Only this kind: list, menu, details or categories.
        #[bpaf(long("kind"), argument("KIND"))]
        kind: Option<CacheKind>,
        /// Only expired entries.
        #[bpaf(long("expired"), switch)]
        expired: bool,
        /// Also wipe the restaurant/item catalog and the image store.
        #[bpaf(long("all"), switch)]
        all: bool,
    },

    /// Drop expired responses and old catalog rows, evict images over the
    /// size cap, and compact the database.
    #[bpaf(command("prune"))]
    Prune {
        #[bpaf(long("json"), switch)]
        json: bool,
    },

    /// Print the cache directory and database path.
    #[bpaf(command("path"))]
    Path {
        #[bpaf(long("json"), switch)]
        json: bool,
    },

    /// Download (if needed) and print the local path of a Grab CDN image.
    #[bpaf(command("image"))]
    Image {
        #[bpaf(long("json"), switch)]
        json: bool,
        #[bpaf(positional("URL"))]
        url: String,
    },
}

pub fn wants_json(cmd: &CacheCmd) -> bool {
    match cmd {
        CacheCmd::Stats { json }
        | CacheCmd::Clear { json, .. }
        | CacheCmd::Prune { json }
        | CacheCmd::Path { json }
        | CacheCmd::Image { json, .. } => *json,
    }
}

fn size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn print_stats(out: Output, cache: &SqliteCache, s: &CacheStats) {
    if out.is_json() {
        let mut j = serde_json::to_value(s).unwrap_or_default();
        j["ttl_secs"] = CacheKind::ALL
            .iter()
            .map(|k| (k.as_str().to_string(), json!(cache.ttl(*k))))
            .collect::<serde_json::Map<_, _>>()
            .into();
        out.json(&j);
        return;
    }
    out.line(format!(
        "db {}  {}  schema v{}{}",
        s.db_path,
        size(s.db_bytes),
        s.schema_version,
        if s.enabled { "" } else { "  (disabled)" }
    ));
    out.line(format!(
        "responses: {} entries, {}, {} expired",
        s.entries,
        size(s.total_bytes),
        s.expired,
    ));
    for kind in CacheKind::ALL {
        let Some(k) = s.kinds.get(kind.as_str()) else {
            continue;
        };
        out.line(format!(
            "  {:<10} {:>5}  {:>10}  {} expired  (ttl {}s)",
            kind.as_str(),
            k.entries,
            size(k.bytes),
            k.expired,
            cache.ttl(kind)
        ));
    }
    out.line("catalog:");
    for (table, n) in &s.tables {
        out.line(format!("  {table:<21} {n:>6}"));
    }
    out.line(format!(
        "images: {} files ({} urls), {} of {} cap",
        s.images.files,
        s.images.urls,
        size(s.images.bytes),
        size(s.images.max_bytes)
    ));
}

pub async fn run(out: Output, cmd: CacheCmd) -> Result<()> {
    let config = local::load_config()?;
    // Reads/writes don't matter here; stats/clear/prune/path work in any mode.
    let cache = SqliteCache::new(paths::cache_dir()?, config.cache);
    match cmd {
        CacheCmd::Stats { .. } => print_stats(out, &cache, &cache.stats()?),
        CacheCmd::Clear {
            kind, expired, all, ..
        } => {
            if all {
                let r = cache.clear_all()?;
                if out.is_json() {
                    out.json(&json!({ "removed": r, "all": true }));
                } else {
                    out.line(format!(
                        "removed {} responses, {} catalog rows, {} images",
                        r.entries, r.catalog_rows, r.images
                    ));
                }
            } else {
                let removed = cache.clear(kind, expired)?;
                if out.is_json() {
                    out.json(&json!({
                        "removed": removed,
                        "kind": kind.map(CacheKind::as_str),
                        "expired_only": expired,
                    }));
                } else {
                    out.line(format!("removed {removed} cached responses"));
                }
            }
        }
        CacheCmd::Prune { .. } => {
            let r = cache.prune()?;
            if out.is_json() {
                out.json(&serde_json::to_value(&r)?);
            } else {
                out.line(format!(
                    "pruned {} responses, {} restaurants, {} locations, {} categories, {} items; \
                     evicted {} images ({})",
                    r.entries,
                    r.restaurants,
                    r.restaurant_locations,
                    r.menu_categories,
                    r.menu_items,
                    r.images_evicted,
                    size(r.image_bytes_freed)
                ));
            }
        }
        CacheCmd::Path { .. } => {
            let path = cache.root().display().to_string();
            if out.is_json() {
                out.json(&json!({ "path": path, "db": cache.db_path().display().to_string() }));
            } else {
                out.line(path);
            }
        }
        CacheCmd::Image { url, .. } => {
            let handle = CacheHandle::new(
                Arc::new(cache.clone()),
                cache.config().clone(),
                CacheMode::Normal,
            );
            let img = handle.fetch_image(&url).await?;
            if out.is_json() {
                out.json(&serde_json::to_value(&img)?);
            } else {
                out.line(
                    img.path
                        .as_deref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bpaf::Parser;

    #[test]
    fn parses_clear_flags() {
        let p = cache_cmd().to_options();
        let r = p.run_inner(&["clear", "--kind", "menu", "--expired"]);
        assert!(matches!(
            r,
            Ok(CacheCmd::Clear {
                kind: Some(CacheKind::Menu),
                expired: true,
                all: false,
                ..
            })
        ));
        assert!(p.run_inner(&["clear", "--kind", "quote"]).is_err());
        assert!(matches!(
            p.run_inner(&["clear", "--all"]),
            Ok(CacheCmd::Clear { all: true, .. })
        ));
        assert!(matches!(
            p.run_inner(&["prune"]),
            Ok(CacheCmd::Prune { .. })
        ));
        assert!(matches!(
            p.run_inner(&["image", "https://food-cms.grab.com/x.webp"]),
            Ok(CacheCmd::Image { .. })
        ));
    }
}
