//! The saved Grab web session grabcli reads at runtime.
//!
//! It mirrors what a logged-in browser already holds: the site's cookies plus
//! the short-lived hydra device JWT. grabcli never creates these credentials;
//! it imports them from a browser the user is already logged into.
//!
//! Stored as JSON at [`crate::paths::session_path`] with mode 0600. The
//! `Debug` impl redacts every value, and no command ever prints a cookie or
//! token value — only names, expiries and derived status.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorKind, Result};
use crate::jwt;
use crate::paths;

/// Site preference cookies that are not credentials: the saved delivery
/// address (`location`, URL-encoded JSON), the country and the UI language.
/// A fresh login (isolated browser context) doesn't have them, so they are
/// carried over from the previous session.
pub const PREFERENCE_COOKIES: &[&str] = &["location", "gfc_country", "next-i18next"];

/// The saved delivery address cookie.
pub const LOCATION_COOKIE: &str = "location";

fn slash() -> String {
    "/".to_string()
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    #[serde(default = "slash")]
    pub path: String,
    /// Unix seconds; `-1`/`0` for session cookies.
    #[serde(default)]
    pub expires: f64,
}

impl fmt::Debug for Cookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cookie")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .field("domain", &self.domain)
            .finish()
    }
}

/// The on-disk session document. Field names match the browser export helper
/// and the Python reference client so an existing `session.json` imports as-is.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub cookies: Vec<Cookie>,
    /// The hydra device JWT (attached as the `x-hydra-jwt` header).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hydra_jwt: Option<String>,
    /// `exp` of the hydra JWT, Unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hydra_exp: Option<i64>,
    /// The Next.js build id, sent as `x-grab-web-app-version`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_app_version: Option<String>,
    /// Default payment method id captured from the browser, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment_type_id: Option<String>,
    /// The browser's User-Agent at import time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// When the cookies were exported, Unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exported_at: Option<f64>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("cookies", &self.cookies.len())
            .field("hydra_jwt", &self.hydra_jwt.as_ref().map(|_| "<redacted>"))
            .field("hydra_exp", &self.hydra_exp)
            .finish_non_exhaustive()
    }
}

