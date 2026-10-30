//! The runtime context shared by every command.
//!
//! Owns the loaded config, session and HTTP client, the output format, and the
//! hydra refresh policy. Service code calls [`Context::call`] with a prepared
//! request; the context attaches the hydra token, and on a hydra rejection it
//! re-mints once (native or via CDP, per config) and retries.
//!
//! It also owns the response [`Cache`]; read-only browse calls go through
//! [`Context::cached_call`]. Never route cart/quote/order/auth calls through
//! the cache (see the safety notes in [`crate::cache`]).

use std::sync::Arc;

use serde_json::Value;

use crate::cache::{Cache, CacheKey, CacheMode, Fetched};
use crate::config::{Config, DEFAULT_CDP, HydraMode};
use crate::error::{Error, ErrorKind, Result};
use crate::http::{Allowlist, Client, Hydra, Request};
use crate::jwt;
use crate::output::Output;
use crate::paths;
use crate::session::{LOCATION_COOKIE, Session};
use crate::{cdp, hydra, sso};

/// How hydra should be minted, resolved from config + available key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedHydra {
    Native,
    Cdp,
}

pub struct Context {
    pub config: Config,
    pub out: Output,
    session: Session,
    client: Arc<Client>,
    /// The service allowlist, kept so the client can be rebuilt after a refresh.
    allow: Allowlist,
    /// Skew applied when deciding if the stored hydra token is still fresh.
    hydra_skew: i64,
    /// Response cache for read-only browse data.
    cache: Cache,
    /// Whether a silent session refresh was already tried by this process
    /// (it's tried at most once, proactively or after a 401).
    refresh_tried: bool,
}

/// Refresh proactively when the logged-in `gfc_session` has less than this
/// left (it lives 2 h).
const REFRESH_MARGIN_SECS: i64 = 15 * 60;

impl Context {
    /// Build a context for a command that makes Grab calls. `allow` is the
    /// service's URL allowlist.
    pub fn new(out: Output, allow: Allowlist) -> Result<Self> {
        let config = Config::load(&paths::config_path()?)?;
        let session = Session::load_default()?;
        let client = Arc::new(Client::new(&session, &config, allow.clone())?);
        let cache = Cache::from_config(&config.cache, CacheMode::Normal);
        Ok(Self {
            cache,
            config,
            out,
            session,
            client,
            allow,
            hydra_skew: 30,
            refresh_tried: false,
        })
    }

    /// Refresh the short-lived `gfc_session` silently through the SSO cookie
    /// ([`crate::sso`]), persist it and rebuild the HTTP client. Fails with
    /// `auth_expired` ("run `grab auth login`") when the SSO is dead. Never
    /// triggers an OTP or push.
    pub async fn refresh_session(&mut self) -> Result<()> {
        self.refresh_tried = true;
        let ua = self
            .config
            .user_agent
            .clone()
            .or_else(|| self.session.user_agent.clone())
            .unwrap_or_else(|| crate::http::DEFAULT_UA.to_string());
        sso::silent_refresh(&mut self.session, &ua).await?;
        self.session.save_default()?;
        self.client = Arc::new(Client::new(
            &self.session,
            &self.config,
            self.allow.clone(),
        )?);
        Ok(())
    }

    /// Copy only the saved delivery address (`location` cookie) from the
    /// browser's default context at `cdp_base` into the session, leaving
    /// every other cookie (auth included) alone. `Ok(false)` if the browser
    /// has no `location` cookie. The value is never printed.
    pub async fn restore_address_from_browser(&mut self, cdp_base: &str) -> Result<bool> {
        let (cookies, _) = cdp::default_context_cookies(cdp_base).await?;
        if !self
            .session
            .replace_named_cookies(LOCATION_COOKIE, &cookies)
        {
            return Ok(false);
        }
        self.session.save_default()?;
        self.client = Arc::new(Client::new(
            &self.session,
            &self.config,
            self.allow.clone(),
        )?);
        Ok(true)
    }

    /// Replace the saved delivery address (`location` cookie) with `loc`
    /// (the cookie's JSON) and persist the session. Local only: the cookie
    /// is what the web app itself writes; nothing is sent to Grab.
    pub fn set_location(&mut self, loc: &Value) -> Result<()> {
        let json = serde_json::to_string(loc).map_err(|e| Error::parse(e.to_string()))?;
        let value = urlencoding::encode(&json).into_owned();
        self.session.set_location_cookie(&value);
        self.session.save_default()?;
        self.client = Arc::new(Client::new(
            &self.session,
            &self.config,
            self.allow.clone(),
        )?);
        Ok(())
    }

