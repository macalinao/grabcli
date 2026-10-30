//! The shared HTTP client.
//!
//! Every outbound request is checked against an explicit URL allowlist before
//! it is sent. Service crates register the read-only URLs they need. The
//! order-submission URL is deliberately *not* registered unless ordering is
//! enabled in config, so an accidental or injected call to it fails closed with
//! [`ErrorKind::UrlNotAllowed`] instead of reaching Grab.
//!
//! Cookies are sent as a manually built `Cookie:` header (reqwest's cookie
//! store is off), filtered per request host and path the way a browser does.

use std::sync::Arc;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use crate::config::Config;
use crate::error::{Error, ErrorKind, Result};
use crate::session::Session;

pub const DEFAULT_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36";

/// How a URL may be matched by the allowlist.
#[derive(Debug, Clone)]
pub enum Rule {
    /// The request URL (without query) must equal this exactly.
    Exact(String),
    /// The request URL (without query) must start with this prefix. Used for
    /// path-parameterised endpoints such as `.../merchants/<id>`.
    Prefix(String),
}

impl Rule {
    fn matches(&self, url_no_query: &str) -> bool {
        match self {
            Rule::Exact(u) => url_no_query == u,
            Rule::Prefix(p) => url_no_query.starts_with(p),
        }
    }
}

/// A set of allowed URLs. Service crates build one and register their URLs.
#[derive(Debug, Default, Clone)]
pub struct Allowlist {
    rules: Vec<Rule>,
}

impl Allowlist {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn allow_exact(&mut self, url: impl Into<String>) -> &mut Self {
        self.rules.push(Rule::Exact(url.into()));
        self
    }

    pub fn allow_prefix(&mut self, prefix: impl Into<String>) -> &mut Self {
        self.rules.push(Rule::Prefix(prefix.into()));
        self
    }

    /// Add every rule of `other`.
    pub fn extend(&mut self, other: &Allowlist) -> &mut Self {
        self.rules.extend(other.rules.iter().cloned());
        self
    }

    pub fn is_allowed(&self, url: &str) -> bool {
        let no_query = url.split('?').next().unwrap_or(url);
        self.rules.iter().any(|r| r.matches(no_query))
    }
}

/// Whether a call needs the hydra device JWT header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hydra {
    Required,
    NotNeeded,
}

/// A prepared request the client will send if its URL is allowlisted.
pub struct Request {
    pub method: reqwest::Method,
    pub url: String,
    pub body: Option<Value>,
    pub hydra: Hydra,
    /// Extra per-request headers (e.g. `food-web-ua`).
    pub extra_headers: Vec<(String, String)>,
    /// Whether the client may auto-refresh the session and retry this request
    /// once on a 401. False for non-idempotent calls like order submission.
    pub refreshable: bool,
}

impl Request {
    pub fn get(url: impl Into<String>, hydra: Hydra) -> Self {
        Self {
            method: reqwest::Method::GET,
            url: url.into(),
            body: None,
            hydra,
            extra_headers: Vec::new(),
            refreshable: true,
        }
    }

    pub fn post(url: impl Into<String>, body: Value, hydra: Hydra) -> Self {
        Self {
            method: reqwest::Method::POST,
            url: url.into(),
            body: Some(body),
            hydra,
            extra_headers: Vec::new(),
            refreshable: true,
        }
    }

    /// Mark this request as non-retryable: the client will neither auto-refresh
    /// nor resend it on a 401 (use for order submission — no idempotency key).
    pub fn no_refresh(mut self) -> Self {
        self.refreshable = false;
        self
    }
}

/// A cookie parsed from a `Set-Cookie` response header.
#[derive(Debug, Clone)]
pub struct RawCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// Unix seconds; 0 for a session cookie (no Max-Age/Expires we understood).
    pub expires: f64,
    /// `Max-Age<=0` or an `Expires` in the past: the server deleted it.
    pub deleted: bool,
}

