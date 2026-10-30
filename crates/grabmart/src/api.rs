//! GrabMart endpoint URLs and the URL allowlist (see docs/mart.md).
//!
//! Browse calls go to the `public` base and need a reCAPTCHA v3 token (see
//! [`crate::recaptcha`]). The cart/quote call goes to the `protected` base on
//! the mart.grab.com origin with the logged-in session plus `X-Hydra-JWT`.
//!
//! The order-submission endpoint ([`ORDERS`]) is registered **only** when
//! `allow_ordering = true` in config, exactly like GrabFood. With ordering off,
//! any attempt to call it fails closed in the HTTP layer before a request is
//! sent.

use grabcli_core::http::Allowlist;

/// Public (anonymous, reCAPTCHA-gated) API base.
pub const PUBLIC: &str = "https://api.grab.com/martwebapi/v1/public";

/// Protected (logged-in) API base, proxied same-origin by mart.grab.com.
pub const PROTECTED: &str = "https://mart.grab.com/martwebapi/v1/protected";

/// The web origin the reCAPTCHA site key is bound to.
pub const ORIGIN: &str = "https://mart.grab.com";

/// Page opened in the CDP browser when no mart.grab.com tab exists.
pub const RECAPTCHA_PAGE: &str = "https://mart.grab.com/ph/en";

/// The quote endpoint (POST). Prices a cart; never places an order.
pub const CART: &str = "https://mart.grab.com/martwebapi/v1/protected/cart";

/// The place-order endpoint (POST). Never allowlisted unless ordering is on.
pub const ORDERS: &str = "https://mart.grab.com/martwebapi/v1/protected/orders";

fn enc(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

pub fn homefeed_url(lat: f64, lng: f64, offset: u32, size: u32) -> String {
    format!("{PUBLIC}/homefeed?latitude={lat}&longitude={lng}&offset={offset}&size={size}")
}

pub fn category_url(category: &str, lat: f64, lng: f64, offset: u32, size: u32) -> String {
    format!(
        "{PUBLIC}/category?categoryShortcutID={}&latlng={lat},{lng}&offset={offset}&pageSize={size}",
        enc(category)
    )
}

pub fn search_url(keyword: &str, lat: f64, lng: f64, offset: u32, size: u32) -> String {
    format!(
        "{PUBLIC}/search?keyword={}&latitude={lat}&longitude={lng}&offset={offset}&size={size}",
        enc(keyword)
    )
}

pub fn merchant_url(id: &str, lat: f64, lng: f64) -> String {
    format!("{PUBLIC}/merchants/{}?latlng={lat},{lng}", enc(id))
}

#[allow(clippy::too_many_arguments)]
pub fn items_url(
    id: &str,
    department: &str,
    department_type: i64,
    sub: Option<&str>,
    lat: f64,
    lng: f64,
    offset: u32,
    size: u32,
) -> String {
    format!(
        "{PUBLIC}/merchants/{m}/items?merchantID={m}&latlng={lat},{lng}&parentID={}&departmentType={department_type}&subDepartmentID={}&poiID=&offset={offset}&size={size}",
        enc(department),
        enc(sub.unwrap_or_default()),
        m = enc(id),
    )
}

pub fn instore_search_url(
    id: &str,
    keyword: &str,
    lat: f64,
    lng: f64,
    offset: u32,
    size: u32,
) -> String {
    format!(
        "{PUBLIC}/merchants/{m}/search?merchantID={m}&latlng={lat},{lng}&keyword={}&offset={offset}&size={size}",
        enc(keyword),
        m = enc(id),
    )
}

pub fn item_url(item: &str, lat: f64, lng: f64, poi_id: &str) -> String {
    format!(
        "{PUBLIC}/item/{}?latitude={lat}&longitude={lng}&poiID={}",
        enc(item),
        enc(poi_id)
    )
}

/// Build the GrabMart allowlist. Public browse endpoints and the quote are
/// always allowed; the submit endpoint is added only when `allow_ordering`.
pub fn allowlist(allow_ordering: bool) -> Allowlist {
    let mut a = Allowlist::new();
    a.allow_prefix(format!("{PUBLIC}/homefeed"))
        .allow_prefix(format!("{PUBLIC}/category"))
        .allow_prefix(format!("{PUBLIC}/search"))
        .allow_prefix(format!("{PUBLIC}/merchants/"))
        .allow_prefix(format!("{PUBLIC}/item/"))
        .allow_exact(CART);
    if allow_ordering {
        a.allow_exact(ORDERS);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_blocked_by_default() {
        let a = allowlist(false);
        assert!(!a.is_allowed(ORDERS));
        assert!(a.is_allowed(CART));
        assert!(a.is_allowed(&search_url("red bull", 1.0, 2.0, 0, 20)));
        assert!(a.is_allowed(&merchant_url("2-EX", 1.0, 2.0)));
        assert!(a.is_allowed(&item_url("PHITE1", 1.0, 2.0, "")));
        // Other protected endpoints (orders history, wallet, ...) are not allowed.
        assert!(!a.is_allowed("https://mart.grab.com/martwebapi/v1/protected/orders/history"));
        assert!(!a.is_allowed("https://mart.grab.com/martwebapi/v1/protected/wallet/methods"));
    }

    #[test]
    fn submit_allowed_only_when_enabled() {
        assert!(allowlist(true).is_allowed(ORDERS));
    }

    #[test]
    fn urls_encode_params() {
        let u = search_url("S&R", 1.2, 103.8, 0, 20);
        assert!(u.contains("keyword=S%26R"));
        let u = items_url(
            "2-EX",
            "2-EX-PHITEDP1",
            0,
            Some("2-EX-PHITEDP2"),
            1.0,
            2.0,
            20,
            20,
        );
        assert!(u.contains("parentID=2-EX-PHITEDP1"));
        assert!(u.contains("subDepartmentID=2-EX-PHITEDP2"));
        assert!(u.contains("offset=20"));
    }
}
