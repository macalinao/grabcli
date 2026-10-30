//! GrabFood endpoint URLs and the URL allowlist.
//!
//! All read-only endpoints are always allowlisted. The order-submission
//! endpoint ([`ORDERS`]) is registered **only** when ordering is explicitly
//! enabled in config. With ordering disabled (the default), any attempt to call
//! it fails closed in the HTTP layer before a request is sent.

use grabcli_core::http::Allowlist;

/// Guest discovery API (cookies + `x-country-code`, no hydra); see
/// docs/discovery.md.
const GUEST: &str = "https://portal.grab.com/foodweb/guest/v2";

/// Guest search (POST; also the "nearby" list with an empty keyword).
pub const SEARCH: &str = "https://portal.grab.com/foodweb/guest/v2/search";

/// Cuisine / collection listing (GET).
pub const CATEGORY: &str = "https://portal.grab.com/foodweb/guest/v2/category";

/// Home collections (GET).
pub const SHORTCUTS: &str = "https://portal.grab.com/foodweb/guest/v2/category/shortcuts";

/// "Popular near you" (GET).
pub const RECOMMENDED: &str = "https://portal.grab.com/foodweb/guest/v2/recommended/merchants";

/// Guest merchant/menu (GET), same shape as the proxy menu; used for browsing
/// when the logged-in session has expired.
pub const GUEST_MERCHANT_PREFIX: &str = "https://portal.grab.com/foodweb/guest/v2/merchants/";

pub fn category_url(latlng: &str, id: &str, offset: usize, page_size: usize) -> String {
    format!(
        "{CATEGORY}?latlng={latlng}&categoryShortcutID={}&offset={offset}&pageSize={page_size}&countryCode=PH",
        urlencoding::encode(id)
    )
}

pub fn shortcuts_url(latlng: &str) -> String {
    format!("{SHORTCUTS}?latlng={latlng}&countryCode=PH&cityID=4&poiID=")
}

pub fn recommended_url(latitude: f64, longitude: f64, offset: usize) -> String {
    format!(
        "{RECOMMENDED}?latitude={latitude}&longitude={longitude}&mode=&offset={offset}&countryCode=PH"
    )
}

pub fn guest_merchant_url(id: &str, latlng: &str) -> String {
    format!(
        "{GUEST}/merchants/{}?latlng={latlng}",
        urlencoding::encode(id)
    )
}

const PROXY: &str = "https://food.grab.com/proxy/foodweb/v2";

pub fn merchant_url(id: &str, latlng: &str) -> String {
    format!("{PROXY}/order/merchants/{id}?latlng={latlng}")
}
pub const MERCHANT_PREFIX: &str = "https://food.grab.com/proxy/foodweb/v2/order/merchants/";

pub fn cart_url() -> String {
    format!("{PROXY}/order/cart")
}

pub fn eligible_vouchers_url() -> String {
    format!("{PROXY}/rewards/eligible-vouchers")
}

pub fn history_url() -> String {
    format!("{PROXY}/order/history")
}

pub fn order_get_url(id: &str) -> String {
    format!("{PROXY}/order/orders/{id}")
}

pub fn order_track_url(id: &str) -> String {
    format!("{PROXY}/order/orders/{id}/track")
}

/// The place-order endpoint (POST). Never allowlisted unless ordering is on.
pub const ORDERS: &str = "https://food.grab.com/proxy/foodweb/v2/order/orders";

/// Build the GrabFood allowlist. Read-only endpoints are always allowed; the
/// submit endpoint is added only when `allow_ordering` is true.
pub fn allowlist(allow_ordering: bool) -> Allowlist {
    let mut a = Allowlist::new();
    a.allow_exact(SEARCH)
        .allow_exact(CATEGORY)
        .allow_exact(SHORTCUTS)
        .allow_exact(RECOMMENDED)
        .allow_prefix(GUEST_MERCHANT_PREFIX)
        .allow_prefix(MERCHANT_PREFIX)
        .allow_exact(cart_url())
        .allow_exact(eligible_vouchers_url())
        .allow_prefix(history_url())
        .allow_prefix("https://food.grab.com/proxy/foodweb/v2/order/orders/")
        .allow_exact("https://food.grab.com/proxy/foodweb/v2/order/content/merchantsETA");
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
        // read-only ones are allowed
        assert!(a.is_allowed(&cart_url()));
        assert!(a.is_allowed(&merchant_url("2-EXAMPLE", "1,2")));
        assert!(a.is_allowed(&order_track_url("ORD-1")));
        // discovery GETs
        assert!(a.is_allowed(&category_url("1,2", "89", 0, 32)));
        assert!(a.is_allowed(&shortcuts_url("1,2")));
        assert!(a.is_allowed(&recommended_url(1.0, 2.0, 8)));
        assert!(a.is_allowed(&guest_merchant_url("2-EX", "1,2")));
        assert!(!a.is_allowed("https://portal.grab.com/foodweb/guest/v2/categories"));
    }

    #[test]
    fn submit_allowed_only_when_enabled() {
        assert!(allowlist(true).is_allowed(ORDERS));
    }
}