/// Shared state for building requests, minus the hydra token (which the
/// higher-level client refreshes).
pub struct Client {
    inner: reqwest::Client,
    allow: Allowlist,
    user_agent: String,
    web_app_version: Option<String>,
    /// The session's cookies; each request gets only those a browser would
    /// send to its host (see [`Session::cookie_header_for`]).
    session: Session,
    /// The `x-gfc-session` value, absent when the session cookie is missing or
    /// expired (so the client can still run a refresh to obtain a fresh one).
    gfc_session_key: Option<String>,
}

impl Client {
    /// Build from a session + config. The service's allowlist is passed in.
    pub fn new(session: &Session, config: &Config, allow: Allowlist) -> Result<Self> {
        let inner = reqwest::Client::builder()
            .gzip(true)
            .build()
            .map_err(Error::from)?;
        let user_agent = config
            .user_agent
            .clone()
            .or_else(|| session.user_agent.clone())
            .unwrap_or_else(|| DEFAULT_UA.to_string());
        Ok(Self {
            inner,
            allow,
            user_agent,
            web_app_version: session.web_app_version.clone(),
            session: session.clone(),
            gfc_session_key: session.gfc_session_key().ok(),
        })
    }

    pub fn allowlist(&self) -> &Allowlist {
        &self.allow
    }

    fn headers(&self, req: &Request, hydra_jwt: Option<&str>) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        let mut set = |k: &'static str, v: &str| -> Result<()> {
            let val = HeaderValue::from_str(v)
                .map_err(|e| Error::invalid(format!("bad header {k}: {e}")))?;
            h.insert(HeaderName::from_static(k), val);
            Ok(())
        };
        set("accept", "application/json, text/plain, */*")?;
        set("accept-language", "en")?;
        set("user-agent", &self.user_agent)?;
        set("origin", "https://food.grab.com")?;
        set("referer", "https://food.grab.com/")?;
        set("x-gfc-country", "PH")?;
        set("x-country-code", "PH")?;
        if let Some(key) = &self.gfc_session_key {
            set("x-gfc-session", key)?;
        }
        let cookie = self.session.cookie_header_for(&req.url);
        if !cookie.is_empty() {
            set("cookie", &cookie)?;
        }
        if let Some(v) = &self.web_app_version {
            set("x-grab-web-app-version", v)?;
        }
        if req.body.is_some() {
            set("content-type", "application/json;charset=UTF-8")?;
        }
        if req.hydra == Hydra::Required {
            let jwt = hydra_jwt.ok_or_else(|| {
                Error::new(ErrorKind::HydraExpired, "this call needs a hydra JWT")
            })?;
            set("x-hydra-jwt", jwt)?;
        }
        for (k, v) in &req.extra_headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| Error::invalid(format!("bad header {k}: {e}")))?;
            let val = HeaderValue::from_str(v)
                .map_err(|e| Error::invalid(format!("bad header value: {e}")))?;
            h.insert(name, val);
        }
        Ok(h)
    }

    /// Send a request. Fails closed if the URL is not allowlisted. Maps HTTP
    /// status and Grab's `invalid hydra jwt` / 401 into typed errors.
    pub async fn send(&self, req: &Request, hydra_jwt: Option<&str>) -> Result<Value> {
        if !self.allow.is_allowed(&req.url) {
            let path = req.url.split('?').next().unwrap_or(&req.url).to_string();
            return Err(Error::new(
                ErrorKind::UrlNotAllowed,
                format!("refusing non-allowlisted URL: {path}"),
            ));
        }
        let headers = self.headers(req, hydra_jwt)?;
        let mut builder = self
            .inner
            .request(req.method.clone(), &req.url)
            .headers(headers);
        if let Some(body) = &req.body {
            builder = builder.json(body);
        }
        let resp = builder.send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::new(
                ErrorKind::AuthExpired,
                "401 from Grab: the food.grab.com session has expired; run `grab auth refresh` (or `grab auth login` if that fails)",
            ));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(Error::new(
                ErrorKind::RateLimited,
                "429 from Grab: slow down and retry later",
            ));
        }

        let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
            Error::new(
                ErrorKind::Api,
                format!(
                    "{} from {} with non-JSON body",
                    status.as_u16(),
                    req.url.split('?').next().unwrap_or(&req.url)
                ),
            )
        })?;

        // Grab signals an expired/invalid hydra token with a 400 and a
        // `message`/`reason` mentioning the hydra jwt.
        if is_hydra_error(status, &value) {
            return Err(Error::new(
                ErrorKind::HydraExpired,
                "Grab rejected the hydra JWT (invalid hydra jwt)",
            ));
        }
        if !status.is_success() {
            let msg = value
                .get("message")
                .or_else(|| value.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            return Err(Error::new(
                ErrorKind::Api,
                format!("{} from Grab: {msg}", status.as_u16()),
            ));
        }
        Ok(value)
    }
}

