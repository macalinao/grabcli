//! The GrabMart API client: a [`Context`] plus reCAPTCHA minting, call pacing
//! and mart-specific error messages.
//!
//! - Public browse calls mint a fresh reCAPTCHA token per request (only on a
//!   cache miss) and are cached under namespace `"mart"`.
//! - Calls are paced (≥ [`MIN_GAP`] apart) through a [`Pacer`]: in-process
//!   by default ([`MemoryPacer`]); the CLI paces across processes with a
//!   timestamp file. Needed because the mart API answers 429 after ~10 quick calls. A 429 on a
//!   read-only GET is retried once after a back-off, then surfaced clearly.
//! - The cart/quote call is never cached and never retried here.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use grabcli_core::Context;
use grabcli_core::cache::CacheKey;
use grabcli_core::error::{Error, ErrorKind, Result};
use grabcli_core::http::{Hydra, Request};
use grabcli_core::jwt;
use grabcli_core::location::Location;
use serde_json::Value;

use crate::{api, recaptcha};

/// Cache namespace for GrabMart entries.
pub const CACHE_NS: &str = "mart";

/// Minimum gap between two mart API calls.
pub const MIN_GAP: Duration = Duration::from_millis(2000);

/// Back-off before the single retry of a rate-limited read.
const RETRY_AFTER_429: Duration = Duration::from_secs(10);

pub struct Mart {
    pub ctx: Context,
    pub loc: Location,
    cdp: String,
    pacer: Arc<dyn Pacer>,
    /// Print rate-limit retry notices to stderr (the CLI's human mode).
    pub notices: bool,
}

/// Spaces mart API calls at least [`MIN_GAP`] apart.
#[async_trait]
pub trait Pacer: Send + Sync {
    /// Sleep until the next call may go out, then record it.
    async fn wait(&self);
}

/// Paces calls made through this process only.
#[derive(Debug, Default)]
pub struct MemoryPacer {
    last: Mutex<Option<Instant>>,
}

#[async_trait]
impl Pacer for MemoryPacer {
    async fn wait(&self) {
        let wait = {
            let last = self
                .last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            last.map(|t| MIN_GAP.saturating_sub(t.elapsed()))
        };
        if let Some(w) = wait.filter(|w| !w.is_zero()) {
            tokio::time::sleep(w).await;
        }
        *self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
    }
}

/// Headers the mart web app sends (besides cookies/UA from the core client).
fn mart_headers() -> Vec<(String, String)> {
    vec![
        ("origin".into(), api::ORIGIN.into()),
        ("referer".into(), format!("{}/", api::ORIGIN)),
        ("x-country-code".into(), "PH".into()),
    ]
}

/// Re-word core errors for the mart API.
pub fn map_err(e: Error) -> Error {
    match e.kind {
        ErrorKind::RateLimited => Error::new(
            ErrorKind::RateLimited,
            "GrabMart rate limit (429): the mart API allows only ~10 quick calls. \
             Wait a minute and retry; cached results are served without calls.",
        ),
        ErrorKind::Api if e.message.contains("Recaptcha") => Error::new(
            ErrorKind::Api,
            "GrabMart rejected the reCAPTCHA token (403 Recaptcha failed). Reload \
             mart.grab.com in the CDP browser and retry.",
        ),
        _ => e,
    }
}

/// A mart login/401 failure, worded for the user.
fn mart_auth_err(e: Error) -> Error {
    // `auth_missing` here is the one-time consent screen; its message says
    // what to do.
    if e.kind != ErrorKind::AuthExpired {
        return e;
    }
    Error::new(
        ErrorKind::AuthExpired,
        format!(
            "GrabMart quotes need a logged-in Grab session and the silent GrabMart login \
             failed ({}). Run `grab auth login` (browsing still works without it).",
            e.message
        ),
    )
}

impl Mart {
    /// Build a client over `ctx`, registering the mart allowlist (the order
    /// URL only when `allow_ordering` is set). The context's cache mode
    /// applies to browse calls; use [`CacheMode::Off`](grabcli_core::CacheMode)
    /// for the quote/order path.
    pub fn new(ctx: Context) -> Result<Self> {
        let allow = api::allowlist(ctx.config.allow_ordering);
        let ctx = ctx.with_allowlist(&allow)?;
        let loc = Location::from_session(ctx.session())?;
        let cdp = ctx.cdp_base();
        Ok(Self {
            ctx,
            loc,
            cdp,
            pacer: Arc::new(MemoryPacer::default()),
            notices: false,
        })
    }

