//! Silent session refresh through the GrabID SSO cookie (`grab auth refresh`).
//!
//! `gfc_session` (food.grab.com, 2 h) carries no refresh token, and
//! `POST /auth/access-token` only echoes the token of a live `gfc_session`
//! (docs/login.md §1). The only way to a fresh `gfc_session` without a
//! challenge is the browser's own silent OAuth round trip, done here over
//! plain HTTP in ONE cookie jar so the PKCE state food stores in its
//! anonymous `gfc_session` is carried to the callback:
//!
//! 1. `GET food.grab.com/auth/login` → 302 to `…/oauth2/authorize` and a new
//!    anonymous `gfc_session` holding the PKCE verifier/state/nonce.
//! 2. `GET authorize` with the `.grab.com` SSO cookie
//!    (`grabid-openid-authn-ck`):
//!    - 302 to `food.grab.com/auth/callback?code&state` → step 3;
//!    - 302 to weblogin with `auth_cookie_exists=true` (the "Continue as …"
//!      screen) → re-issue authorize with `user_selection_done=true&
//!      remember_me=true`, as that button does (a top-level navigation, no
//!      captcha involved) → expect the callback;
//!    - weblogin without `auth_cookie_exists` → the SSO session is dead:
//!      `auth_expired`, run `grab auth login`.
//! 3. `GET callback` → food exchanges the code server-side and sets the
//!    logged-in `gfc_session`.
//!
//! GrabMart has its own OAuth client (`mart.grab.com/grab-id/login` →
//! `authorize` → `mart.grab.com/grab-id/token`); [`silent_login`] runs the
//! same round trip for it ([`Service::Mart`]). A consent screen
//! (`weblogin.grab.com/consent`) stops the flow: only the user may approve it.
//!
//! No challenge (OTP, push, captcha) can be triggered by this path. Every hop
//! is checked against a fixed allowlist; redirects are never followed blindly.
//! Nothing here logs cookie values, codes or tokens.

use std::time::Duration;

use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderValue};

use crate::error::{Error, ErrorKind, Result};
use crate::http::{Allowlist, parse_set_cookie};
use crate::session::Session;

pub const FOOD_LOGIN: &str = "https://food.grab.com/auth/login";
pub const FOOD_CALLBACK: &str = "https://food.grab.com/auth/callback";
/// GrabMart's own OAuth client (scope `martweb-protected`): the protected mart
/// API doesn't accept food's `gfc_session`.
pub const MART_LOGIN: &str = "https://mart.grab.com/grab-id/login";
pub const MART_CALLBACK: &str = "https://mart.grab.com/grab-id/token";

/// A web app with its own GrabID OAuth client that the SSO cookie can log in
/// silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Food,
    Mart,
}

impl Service {
    pub fn login_url(self) -> &'static str {
        match self {
            Self::Food => FOOD_LOGIN,
            Self::Mart => MART_LOGIN,
        }
    }

    pub fn callback_url(self) -> &'static str {
        match self {
            Self::Food => FOOD_CALLBACK,
            Self::Mart => MART_CALLBACK,
        }
    }

    fn host(self) -> &'static str {
        match self {
            Self::Food => "food.grab.com",
            Self::Mart => "mart.grab.com",
        }
    }
}

/// Cookies the mart login endpoints set that are *not* a session (bookkeeping
/// for the OAuth round trip and routing).
const MART_NON_SESSION: &[&str] = &[
    "x_forwarded_host",
    "grabid_login_params",
    "subpath",
    "last_supported_subpath",
    "user_place",
    "_dd_s",
    "_gsvid",
];

/// Names of the mart.grab.com session cookies in `session` (whatever the
/// mart callback set, minus known bookkeeping cookies). Names only.
pub fn mart_session_cookies(session: &Session) -> Vec<&str> {
    let now = crate::jwt::now() as f64;
    session
        .cookies
        .iter()
        .filter(|c| c.domain.trim_start_matches('.') == "mart.grab.com")
        .filter(|c| !MART_NON_SESSION.contains(&c.name.as_str()))
        .filter(|c| c.expires <= 0.0 || c.expires > now)
        .map(|c| c.name.as_str())
        .collect()
}
/// The SSO cookie the IdP reads on `authorize`.
pub const SSO_COOKIE: &str = "grabid-openid-authn-ck";

/// The only URLs the refresh may touch.
pub fn allowlist() -> Allowlist {
    let mut a = Allowlist::new();
    a.allow_exact(FOOD_LOGIN)
        .allow_exact(FOOD_CALLBACK)
        .allow_exact(MART_LOGIN)
        .allow_exact(MART_CALLBACK)
        .allow_exact("https://partner-api.grab.com/grabid/v1/oauth2/authorize")
        .allow_exact("https://api.grab.com/grabid/v1/oauth2/authorize");
    a
}