/// Parse one `Set-Cookie` header received from `host` into a [`RawCookie`].
///
/// Domains are stored the way browsers (and the CDP export) do: a `Domain`
/// attribute becomes `.grab.com` (a domain cookie), no `Domain` attribute means
/// a host-only cookie for `host`. `Max-Age` wins over `Expires`; a
/// `Max-Age<=0` or an `Expires` in the past marks the cookie as a deletion.
pub fn parse_set_cookie(s: &str, host: &str) -> Option<RawCookie> {
    parse_set_cookie_at(s, host, crate::jwt::now())
}

fn parse_set_cookie_at(s: &str, host: &str, now: i64) -> Option<RawCookie> {
    let mut parts = s.split(';');
    let nv = parts.next()?.trim();
    let (name, value) = nv.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let mut domain = host.to_ascii_lowercase();
    let mut path = "/".to_string();
    let mut max_age: Option<i64> = None;
    let mut expires_at: Option<i64> = None;
    for attr in parts {
        let attr = attr.trim();
        let Some((k, v)) = attr.split_once('=') else {
            continue;
        };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "domain" => {
                let bare = v.trim_start_matches('.').to_ascii_lowercase();
                if !bare.is_empty() {
                    domain = format!(".{bare}");
                }
            }
            "path" if v.starts_with('/') => path = v.to_string(),
            "max-age" => max_age = v.parse::<i64>().ok(),
            "expires" => expires_at = parse_http_date(v),
            _ => {}
        }
    }
    let (expires, deleted) = match (max_age, expires_at) {
        (Some(secs), _) => ((now + secs) as f64, secs <= 0),
        (None, Some(at)) => (at as f64, at <= now),
        (None, None) => (0.0, false),
    };
    Some(RawCookie {
        name: name.to_string(),
        value: value.trim().to_string(),
        domain,
        path,
        expires,
        deleted,
    })
}