    /// Use another [`Pacer`] (e.g. one shared across processes).
    #[must_use]
    pub fn with_pacer(mut self, pacer: Arc<dyn Pacer>) -> Self {
        self.pacer = pacer;
        self
    }

    /// A cache key in the mart namespace at the saved delivery location.
    pub fn key(&self, kind: grabcli_core::cache::CacheKind, endpoint: &str) -> CacheKey {
        CacheKey::new(CACHE_NS, kind, endpoint).location(self.loc.latitude, self.loc.longitude)
    }

    /// GET a public endpoint through the cache. A token is minted only when
    /// the cache misses.
    pub async fn public(&mut self, key: &CacheKey, url: &str, action: &str) -> Result<Value> {
        let cache = self.ctx.cache().clone();
        cache
            .get_or_fetch(key, || self.fetch_public(url, action))
            .await
    }

    /// GET a public endpoint live (never cached): mint a token, pace, send;
    /// retry once on 429.
    pub async fn fetch_public(&mut self, url: &str, action: &str) -> Result<Value> {
        let mut attempt = 0;
        loop {
            self.pacer.wait().await;
            let token = recaptcha::mint(&self.cdp, action).await?;
            let mut req = Request::get(url, Hydra::NotNeeded);
            req.extra_headers = mart_headers();
            req.extra_headers.push(("x-recaptcha-token".into(), token));
            match self.ctx.call(&req).await {
                Err(e) if e.kind == ErrorKind::RateLimited && attempt == 0 => {
                    attempt += 1;
                    if self.notices {
                        eprintln!(
                            "GrabMart rate limit (429); retrying once in {}s…",
                            RETRY_AFTER_429.as_secs()
                        );
                    }
                    tokio::time::sleep(RETRY_AFTER_429).await;
                }
                other => return other.map_err(map_err),
            }
        }
    }

    /// Log in to GrabMart silently if the session has no mart web session
    /// yet. Returns whether a login was done now. Call it before spending
    /// API calls on a quote.
    pub async fn ensure_mart_session(&mut self) -> Result<bool> {
        if self.ctx.has_mart_session() {
            return Ok(false);
        }
        self.ctx.login_mart().await.map_err(mart_auth_err)?;
        Ok(true)
    }

    /// `POST /protected/cart` (price a cart). Logged-in session + hydra; no
    /// reCAPTCHA. Never cached.
    ///
    /// The protected mart API wants GrabMart's own web session (its own
    /// GrabID client), not food's `gfc_session`: if there is none, or the
    /// call gets a 401, log in to mart silently through the SSO cookie and
    /// (after a 401) price once more. A quote never places an order, so one
    /// retry is safe; food's refresh is not used here.
    pub async fn quote_call(&mut self, body: Value) -> Result<Value> {
        let mut mart_login_done = self.ensure_mart_session().await?;
        loop {
            self.pacer.wait().await;
            let mut req = Request::post(api::CART, body.clone(), Hydra::Required).no_refresh();
            req.extra_headers = mart_headers();
            match self.ctx.call(&req).await {
                Err(e) if e.kind == ErrorKind::AuthExpired && !mart_login_done => {
                    mart_login_done = true;
                    self.ctx.login_mart().await.map_err(mart_auth_err)?;
                }
                Err(e) if e.kind == ErrorKind::AuthExpired => return Err(mart_auth_err(e)),
                other => return other.map_err(map_err),
            }
        }
    }

    /// Default payment method id from config or session.
    pub fn payment_default(&self) -> String {
        self.ctx
            .config
            .payment_type_id
            .clone()
            .or_else(|| self.ctx.session().payment_type_id.clone())
            .unwrap_or_default()
    }

    /// Whether the saved session still has a usable web login (for hints).
    pub fn logged_in(&self) -> bool {
        self.ctx
            .session()
            .gfc_session_exp()
            .is_some_and(|e| e > jwt::now())
    }
}