/// Where a redirect points during the silent round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hop {
    /// GrabID `oauth2/authorize`.
    Authorize,
    /// The service's callback (`food.grab.com/auth/callback?code=…`,
    /// `mart.grab.com/grab-id/token?code=…`): the IdP accepted the SSO.
    Callback,
    /// The weblogin "Continue as …" screen: the IdP sees a live SSO cookie but
    /// wants the account picked.
    WebLoginWithSso,
    /// The weblogin phone screen: no live SSO, a real login is needed.
    WebLoginNoSso,
    /// weblogin's OAuth consent screen: the SSO is fine but the user never
    /// granted this client (e.g. GrabMart web) access. Needs a person once.
    Consent,
    /// food.grab.com past the callback (logged in).
    Food,
    Other(String),
}

pub fn classify_hop(url: &Url) -> Hop {
    classify_hop_for(Service::Food, url)
}

/// [`classify_hop`] for `service`'s callback.
pub fn classify_hop_for(service: Service, url: &Url) -> Hop {
    let host = url.host_str().unwrap_or_default();
    let path = url.path();
    if service == Service::Mart {
        if host == "mart.grab.com" && path == "/grab-id/token" {
            return Hop::Callback;
        }
        if host == "mart.grab.com" || host == "food.grab.com" {
            return Hop::Other(format!("{host}{path}"));
        }
    }
    let has = |k: &str, v: &str| url.query_pairs().any(|(a, b)| a == k && b == v);
    match host {
        "partner-api.grab.com" | "api.grab.com" if path == "/grabid/v1/oauth2/authorize" => {
            Hop::Authorize
        }
        "food.grab.com" if path == "/auth/callback" => Hop::Callback,
        "food.grab.com" if !path.starts_with("/auth/") => Hop::Food,
        "weblogin.grab.com" if path.starts_with("/consent") => Hop::Consent,
        "weblogin.grab.com" if has("auth_cookie_exists", "true") => Hop::WebLoginWithSso,
        "weblogin.grab.com" => Hop::WebLoginNoSso,
        _ => Hop::Other(format!("{host}{path}")),
    }
}

/// `authorize` URL plus what the "Continue as …" button adds.
pub fn with_account_selected(authorize: &Url) -> Url {
    let mut u = authorize.clone();
    let keep: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| k != "user_selection_done" && k != "remember_me")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    u.query_pairs_mut()
        .clear()
        .extend_pairs(keep)
        .append_pair("user_selection_done", "true")
        .append_pair("remember_me", "true");
    u
}

fn dead_sso() -> Error {
    Error::new(
        ErrorKind::AuthExpired,
        "the Grab SSO login is no longer valid (expired or logged out), so the session can't be \
         refreshed silently; log in again with `grab auth login --phone +63…` (preview it with --dry-run first)",
    )
}

fn consent_needed(service: Service) -> Error {
    Error::new(
        ErrorKind::AuthMissing,
        format!(
            "Grab wants a one-time consent before {host} can use this login (the SSO itself is \
             fine). In the logged-in browser, open {login} and approve it, then retry. grabcli \
             never approves consent screens for you.",
            host = service.host(),
            login = service.login_url(),
        ),
    )
}

/// One cookie jar + a no-redirect client.
struct Jar<'a> {
    http: reqwest::Client,
    session: &'a mut Session,
    ua: &'a str,
    allow: Allowlist,
}