/// Parse an HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`, also the
/// `06-Nov-1994` cookie variant) into Unix seconds.
pub fn parse_http_date(s: &str) -> Option<i64> {
    let s = s.trim();
    // Drop the weekday ("Sun, ").
    let rest = s.split_once(',').map_or(s, |(_, r)| r).trim();
    let mut fields = rest.split([' ', '-']).filter(|f| !f.is_empty());
    let day: i64 = fields.next()?.parse().ok()?;
    let mon = fields.next()?.to_ascii_lowercase();
    let month = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ]
    .iter()
    .position(|m| mon.starts_with(m))? as i64
        + 1;
    let mut year: i64 = fields.next()?.parse().ok()?;
    if year < 100 {
        year += if year < 70 { 2000 } else { 1900 };
    }
    let mut hms = fields.next()?.split(':').map(|x| x.parse::<i64>().ok());
    let (h, m, sec) = (hms.next()??, hms.next()??, hms.next()??);
    if !(1..=31).contains(&day) || h > 23 || m > 59 || sec > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + h * 3600 + m * 60 + sec)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn is_hydra_error(status: reqwest::StatusCode, value: &Value) -> bool {
    if status != reqwest::StatusCode::BAD_REQUEST {
        return false;
    }
    let text = |k: &str| {
        value
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    text("message").contains("hydra") || text("reason").contains("hydra")
}

/// A shareable handle so service code can hold the client cheaply.
pub type SharedClient = Arc<Client>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_matches_exact_and_prefix_ignoring_query() {
        let mut a = Allowlist::new();
        a.allow_exact("https://food.grab.com/proxy/foodweb/v2/order/cart")
            .allow_prefix("https://food.grab.com/proxy/foodweb/v2/order/merchants/");
        assert!(a.is_allowed("https://food.grab.com/proxy/foodweb/v2/order/cart"));
        assert!(a.is_allowed(
            "https://food.grab.com/proxy/foodweb/v2/order/merchants/2-EXAMPLE?latlng=1,2"
        ));
        // The order-submission endpoint is never registered for read-only use.
        assert!(!a.is_allowed("https://food.grab.com/proxy/foodweb/v2/order/orders"));
        assert!(!a.is_allowed("https://evil.example.com/"));
    }

    #[test]
    fn hydra_error_detection() {
        let v = serde_json::json!({"reason":"invalid_argument","message":"invalid hydra jwt"});
        assert!(is_hydra_error(reqwest::StatusCode::BAD_REQUEST, &v));
        assert!(!is_hydra_error(reqwest::StatusCode::OK, &v));
    }

    #[test]
    fn parses_set_cookie_with_attrs() {
        let c = parse_set_cookie_at(
            "gfc_session=abc.def; Max-Age=7200; Path=/; Domain=food.grab.com; HttpOnly; Secure",
            "food.grab.com",
            1_000,
        )
        .expect("parse");
        assert_eq!(c.name, "gfc_session");
        assert_eq!(c.value, "abc.def");
        assert_eq!(c.domain, ".food.grab.com");
        assert_eq!(c.path, "/");
        assert_eq!(c.expires, 8_200.0);
        assert!(!c.deleted);
    }

    #[test]
    fn parses_set_cookie_minimal_is_host_only_session() {
        let c = parse_set_cookie_at("k=v", "api.grab.com", 1_000).expect("parse");
        assert_eq!((c.name.as_str(), c.value.as_str()), ("k", "v"));
        assert_eq!(c.domain, "api.grab.com");
        assert_eq!(c.expires, 0.0);
        assert!(parse_set_cookie("=nope", "food.grab.com").is_none());
    }

    #[test]
    fn leading_dot_domain_is_kept_as_domain_cookie() {
        for raw in [
            "a=1; Domain=.grab.com",
            "a=1; Domain=grab.com",
            "a=1; domain=GRAB.com",
        ] {
            let c = parse_set_cookie_at(raw, "partner-api.grab.com", 0).expect("parse");
            assert_eq!(c.domain, ".grab.com", "{raw}");
        }
    }

    #[test]
    fn expires_attribute_is_read() {
        // Real shape from food.grab.com/auth/logout (value replaced).
        let c = parse_set_cookie_at(
            "gfc_country=X; Max-Age=31536000; Path=/; Expires=Sun, 17 Oct 2027 14:58:04 GMT; HttpOnly",
            "food.grab.com",
            1_792_248_684,
        )
        .expect("parse");
        // Max-Age wins over Expires.
        assert_eq!(c.expires, (1_792_248_684 + 31_536_000) as f64);

        let c = parse_set_cookie_at(
            "next-i18next=en; path=/; expires=Sun, 17 Oct 2027 14:58:04 GMT",
            "food.grab.com",
            0,
        )
        .expect("parse");
        assert_eq!(c.expires, 1_823_785_084.0);
        assert!(!c.deleted);
    }

    #[test]
    fn deletion_by_max_age_or_past_expires() {
        let now = 1_792_248_684; // 2026-10-17T14:51:24Z, before the 14:58:04 Expires
        let del = parse_set_cookie_at(
            "gfc_session=; Max-Age=0; Path=/; Expires=Sat, 17 Oct 2026 14:58:04 GMT",
            "food.grab.com",
            now,
        )
        .expect("parse");
        assert!(del.deleted);
        let past = parse_set_cookie_at(
            "x=1; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
            "food.grab.com",
            now,
        )
        .expect("parse");
        assert!(past.deleted);
    }

    #[test]
    fn http_dates() {
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(
            parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(
            parse_http_date("Sat, 29 Feb 2028 12:00:00 GMT"),
            Some(1_835_438_400)
        );
        assert_eq!(parse_http_date("garbage"), None);
    }
}
