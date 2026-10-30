//! GrabMart order submission — gated exactly like GrabFood, and disabled by
//! default.
//!
//! 1. The submit URL ([`crate::api::ORDERS`]) is only on the HTTP allowlist
//!    when `allow_ordering = true` in config.
//! 2. `grab mart order` re-quotes the cart and refuses unless `--yes
//!    <quote-id>` matches the *fresh* quote id, or the user confirms on a TTY.
//!    The decision is GrabFood's [`grabfood::order::gate`], shared so both
//!    services have one implementation.
//! 3. No idempotency key exists, so a failed submit is never retried; the
//!    caller is told to check the Grab app for an ongoing order.
//!
//! Body shape from the mart web bundle's place-order handler.

use grabcli_core::location::Location;
use serde_json::{Value, json};

pub use grabfood::order::{Gate, gate, parse_submit};

use crate::quote::Quote;

/// Build the `POST /protected/orders` body: food's order body plus
/// `businessType:"MART"` and the out-of-stock preference `mpaDetail` (left
/// `null`, the web default when none is chosen). `addressDetail` and the saved
/// driver note come from the location cookie; `extra_notes` (`--notes`) is
/// appended to the note, as for food.
pub fn build_submit_body(quote: &Quote, loc: &Location, extra_notes: Option<&str>) -> Value {
    json!({
        "deliveryNotes": loc.delivery_notes(extra_notes),
        "destination": {
            "coordinates": {
                "latitude": loc.latitude,
                "longitude": loc.longitude,
                "poiID": loc.id,
            },
            "address": loc.address,
            "addressDetail": loc.address_detail.as_deref().unwrap_or_default(),
            "poiID": loc.id,
        },
        "paymentTypeID": quote.payment_type_id,
        "cartSignature": quote.quote_signature,
        "businessType": "MART",
        "supportComplexMenu": true,
        "mpaDetail": null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api;
    use grabcli_core::money::Money;

    fn quote() -> Quote {
        Quote {
            merchant_name: "S&R".into(),
            subtotal: Money(33900),
            delivery_before: Money(9900),
            delivery_after: Money(9900),
            fees: vec![],
            discounts: vec![],
            tax: None,
            total_before: Money(43800),
            total_after: Money(43800),
            eta_min: None,
            quote_signature: "SIG-SECRET".into(),
            payment_type_id: "PAY-1".into(),
            address: "1 Example St".into(),
            poi_id: "POI-X".into(),
            id: "abc123abc123".into(),
        }
    }

    fn loc() -> Location {
        serde_json::from_str(
            r#"{"id":"POI-X","latitude":1.0,"longitude":2.0,"address":"1 Example St","countryCode":"PH"}"#,
        )
        .expect("loc")
    }

    #[test]
    fn body_is_mart_order() {
        let b = build_submit_body(&quote(), &loc(), None);
        assert_eq!(b["businessType"], "MART");
        assert_eq!(b["cartSignature"], "SIG-SECRET");
        assert_eq!(b["paymentTypeID"], "PAY-1");
        assert_eq!(b["destination"]["coordinates"]["poiID"], "POI-X");
        assert!(b["mpaDetail"].is_null());
    }

    #[test]
    fn body_has_address_detail_and_appended_notes() {
        let saved: Location = serde_json::from_str(
            r#"{"id":"POI-X","latitude":1.0,"longitude":2.0,"address":"1 Example St","addressDetail":"Unit 1A, Example Tower","noteToDriver":"Leave at lobby"}"#,
        )
        .expect("loc");
        let b = build_submit_body(&quote(), &saved, Some("Ring bell"));
        assert_eq!(b["destination"]["addressDetail"], "Unit 1A, Example Tower");
        assert_eq!(b["deliveryNotes"], "Leave at lobby; Ring bell");
        let b = build_submit_body(&quote(), &loc(), Some("Ring bell"));
        assert_eq!(b["destination"]["addressDetail"], "");
        assert_eq!(b["deliveryNotes"], "Ring bell");
        let r = crate::quote::redacted(&build_submit_body(&quote(), &saved, None));
        assert_eq!(r["destination"]["addressDetail"], "<redacted>");
        assert_eq!(r["deliveryNotes"], "<redacted>");
    }

    #[test]
    fn redacted_submit_body_hides_secrets() {
        let b = crate::quote::redacted(&build_submit_body(&quote(), &loc(), None));
        let s = serde_json::to_string(&b).expect("json");
        assert!(!s.contains("SIG-SECRET"));
        assert!(!s.contains("1 Example St"));
        assert!(!s.contains("POI-X"));
    }

    #[test]
    fn gate_requires_matching_fresh_id() {
        assert_eq!(gate("abc", Some("abc"), false, None), Gate::Proceed);
        assert!(matches!(
            gate("abc", Some("old"), false, None),
            Gate::QuoteChanged { .. }
        ));
        assert_eq!(gate("abc", None, false, None), Gate::NeedConfirmation);
        assert_eq!(gate("abc", None, true, Some(false)), Gate::NeedConfirmation);
    }

    #[test]
    fn submit_url_fails_closed_without_allow_ordering() {
        assert!(!api::allowlist(false).is_allowed(api::ORDERS));
    }
}