    /// Best-effort follow-up after a login or refresh saved a session: fill
    /// in a missing saved address from the browser, and mint hydra so the
    /// next quote doesn't have to. Never fails; returns notes for the user.
    pub async fn after_login(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.session.cookie(LOCATION_COOKIE).is_none() {
            let base = self.cdp_base();
            match self.restore_address_from_browser(&base).await {
                Ok(true) => notes.push("copied the saved delivery address from the browser".into()),
                Ok(false) => notes.push(
                    "no saved delivery address in the session or the browser; set one on \
                     food.grab.com, then `grab auth import --address-only`"
                        .into(),
                ),
                Err(_) => notes.push(
                    "no saved delivery address and the browser was unreachable; later run \
                     `grab auth import --address-only`"
                        .into(),
                ),
            }
        }
        if !self.has_mart_session()
            && let Err(e) = self.login_mart().await
        {
            notes.push(if e.kind == ErrorKind::AuthMissing {
                format!("GrabMart: {}", e.message)
            } else {
                format!(
                    "GrabMart session not set up now ({}); `grab mart quote` retries it",
                    e.kind.as_str()
                )
            });
        }
        if !self.session.hydra_valid(self.hydra_skew)
            && let Err(e) = self.mint_hydra().await
        {
            notes.push(format!(
                "hydra not minted now ({}); commands mint it on demand",
                e.kind.as_str()
            ));
        }
        notes
    }

    /// Log in to GrabMart's own web client silently through the SSO cookie
    /// ([`sso::Service::Mart`]): the protected mart API doesn't accept
    /// food's `gfc_session`. Persists the session and rebuilds the client.
    /// Never triggers an OTP or push.
    pub async fn login_mart(&mut self) -> Result<()> {
        let ua = self
            .config
            .user_agent
            .clone()
            .or_else(|| self.session.user_agent.clone())
            .unwrap_or_else(|| crate::http::DEFAULT_UA.to_string());
        sso::silent_login(sso::Service::Mart, &mut self.session, &ua).await?;
        self.session.save_default()?;
        self.client = Arc::new(Client::new(
            &self.session,
            &self.config,
            self.allow.clone(),
        )?);
        Ok(())
    }

    /// Whether the session has a GrabMart web session cookie.
    pub fn has_mart_session(&self) -> bool {
        !sso::mart_session_cookies(&self.session).is_empty()
    }

    /// Whether the logged-in `gfc_session` is about to expire (or has).
    /// A missing/anonymous one is left to the 401 path, so commands that
    /// don't need a login don't pay for a refresh attempt.
    fn session_expiring(&self) -> bool {
        self.session.gfc_logged_in()
            && self
                .session
                .gfc_expires_at()
                .is_some_and(|e| e - jwt::now() < REFRESH_MARGIN_SECS)
    }

    /// Set the cache mode (from `--no-cache` / `--refresh`). `GRAB_NO_CACHE`
    /// and `[cache] enabled = false` still force it off.
    #[must_use]
    pub fn with_cache_mode(mut self, mode: CacheMode) -> Self {
        self.cache = Cache::from_config(&self.config.cache, mode);
        self
    }

    /// The response cache. Clone it to call [`Cache::get_or_fetch`] with a
    /// closure that borrows this context.
    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    /// [`call`](Self::call) through the cache: serve a fresh entry for `key`,
    /// else make the call and store the raw response. Only for read-only
    /// browse requests — never carts, quotes, orders, tracking or auth.
    pub async fn cached_call(&mut self, key: &CacheKey, req: &Request) -> Result<Value> {
        let cache = self.cache.clone();
        cache.get_or_fetch(key, || self.call(req)).await
    }