impl Jar<'_> {
    /// GET `url` (allowlisted, https, grab.com), absorb `Set-Cookie`, return
    /// the status and the resolved `Location`, if any.
    async fn hop(&mut self, url: &Url) -> Result<(u16, Option<Url>)> {
        if url.scheme() != "https" || !self.allow.is_allowed(url.as_str()) {
            return Err(Error::new(
                ErrorKind::UrlNotAllowed,
                format!(
                    "refresh refused to follow {}{}",
                    url.host_str().unwrap_or_default(),
                    url.path()
                ),
            ));
        }
        let mut h = HeaderMap::new();
        let mut put = |k: &'static str, v: &str| {
            if let Ok(v) = HeaderValue::from_str(v) {
                h.insert(k, v);
            }
        };
        put("user-agent", self.ua);
        put("accept", "text/html,application/xhtml+xml,*/*;q=0.8");
        put("accept-language", "en");
        let cookie = self.session.cookie_header_for(url.as_str());
        if !cookie.is_empty() {
            put("cookie", &cookie);
        }
        let resp = self
            .http
            .get(url.clone())
            .headers(h)
            .send()
            .await
            .map_err(|e| Error::new(ErrorKind::Network, format!("refresh: {e}")))?;
        let host = url.host_str().unwrap_or_default().to_string();
        for raw in resp
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
        {
            if let Some(c) = parse_set_cookie(raw, &host) {
                self.session.apply_set_cookie(&c);
            }
        }
        let status = resp.status().as_u16();
        if std::env::var_os("GRABCLI_TRACE_SSO").is_some() {
            let keys: Vec<String> = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|l| url.join(l).ok())
                .map(|u| {
                    let q: Vec<String> = u.query_pairs().map(|(k, _)| k.into_owned()).collect();
                    vec![format!(
                        "{}{} [{}]",
                        u.host_str().unwrap_or_default(),
                        u.path(),
                        q.join(",")
                    )]
                })
                .unwrap_or_default();
            eprintln!(
                "TRACE {}{} -> {status} {keys:?}",
                url.host_str().unwrap_or_default(),
                url.path()
            );
        }
        let loc = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|l| url.join(l).ok());
        Ok((status, loc))
    }
}

/// Run the silent round trip for food. On success `session` holds a
/// logged-in `gfc_session` (and any re-issued SSO cookies). On failure
/// `session` is left untouched.
pub async fn silent_refresh(session: &mut Session, ua: &str) -> Result<()> {
    silent_login(Service::Food, session, ua).await
}