impl Session {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::new(
                    ErrorKind::AuthMissing,
                    format!(
                        "no session at {}: run `grab auth import` (or `--from <session.json>`)",
                        path.display()
                    ),
                ));
            }
            Err(e) => return Err(e.into()),
        };
        serde_json::from_slice(&bytes).map_err(|e| Error::parse(format!("{}: {e}", path.display())))
    }

    /// Load from the default/env path.
    pub fn load_default() -> Result<Self> {
        Self::load(&paths::session_path()?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let data = serde_json::to_vec_pretty(self)?;
        paths::write_private(path, &data)
    }

    pub fn save_default(&self) -> Result<()> {
        self.save(&paths::session_path()?)
    }

    /// Insert or replace a cookie, matched by name, path and domain (a
    /// leading dot is ignored for the match, so `.grab.com` and a legacy
    /// `grab.com` entry are the same cookie). Used to merge `Set-Cookie`s.
    pub fn upsert_cookie(
        &mut self,
        name: &str,
        value: &str,
        domain: &str,
        path: &str,
        expires: f64,
    ) {
        let path = if path.is_empty() { "/" } else { path };
        let same = |c: &Cookie| {
            c.name == name
                && c.path == path
                && c.domain.trim_start_matches('.') == domain.trim_start_matches('.')
        };
        // Drop any duplicates an older parser left behind.
        let mut seen = false;
        self.cookies.retain(|c| {
            if same(c) {
                let keep = !seen;
                seen = true;
                keep
            } else {
                true
            }
        });
        if let Some(c) = self.cookies.iter_mut().find(|c| same(c)) {
            c.value = value.to_string();
            c.domain = domain.to_string();
            c.expires = if expires == 0.0 { -1.0 } else { expires };
        } else {
            self.cookies.push(Cookie {
                name: name.to_string(),
                value: value.to_string(),
                domain: domain.to_string(),
                path: path.to_string(),
                expires: if expires == 0.0 { -1.0 } else { expires },
            });
        }
    }

    /// Remove a cookie (same matching rules as [`upsert_cookie`](Self::upsert_cookie)).
    pub fn remove_cookie(&mut self, name: &str, domain: &str, path: &str) {
        self.cookies.retain(|c| {
            !(c.name == name
                && c.path == path
                && c.domain.trim_start_matches('.') == domain.trim_start_matches('.'))
        });
    }

    /// Apply one parsed `Set-Cookie` (insert, replace, or delete).
    pub fn apply_set_cookie(&mut self, c: &crate::http::RawCookie) {
        if c.deleted {
            self.remove_cookie(&c.name, &c.domain, &c.path);
        } else {
            self.upsert_cookie(&c.name, &c.value, &c.domain, &c.path, c.expires);
        }
    }

    /// Copy the [`PREFERENCE_COOKIES`] that this session lacks (by name)
    /// from `prev`. Never touches auth cookies and never overwrites a
    /// preference the new session already has. Returns the names copied.
    pub fn carry_over_preferences(&mut self, prev: &Session) -> Vec<String> {
        let mut copied = Vec::new();
        for name in PREFERENCE_COOKIES {
            if self.cookie(name).is_some() {
                continue;
            }
            let from: Vec<Cookie> = prev
                .cookies
                .iter()
                .filter(|c| c.name == *name)
                .cloned()
                .collect();
            if !from.is_empty() {
                self.cookies.extend(from);
                copied.push((*name).to_string());
            }
        }
        copied
    }

    /// Replace every cookie called `name` with the ones called `name` in
    /// `source` (e.g. a browser's cookies), leaving all other cookies alone.
    /// Returns whether `source` had any; if not, nothing changes.
    pub fn replace_named_cookies(&mut self, name: &str, source: &[Cookie]) -> bool {
        let new: Vec<Cookie> = source.iter().filter(|c| c.name == name).cloned().collect();
        if new.is_empty() {
            return false;
        }
        self.cookies.retain(|c| c.name != name);
        self.cookies.extend(new);
        true
    }

    /// Set the `location` cookie's value, keeping the existing cookie's
    /// domain, path and expiry (or `food.grab.com`, `/`, one year if none).
    pub fn set_location_cookie(&mut self, value: &str) {
        let (domain, path, expires) = self
            .cookies
            .iter()
            .find(|c| c.name == LOCATION_COOKIE)
            .map(|c| (c.domain.clone(), c.path.clone(), c.expires))
            .unwrap_or_else(|| {
                let year = (crate::jwt::now() + 365 * 24 * 3600) as f64;
                ("food.grab.com".to_string(), "/".to_string(), year)
            });
        self.cookies.retain(|c| c.name != LOCATION_COOKIE);
        self.upsert_cookie(LOCATION_COOKIE, value, &domain, &path, expires);
    }

    /// Value of the first stored cookie called `name` (any domain).
    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.cookies
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.value.as_str())
    }

    /// The `Cookie:` header a browser would send to `url`: only cookies whose
    /// domain and path match, and that haven't expired. Domain cookies are
    /// stored with a leading dot (`.grab.com`); others are host-only.
    pub fn cookie_header_for(&self, url: &str) -> String {
        let Ok(u) = reqwest::Url::parse(url) else {
            return String::new();
        };
        let host = u.host_str().unwrap_or_default().to_ascii_lowercase();
        let path = u.path();
        let now = jwt::now() as f64;
        self.cookies
            .iter()
            .filter(|c| domain_matches(&host, &c.domain))
            .filter(|c| path_matches(path, &c.path))
            .filter(|c| c.expires <= 0.0 || c.expires > now)
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Whether `gfc_session` is a *logged-in* food session (its JWT carries
    /// an access token). Before login, food.grab.com/auth/login sets an
    /// anonymous `gfc_session` that only holds the PKCE verifier and state.
    pub fn gfc_logged_in(&self) -> bool {
        self.cookie("gfc_session")
            .and_then(|t| jwt::payload(t).ok())
            .and_then(|p| {
                p.get("accessToken")
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
            })
            .unwrap_or(false)
    }

    /// The `x-gfc-session` header value: the `sessionKey` claim inside the
    /// `gfc_session` cookie (a JWT).
    pub fn gfc_session_key(&self) -> Result<String> {
        let tok = self.cookie("gfc_session").ok_or_else(|| {
            Error::new(
                ErrorKind::AuthExpired,
                "gfc_session cookie missing: not logged in to food.grab.com; run `grab auth login`",
            )
        })?;
        jwt::payload(tok)?
            .get("sessionKey")
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
            .ok_or_else(|| Error::parse("gfc_session has no sessionKey claim"))
    }

    /// Expiry of the gfc_session cookie's embedded access token (Unix seconds).
    pub fn gfc_session_exp(&self) -> Option<i64> {
        jwt::exp(self.cookie("gfc_session")?)
    }

    /// When the food session stops working: the earlier of the
    /// `gfc_session` cookie's own expiry (`Max-Age=7200`) and its JWT `exp`.
    pub fn gfc_expires_at(&self) -> Option<i64> {
        let cookie = self.cookies.iter().find(|c| c.name == "gfc_session")?;
        let by_cookie = (cookie.expires > 0.0).then_some(cookie.expires as i64);
        match (by_cookie, jwt::exp(&cookie.value)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn hydra_valid(&self, skew_secs: i64) -> bool {
        match (self.hydra_jwt.as_ref(), self.hydra_exp) {
            (Some(_), Some(exp)) => exp - skew_secs > jwt::now(),
            _ => false,
        }
    }
}

/// Cookie domain matching (RFC 6265 §5.1.3): `.grab.com` matches
/// `grab.com` and any subdomain; a host-only `food.grab.com` matches only
/// itself.
pub fn domain_matches(host: &str, cookie_domain: &str) -> bool {
    let d = cookie_domain.to_ascii_lowercase();
    match d.strip_prefix('.') {
        Some(bare) => host == bare || host.ends_with(&format!(".{bare}")),
        None => host == d,
    }
}

/// Cookie path matching (RFC 6265 §5.1.4).
fn path_matches(req_path: &str, cookie_path: &str) -> bool {
    let cp = if cookie_path.is_empty() {
        "/"
    } else {
        cookie_path
    };
    req_path == cp
        || (req_path.starts_with(cp)
            && (cp.ends_with('/') || req_path[cp.len()..].starts_with('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Session {
        // All synthetic: invented cookie values, not from any real account.
        let gfc = format!(
            "{}.{}.sig",
            jwt::b64url(br#"{"alg":"HS256"}"#),
            jwt::b64url(br#"{"sessionKey":"SESS-FAKE","exp":1800000000}"#)
        );
        Session {
            cookies: vec![
                Cookie {
                    name: "gfc_session".into(),
                    value: gfc,
                    domain: "food.grab.com".into(),
                    path: "/".into(),
                    expires: 1_800_000_000.0,
                },
                Cookie {
                    name: "location".into(),
                    value: "%7B%7D".into(),
                    domain: "food.grab.com".into(),
                    path: "/".into(),
                    expires: -1.0,
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn reads_gfc_session_key() -> Result<()> {
        let s = sample();
        assert_eq!(s.gfc_session_key()?, "SESS-FAKE");
        assert_eq!(s.gfc_session_exp(), Some(1_800_000_000));
        Ok(())
    }

    #[test]
    fn cookie_header_joins() {
        let s = sample();
        let h = s.cookie_header_for("https://food.grab.com/proxy/x");
        assert!(h.contains("gfc_session="));
        assert!(h.contains("; location="));
    }

    fn ck(name: &str, domain: &str, expires: f64) -> Cookie {
        Cookie {
            name: name.into(),
            value: format!("{name}-v"),
            domain: domain.into(),
            path: "/".into(),
            expires,
        }
    }

    #[test]
    fn cookie_header_is_per_host() {
        let s = Session {
            cookies: vec![
                ck("sso", ".grab.com", -1.0),
                ck("gfc_session", "food.grab.com", -1.0),
                ck("waf", ".food.grab.com", -1.0),
                ck("mart_only", "mart.grab.com", -1.0),
                ck("old", ".grab.com", 1.0),
            ],
            ..Default::default()
        };
        assert_eq!(
            s.cookie_header_for("https://food.grab.com/auth/login"),
            "sso=sso-v; gfc_session=gfc_session-v; waf=waf-v"
        );
        assert_eq!(
            s.cookie_header_for("https://partner-api.grab.com/grabid/v1/x"),
            "sso=sso-v"
        );
        assert_eq!(
            s.cookie_header_for("https://mart.grab.com/"),
            "sso=sso-v; mart_only=mart_only-v"
        );
        assert_eq!(s.cookie_header_for("https://grab.com.evil.example/"), "");
        assert_eq!(s.cookie_header_for("https://evilgrab.com/"), "");
    }

    #[test]
    fn path_matching() {
        assert!(path_matches("/auth/login", "/"));
        assert!(path_matches("/auth/login", "/auth"));
        assert!(path_matches("/auth", "/auth"));
        assert!(!path_matches("/authx", "/auth"));
        assert!(!path_matches("/", "/auth"));
    }

    #[test]
    fn set_cookie_replaces_instead_of_duplicating() {
        let mut s = Session {
            cookies: vec![ck("passenger_authn_token", ".grab.com", -1.0)],
            ..Default::default()
        };
        let raw = crate::http::parse_set_cookie(
            "passenger_authn_token=NEW; Domain=.grab.com; Path=/; Max-Age=60",
            "partner-api.grab.com",
        )
        .expect("parse");
        s.apply_set_cookie(&raw);
        assert_eq!(s.cookies.len(), 1);
        assert_eq!(s.cookie("passenger_authn_token"), Some("NEW"));
        assert_eq!(s.cookies[0].domain, ".grab.com");

        // A legacy dot-stripped duplicate is collapsed too.
        s.cookies
            .push(ck("passenger_authn_token", "grab.com", -1.0));
        s.apply_set_cookie(&raw);
        assert_eq!(s.cookies.len(), 1);
    }

    #[test]
    fn set_cookie_deletes() {
        let mut s = sample();
        let del = crate::http::parse_set_cookie(
            "gfc_session=; Max-Age=0; Path=/; Expires=Sat, 17 Oct 2026 14:58:04 GMT",
            "food.grab.com",
        )
        .expect("parse");
        s.apply_set_cookie(&del);
        assert!(s.cookie("gfc_session").is_none());
        assert!(s.cookie("location").is_some());
    }

    #[test]
    fn logged_in_needs_access_token() {
        let anon = format!(
            "{}.{}.sig",
            jwt::b64url(br#"{"alg":"HS256"}"#),
            jwt::b64url(br#"{"state":"S","codeVerifier":"V","accessToken":""}"#)
        );
        let mut s = Session {
            cookies: vec![Cookie {
                value: anon,
                ..ck("gfc_session", "food.grab.com", -1.0)
            }],
            ..Default::default()
        };
        assert!(!s.gfc_logged_in());
        s.cookies[0].value = format!(
            "{}.{}.sig",
            jwt::b64url(br#"{"alg":"HS256"}"#),
            jwt::b64url(br#"{"accessToken":"AT","sessionKey":"K"}"#)
        );
        assert!(s.gfc_logged_in());
        assert!(!Session::default().gfc_logged_in());
    }

    #[test]
    fn carry_over_copies_only_missing_preferences() {
        let prev = Session {
            cookies: vec![
                ck("location", "food.grab.com", -1.0),
                ck("gfc_country", "food.grab.com", -1.0),
                ck("next-i18next", "food.grab.com", -1.0),
                ck("gfc_session", "food.grab.com", -1.0),
                ck("grabid-openid-authn-ck", ".grab.com", -1.0),
            ],
            ..Default::default()
        };
        let mut fresh = Session {
            cookies: vec![
                Cookie {
                    value: "NEW".into(),
                    ..ck("gfc_session", "food.grab.com", -1.0)
                },
                Cookie {
                    value: "en-new".into(),
                    ..ck("next-i18next", "food.grab.com", -1.0)
                },
            ],
            ..Default::default()
        };
        let copied = fresh.carry_over_preferences(&prev);
        assert_eq!(copied, vec!["location", "gfc_country"]);
        // Auth cookies come only from the fresh login.
        assert_eq!(fresh.cookie("gfc_session"), Some("NEW"));
        assert!(fresh.cookie("grabid-openid-authn-ck").is_none());
        // An existing preference is not overwritten.
        assert_eq!(fresh.cookie("next-i18next"), Some("en-new"));
        assert_eq!(fresh.cookie("location"), Some("location-v"));
        assert_eq!(fresh.cookies.len(), 4);
        // Idempotent.
        assert!(fresh.carry_over_preferences(&prev).is_empty());
        assert_eq!(fresh.cookies.len(), 4);
    }

    #[test]
    fn carry_over_from_empty_is_noop() {
        let mut s = sample();
        let n = s.cookies.len();
        assert!(s.carry_over_preferences(&Session::default()).is_empty());
        assert_eq!(s.cookies.len(), n);
    }

    #[test]
    fn replace_named_merges_only_that_cookie() {
        let mut s = Session {
            cookies: vec![
                ck("gfc_session", "food.grab.com", -1.0),
                ck("location", "food.grab.com", -1.0),
                ck("passenger_authn_token", ".grab.com", -1.0),
            ],
            ..Default::default()
        };
        let browser = vec![
            Cookie {
                value: "BROWSER-LOC".into(),
                ..ck("location", "food.grab.com", -1.0)
            },
            Cookie {
                value: "BROWSER-GFC".into(),
                ..ck("gfc_session", "food.grab.com", -1.0)
            },
        ];
        assert!(s.replace_named_cookies(LOCATION_COOKIE, &browser));
        assert_eq!(s.cookie("location"), Some("BROWSER-LOC"));
        assert_eq!(s.cookie("gfc_session"), Some("gfc_session-v"));
        assert_eq!(s.cookies.len(), 3);
        // A source without the cookie leaves the session unchanged.
        assert!(!s.replace_named_cookies(LOCATION_COOKIE, &browser[1..]));
        assert_eq!(s.cookie("location"), Some("BROWSER-LOC"));
    }

    #[test]
    fn set_location_keeps_cookie_attrs() {
        let mut s = Session {
            cookies: vec![
                ck("location", "food.grab.com", 1234.0),
                ck("gfc_session", "food.grab.com", -1.0),
            ],
            ..Default::default()
        };
        s.set_location_cookie("NEW-LOC");
        assert_eq!(s.cookie("location"), Some("NEW-LOC"));
        assert_eq!(s.cookies.len(), 2);
        let c = s
            .cookies
            .iter()
            .find(|c| c.name == "location")
            .expect("cookie");
        assert_eq!((c.domain.as_str(), c.expires), ("food.grab.com", 1234.0));

        let mut empty = Session::default();
        empty.set_location_cookie("X");
        let c = &empty.cookies[0];
        assert_eq!((c.domain.as_str(), c.path.as_str()), ("food.grab.com", "/"));
        assert!(c.expires > 0.0);
    }

    #[test]
    fn debug_redacts() {
        let s = sample();
        let d = format!("{s:?}");
        assert!(!d.contains("SESS-FAKE"));
    }

    #[test]
    fn missing_session_is_auth_missing() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let e = Session::load(&dir.path().join("none.json")).expect_err("should error");
        assert_eq!(e.kind, ErrorKind::AuthMissing);
        Ok(())
    }

    #[test]
    fn roundtrip_save_load() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let p = dir.path().join("session.json");
        sample().save(&p)?;
        let back = Session::load(&p)?;
        assert_eq!(back.cookies.len(), 2);
        Ok(())
    }
}