    /// [`cached_call`](Self::cached_call) that also reports whether the
    /// response came from the cache and when it was fetched (for "cached 4m
    /// ago" markers).
    pub async fn cached_call_meta(
        &mut self,
        key: &CacheKey,
        req: &Request,
    ) -> Result<Fetched<Value>> {
        let cache = self.cache.clone();
        cache.get_or_fetch_meta(key, || self.call(req)).await
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn cdp_base(&self) -> String {
        self.config
            .cdp
            .clone()
            .unwrap_or_else(|| DEFAULT_CDP.to_string())
    }

    fn resolve_hydra_mode(&self) -> ResolvedHydra {
        let key_present = self.hydra_key_pem().is_some();
        match self.config.hydra_mode.unwrap_or_default() {
            HydraMode::Native => ResolvedHydra::Native,
            HydraMode::Cdp => ResolvedHydra::Cdp,
            HydraMode::Auto => {
                if key_present {
                    ResolvedHydra::Native
                } else {
                    ResolvedHydra::Cdp
                }
            }
        }
    }

    fn hydra_key_path(&self) -> Option<std::path::PathBuf> {
        if let Some(p) = &self.config.hydra_key {
            return Some(p.clone());
        }
        let default = paths::config_dir().ok()?.join("hydra-key.pem");
        default.exists().then_some(default)
    }

    fn hydra_key_pem(&self) -> Option<String> {
        std::fs::read_to_string(self.hydra_key_path()?).ok()
    }

    /// Ensure a usable hydra token exists, minting/refreshing if needed and
    /// allowed. Persists the refreshed session. Returns the token.
    pub async fn ensure_hydra(&mut self) -> Result<String> {
        if self.session.hydra_valid(self.hydra_skew)
            && let Some(tok) = &self.session.hydra_jwt
        {
            return Ok(tok.clone());
        }
        self.mint_hydra().await
    }

    /// Mint a fresh hydra token regardless of the current one.
    pub async fn mint_hydra(&mut self) -> Result<String> {
        match self.resolve_hydra_mode() {
            ResolvedHydra::Native => {
                let pem = self.hydra_key_pem().ok_or_else(|| {
                    Error::new(
                        ErrorKind::HydraExpired,
                        "native hydra minting needs a signing key at \
                         <config>/hydra-key.pem (see docs/hydra.md); or set \
                         hydra_mode = \"cdp\"",
                    )
                })?;
                let ver = hydra::DEFAULT_VER;
                let (tok, exp) = hydra::mint_native(&pem, &hydra::DeviceFields::random(), ver)?;
                self.session.hydra_jwt = Some(tok.clone());
                self.session.hydra_exp = Some(exp);
                self.session.save_default()?;
                Ok(tok)
            }
            ResolvedHydra::Cdp => {
                let base = self.cdp_base();
                let page = self
                    .config
                    .hydra_page
                    .clone()
                    .unwrap_or_else(|| "https://food.grab.com/ph/en/".to_string());
                cdp::mint_hydra(&base, &page, &mut self.session).await?;
                self.session.save_default()?;
                self.session.hydra_jwt.clone().ok_or_else(|| {
                    Error::new(ErrorKind::HydraExpired, "hydra mint produced no token")
                })
            }
        }
    }

    /// Make an allowlisted call, attaching hydra when required and retrying
    /// once if Grab rejects the token (and `auto_refresh` is on).
    pub async fn call(&mut self, req: &Request) -> Result<Value> {
        // Proactive, best-effort: if it fails the request still goes out and
        // a 401 reports the real problem.
        if self.config.auto_refresh() && !self.refresh_tried && self.session_expiring() {
            let _ = self.refresh_session().await;
        }
        let token = if req.hydra == Hydra::Required {
            Some(self.ensure_hydra().await?)
        } else {
            None
        };

        match self.client.send(req, token.as_deref()).await {
            Err(e)
                if e.kind == ErrorKind::HydraExpired
                    && req.hydra == Hydra::Required
                    && self.config.auto_refresh() =>
            {
                // Re-mint once and retry.
                let fresh = self.mint_hydra().await?;
                self.client.send(req, Some(&fresh)).await
            }
            Err(e)
                if e.kind == ErrorKind::AuthExpired
                    && req.refreshable
                    && !self.refresh_tried
                    && self.config.auto_refresh() =>
            {
                // The short-lived session expired: refresh it from the
                // long-lived cookies and retry the request exactly once. Never
                // taken for non-idempotent calls (`refreshable == false`).
                self.refresh_session().await?;
                let token = if req.hydra == Hydra::Required {
                    Some(self.ensure_hydra().await?)
                } else {
                    None
                };
                self.client.send(req, token.as_deref()).await
            }
            other => other,
        }
    }

    /// Status snapshot for `grab auth status` — cookie names and expiries, never
    /// cookie values. The saved unit and driver note are shown to the user.
    pub fn auth_status(&self) -> Value {
        let now = jwt::now();
        let loc = crate::location::Location::from_session(&self.session).ok();
        let cookies: Vec<Value> = self
            .session
            .cookies
            .iter()
            .map(|c| {
                let exp = if c.expires > 0.0 {
                    Some(c.expires as i64)
                } else {
                    None
                };
                serde_json::json!({
                    "name": c.name,
                    "domain": c.domain,
                    "expires": exp,
                    "expires_in_secs": exp.map(|e| e - now),
                    "expired": exp.is_some_and(|e| e <= now),
                })
            })
            .collect();
        serde_json::json!({
            "session_path": paths::session_path().ok().map(|p| p.display().to_string()),
            "cookies": cookies,
            "gfc_session_present": self.session.cookie("gfc_session").is_some(),
            "gfc_session_logged_in": self.session.gfc_logged_in(),
            "gfc_session_exp": self.session.gfc_expires_at(),
            "gfc_session_expires_in_secs": self.session.gfc_expires_at().map(|e| e - now),
            "hydra_exp": self.session.hydra_exp,
            "hydra_expires_in_secs": self.session.hydra_exp.map(|e| e - now),
            "hydra_valid": self.session.hydra_valid(self.hydra_skew),
            "web_app_version_present": self.session.web_app_version.is_some(),
            "payment_type_id_present":
                self.session.payment_type_id.is_some() || self.config.payment_type_id.is_some(),
            "has_saved_address": self.session.cookie("location").is_some(),
            "address_detail": loc.as_ref().and_then(|l| l.address_detail.clone()),
            "note_to_driver": loc.as_ref().and_then(|l| l.note_to_driver.clone()),
        })
    }
}
