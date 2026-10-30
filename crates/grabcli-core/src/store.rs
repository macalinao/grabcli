//! Where a [`Session`] lives. The library never picks a path: the embedder
//! passes an `Arc<dyn SessionStore>` to [`Context::new`](crate::Context::new)
//! and the context loads the session once, then saves every change it makes
//! (silent SSO refresh, hydra mint, GrabMart login, address changes) through
//! it.
//!
//! The trait is async (via `async_trait`) because [`Context`](crate::Context)
//! is async and embedders may back it with an async database client; calling
//! a blocking store from inside the context would need a nested runtime. A
//! file-backed store simply does its (small) blocking IO inside the future.
//!
//! The CLI's `FileSessionStore` (`session.json`, mode 0600) lives in the
//! `grabcli` binary crate; [`MemorySessionStore`] is for tests and embedders
//! that persist elsewhere.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use crate::error::Result;
use crate::session::Session;

/// Loads and saves the one Grab session a [`Context`](crate::Context) uses.
/// Implementations must never log cookie or token values.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// The stored session, or `None` if there is none yet.
    async fn load(&self) -> Result<Option<Session>>;
    /// Replace the stored session.
    async fn save(&self, session: &Session) -> Result<()>;
}

/// A session held in memory. Counts saves (handy in tests).
#[derive(Default)]
pub struct MemorySessionStore {
    session: Mutex<Option<Session>>,
    saves: AtomicUsize,
}

impl std::fmt::Debug for MemorySessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySessionStore")
            .field("saves", &self.save_count())
            .finish_non_exhaustive()
    }
}

impl MemorySessionStore {
    pub fn new(session: Option<Session>) -> Self {
        Self {
            session: Mutex::new(session),
            saves: AtomicUsize::new(0),
        }
    }

    /// The current session (a clone).
    pub fn get(&self) -> Option<Session> {
        self.session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// How many times [`SessionStore::save`] was called.
    pub fn save_count(&self) -> usize {
        self.saves.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SessionStore for MemorySessionStore {
    async fn load(&self) -> Result<Option<Session>> {
        Ok(self.get())
    }

    async fn save(&self, session: &Session) -> Result<()> {
        *self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session.clone());
        self.saves.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn memory_store_roundtrip() -> Result<()> {
        let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::default());
        assert!(store.load().await?.is_none());
        let s = Session {
            web_app_version: Some("v1".into()),
            ..Session::default()
        };
        store.save(&s).await?;
        let back = store
            .load()
            .await?
            .ok_or_else(|| crate::Error::invalid("none"))?;
        assert_eq!(back.web_app_version.as_deref(), Some("v1"));
        Ok(())
    }
}