/// Run the silent SSO round trip for `service` (see the module docs): its
/// login URL → GrabID `authorize` (with the SSO cookie) → its callback, all in
/// one cookie jar. Never sends a challenge. On failure `session` is left
/// untouched.
pub async fn silent_login(service: Service, session: &mut Session, ua: &str) -> Result<()> {
    let has_sso = session.cookies.iter().any(|c| {
        c.name == SSO_COOKIE && (c.expires <= 0.0 || c.expires > crate::jwt::now() as f64)
    });
    if !has_sso {
        return Err(Error::new(
            ErrorKind::AuthExpired,
            "no Grab SSO cookie in the session, so it can't be refreshed silently; log in with `grab auth login --phone +63…` (preview it with --dry-run first)",
        ));
    }
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .gzip(true)
        .build()
        .map_err(|e| Error::new(ErrorKind::Network, e.to_string()))?;
    let mut work = session.clone();
    let mut jar = Jar {
        http,
        session: &mut work,
        ua,
        allow: allowlist(),
    };
    let classify = |u: &Url| classify_hop_for(service, u);

    let start = Url::parse(service.login_url()).map_err(|e| Error::parse(e.to_string()))?;
    let (_, loc) = jar.hop(&start).await?;
    let authorize = loc
        .filter(|u| classify(u) == Hop::Authorize)
        .ok_or_else(|| {
            Error::new(
                ErrorKind::Api,
                format!("{} login did not redirect to GrabID", service.host()),
            )
        })?;

    let (_, loc) = jar.hop(&authorize).await?;
    let mut next =
        loc.ok_or_else(|| Error::new(ErrorKind::Api, "GrabID authorize did not redirect"))?;
    match classify(&next) {
        Hop::Callback => {}
        Hop::WebLoginWithSso => {
            let (_, loc) = jar.hop(&with_account_selected(&authorize)).await?;
            next = loc.ok_or_else(dead_sso)?;
            match classify(&next) {
                Hop::Callback => {}
                Hop::Consent => return Err(consent_needed(service)),
                _ => return Err(dead_sso()),
            }
        }
        Hop::WebLoginNoSso => return Err(dead_sso()),
        Hop::Consent => return Err(consent_needed(service)),
        other => {
            return Err(Error::new(
                ErrorKind::Api,
                format!("GrabID authorize redirected somewhere unexpected ({other:?})"),
            ));
        }
    }

    let (status, loc) = jar.hop(&next).await?;
    let landed = loc.as_ref().map(classify);
    let ok = match service {
        Service::Food => jar.session.gfc_logged_in(),
        Service::Mart => {
            (200..400).contains(&status)
                && !mart_session_cookies(jar.session).is_empty()
                && !loc.as_ref().is_some_and(|u| {
                    u.path().starts_with("/grab-id/") || u.path().contains("login-fail")
                })
        }
    };
    if !ok {
        return Err(Error::new(
            ErrorKind::AuthExpired,
            format!(
                "{} {} did not log in (HTTP {status}, next {landed:?}); run `grab auth login`",
                service.host(),
                Url::parse(service.callback_url())
                    .map(|u| u.path().to_string())
                    .unwrap_or_default(),
            ),
        ));
    }
    *session = work;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).expect("test url")
    }

    #[test]
    fn classifies_hops() {
        assert_eq!(
            classify_hop(&u(
                "https://partner-api.grab.com/grabid/v1/oauth2/authorize?client_id=x"
            )),
            Hop::Authorize
        );
        assert_eq!(
            classify_hop(&u("https://food.grab.com/auth/callback?code=C&state=S")),
            Hop::Callback
        );
        assert_eq!(
            classify_hop(&u(
                "https://weblogin.grab.com/passenger/login?ctx_id=1&auth_cookie_exists=true"
            )),
            Hop::WebLoginWithSso
        );
        assert_eq!(
            classify_hop(&u("https://weblogin.grab.com/passenger/login?ctx_id=1")),
            Hop::WebLoginNoSso
        );
        assert_eq!(classify_hop(&u("https://food.grab.com/ph/en/")), Hop::Food);
        assert!(matches!(
            classify_hop(&u("https://evil.example/auth/callback")),
            Hop::Other(_)
        ));
    }

    #[test]
    fn account_selection_params_are_added_once() {
        let a = u(
            "https://partner-api.grab.com/grabid/v1/oauth2/authorize?client_id=c&state=s&remember_me=false",
        );
        let b = with_account_selected(&a);
        let pairs: Vec<(String, String)> = b
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert!(pairs.contains(&("client_id".into(), "c".into())));
        assert!(pairs.contains(&("state".into(), "s".into())));
        assert_eq!(pairs.iter().filter(|(k, _)| k == "remember_me").count(), 1);
        assert!(pairs.contains(&("remember_me".into(), "true".into())));
        assert!(pairs.contains(&("user_selection_done".into(), "true".into())));
    }

    #[test]
    fn classifies_mart_hops() {
        let m = |s: &str| classify_hop_for(Service::Mart, &u(s));
        assert_eq!(
            m("https://mart.grab.com/grab-id/token?code=C&state=S"),
            Hop::Callback
        );
        assert_eq!(
            m("https://partner-api.grab.com/grabid/v1/oauth2/authorize?client_id=x"),
            Hop::Authorize
        );
        assert!(matches!(m("https://mart.grab.com/ph/en"), Hop::Other(_)));
        assert_eq!(m("https://weblogin.grab.com/consent"), Hop::Consent);
        // Food's callback is not mart's.
        assert!(matches!(
            m("https://food.grab.com/auth/callback?code=C"),
            Hop::Other(_)
        ));
        assert_ne!(
            classify_hop(&u("https://mart.grab.com/grab-id/token?code=C")),
            Hop::Callback
        );
    }

    #[test]
    fn mart_session_cookie_names_skip_bookkeeping() {
        let ck = |name: &str, domain: &str| crate::session::Cookie {
            name: name.into(),
            value: "v".into(),
            domain: domain.into(),
            path: "/".into(),
            expires: -1.0,
        };
        let s = Session {
            cookies: vec![
                ck("x_forwarded_host", "mart.grab.com"),
                ck("grabid_login_params", "mart.grab.com"),
                ck("gfc_session", "food.grab.com"),
            ],
            ..Default::default()
        };
        assert!(mart_session_cookies(&s).is_empty());
        let mut s2 = s.clone();
        s2.cookies.push(ck("mart_session", "mart.grab.com"));
        assert_eq!(mart_session_cookies(&s2), vec!["mart_session"]);
    }

    #[test]
    fn allowlist_is_tight() {
        let a = allowlist();
        assert!(a.is_allowed("https://food.grab.com/auth/callback?code=1"));
        assert!(a.is_allowed("https://partner-api.grab.com/grabid/v1/oauth2/authorize?x=1"));
        assert!(a.is_allowed("https://mart.grab.com/grab-id/token?code=1"));
        assert!(!a.is_allowed("https://mart.grab.com/grab-id/logout"));
        assert!(!a.is_allowed("https://food.grab.com/auth/logout"));
        assert!(!a.is_allowed("https://partner-api.grab.com/grabid/v1/authnv4/login"));
        assert!(!a.is_allowed("https://partner-api.grab.com/grabid/v1/oauth2/logout"));
    }

    #[tokio::test]
    async fn refuses_without_sso_cookie() {
        let mut s = Session::default();
        let e = silent_refresh(&mut s, "ua").await.err().map(|e| e.kind);
        assert_eq!(e, Some(ErrorKind::AuthExpired));
    }
}
