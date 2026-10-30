//! The CLI's storage: config.toml, `session.json` (0600), the login send log
//! and pending login, the hydra key, and the SQLite cache. The library crates
//! own none of this; they get it through [`Context::new`].

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use grabcli_core::Context;
use grabcli_core::cache::{Cache, CacheMode, NO_CACHE_ENV};
use grabcli_core::config::Config;
use grabcli_core::error::{Error, ErrorKind, Result};
use grabcli_core::http::Allowlist;
use grabcli_core::login::{PendingLogin, SendBudget, SendLog};
use grabcli_core::session::Session;
use grabcli_core::store::SessionStore;
use grabcli_sqlite::SqliteCache;

use crate::paths;

/// `config.toml` (defaults if it doesn't exist).
pub fn load_config() -> Result<Config> {
    let path = paths::config_path()?;
    match std::fs::read_to_string(&path) {
        Ok(s) => Config::parse(&s)
            .map_err(|e| Error::new(e.kind, format!("{}: {}", path.display(), e.message))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e.into()),
    }
}

/// A session kept in one JSON file, written 0600.
#[derive(Debug, Clone)]
pub struct FileSessionStore {
    pub path: PathBuf,
}

impl FileSessionStore {
    /// `$GRABCLI_SESSION` or `<config dir>/session.json`.
    pub fn default_path() -> Result<Self> {
        Ok(Self {
            path: paths::session_path()?,
        })
    }

    pub fn read(&self) -> Result<Option<Session>> {
        match std::fs::read(&self.path) {
            Ok(b) => serde_json::from_slice(&b)
                .map(Some)
                .map_err(|e| Error::parse(format!("{}: {e}", self.path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The session, or the CLI's `auth_missing` error naming the file.
    pub fn require(&self) -> Result<Session> {
        self.read()?.ok_or_else(|| {
            Error::new(
                ErrorKind::AuthMissing,
                format!(
                    "no session at {}: run `grab auth import` (or `--from <session.json>`)",
                    self.path.display()
                ),
            )
        })
    }

    pub fn write(&self, session: &Session) -> Result<()> {
        paths::write_private(&self.path, &serde_json::to_vec_pretty(session)?)
    }
}

#[async_trait]
impl SessionStore for FileSessionStore {
    async fn load(&self) -> Result<Option<Session>> {
        self.read()
    }

    async fn save(&self, session: &Session) -> Result<()> {
        self.write(session)
    }
}

/// The login send log, `<state>/login-sends.json` (0600).
#[derive(Debug, Clone)]
pub struct FileSendBudget {
    pub path: PathBuf,
}

impl FileSendBudget {
    pub fn default_path() -> Result<Self> {
        Ok(Self {
            path: paths::state_dir()?.join("login-sends.json"),
        })
    }

    pub fn log(&self) -> Result<SendLog> {
        match std::fs::read(&self.path) {
            Ok(b) => Ok(serde_json::from_slice(&b).unwrap_or_default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SendLog::default()),
            Err(e) => Err(e.into()),
        }
    }
}

#[async_trait]
impl SendBudget for FileSendBudget {
    async fn check(&self, now: i64) -> Result<()> {
        self.log()?.check(now)
    }

    async fn record(&self, now: i64) -> Result<()> {
        let mut log = self.log()?;
        log.record(now);
        paths::write_private(&self.path, &serde_json::to_vec_pretty(&log)?)
    }
}

fn pending_path() -> Result<PathBuf> {
    Ok(paths::state_dir()?.join("login-pending.json"))
}

/// The pending login, `<state>/login-pending.json`.
pub fn read_pending() -> Result<Option<PendingLogin>> {
    match std::fs::read(pending_path()?) {
        Ok(b) => Ok(Some(serde_json::from_slice(&b)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Save (`Some`) or remove (`None`) the pending login.
pub fn write_pending(p: Option<&PendingLogin>) -> Result<()> {
    let path = pending_path()?;
    match p {
        Some(p) => paths::write_private(&path, &serde_json::to_vec_pretty(p)?),
        None => match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        },
    }
}

/// The hydra signing key: `hydra_key` from config, else
/// `<config dir>/hydra-key.pem` if it exists.
pub fn hydra_key(config: &Config) -> Option<String> {
    let path = match &config.hydra_key {
        Some(p) => p.clone(),
        None => {
            let p = paths::config_dir().ok()?.join("hydra-key.pem");
            p.exists().then_some(p)?
        }
    };
    std::fs::read_to_string(path).ok()
}

/// `GRAB_NO_CACHE=1` (any truthy value) turns the cache off.
pub fn cache_mode(mode: CacheMode) -> CacheMode {
    let off = std::env::var(NO_CACHE_ENV).is_ok_and(|v| {
        let v = v.trim().to_ascii_lowercase();
        !(v.is_empty() || v == "0" || v == "false" || v == "no")
    });
    if off { CacheMode::Off } else { mode }
}

/// The SQLite cache under the cache dir (`None` if there's no cache dir).
pub fn sqlite_cache(config: &Config) -> Option<SqliteCache> {
    paths::cache_dir()
        .ok()
        .map(|root| SqliteCache::new(root, config.cache.clone()))
}

/// A context over the CLI's files: config, `session.json`, the SQLite cache
/// (in `mode`, after `GRAB_NO_CACHE`), the hydra key and the allowlist
/// `allow` builds from the config.
pub async fn context(allow: impl FnOnce(&Config) -> Allowlist, mode: CacheMode) -> Result<Context> {
    let config = load_config()?;
    let allow = allow(&config);
    let store = FileSessionStore::default_path()?;
    // The same check Context::new makes, with the message naming the file.
    store.require()?;
    let (cache, mode): (Arc<dyn Cache>, CacheMode) = match sqlite_cache(&config) {
        Some(c) => (Arc::new(c), cache_mode(mode)),
        None => (
            Arc::new(grabcli_core::cache::memory::NoCache),
            CacheMode::Off,
        ),
    };
    let key = hydra_key(&config);
    Ok(Context::new(config, Arc::new(store), cache)
        .await?
        .with_hydra_key(key)
        .with_allowlist(&allow)?
        .with_cache_mode(mode))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_store_roundtrip_is_private() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = FileSessionStore {
            path: dir.path().join("sub").join("session.json"),
        };
        assert!(store.load().await?.is_none());
        assert_eq!(
            store.require().err().map(|e| e.kind),
            Some(ErrorKind::AuthMissing)
        );
        let s = Session {
            web_app_version: Some("v1".into()),
            ..Session::default()
        };
        store.save(&s).await?;
        let back = store.require()?;
        assert_eq!(back.web_app_version.as_deref(), Some("v1"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&store.path)?.permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        Ok(())
    }

    #[tokio::test]
    async fn file_budget_counts_sends() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let b = FileSendBudget {
            path: dir.path().join("login-sends.json"),
        };
        assert!(b.log()?.sends.is_empty());
        b.check(1_000).await?;
        b.record(1_000).await?;
        assert_eq!(b.log()?.sends, vec![1_000]);
        Ok(())
    }
}
