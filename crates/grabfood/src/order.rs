//! Order submission — gated, and disabled by default.
//!
//! Safety model (see also `docs/checkout.md`):
//!
//! 1. The submit URL is only on the HTTP allowlist when `allow_ordering = true`
//!    in config. With it off, a submit attempt fails closed in the HTTP layer.
//! 2. Even with ordering enabled, `grab food order` re-quotes the current cart
//!    and refuses unless the caller supplied `--yes <quote-id>` matching the
//!    *fresh* quote's id, or confirmed interactively on a TTY.
//! 3. There is no server idempotency key for this endpoint, so on any ambiguous
//!    result (timeout, 5xx, network error) we NEVER retry — the caller is told
//!    to check `grab food orders --ongoing`.
//!
//! The body shape is taken from the web app's `createOrder` thunk: it carries
//! the `cartSignature` (the quote's `quoteSignature`) plus destination and
//! payment, and relies on the server to tie items/promos to that signature.

use grabcli_core::error::{Error, ErrorKind, Result};
use grabcli_core::location::Location;
use serde_json::{Value, json};

use crate::quote::Quote;

/// Build the `POST /order/orders` request body.
///
/// Mirrors the web `createOrder` body:
/// `{destination{address,keywords,coordinates{lat,lng},addressDetail,poiID},
///   cartSignature, supportComplexMenu:true, deliveryNotes, paymentTypeID,
///   isIntegrated, countryCode}`.
///
/// `addressDetail` and the saved driver note come from the location cookie;
/// `extra_notes` (`--notes`) is appended to the saved note, not a replacement.
pub fn build_submit_body(
    quote: &Quote,
    loc: &Location,
    is_integrated: bool,
    extra_notes: Option<&str>,
) -> Value {
    json!({
        "destination": {
            "address": loc.address,
            "keywords": loc.address,
            "coordinates": { "latitude": loc.latitude, "longitude": loc.longitude },
            "addressDetail": loc.address_detail.as_deref().unwrap_or_default(),
            "poiID": loc.id,
        },
        "cartSignature": quote.quote_signature,
        "supportComplexMenu": true,
        "deliveryNotes": loc.delivery_notes(extra_notes),
        "paymentTypeID": quote.payment_type_id,
        "isIntegrated": is_integrated,
        "countryCode": "PH",
    })
}

/// A copy of the submit body safe to display: the `cartSignature` (which ties to
/// a priced quote and shouldn't be echoed), address text, unit and driver note
/// are redacted.
pub fn redacted_body(body: &Value) -> Value {
    let mut b = body.clone();
    if let Some(obj) = b.as_object_mut() {
        for k in ["cartSignature", "deliveryNotes"] {
            if obj.contains_key(k) {
                obj.insert(k.into(), json!("<redacted>"));
            }
        }
        if let Some(dest) = obj.get_mut("destination").and_then(Value::as_object_mut) {
            for k in ["address", "keywords", "addressDetail"] {
                if dest.contains_key(k) {
                    dest.insert(k.into(), json!("<redacted>"));
                }
            }
            dest.insert("coordinates".into(), json!("<redacted>"));
            if dest.contains_key("poiID") {
                dest.insert("poiID".into(), json!("<redacted>"));
            }
        }
    }
    b
}

/// Decision of the confirmation gate, computed from inputs (pure, testable).
#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    /// Proceed: the fresh quote id matched `--yes` or the user confirmed.
    Proceed,
    /// Refuse: the fresh quote differs from the confirmed one.
    QuoteChanged { expected: String, actual: String },
    /// Refuse: no `--yes` and not a TTY.
    NeedConfirmation,
}

/// Decide whether to proceed given the fresh quote id, an optional `--yes` id,
/// whether stdin is a TTY, and (for TTY) the typed confirmation result.
pub fn gate(fresh_id: &str, yes: Option<&str>, is_tty: bool, tty_confirmed: Option<bool>) -> Gate {
    if let Some(expected) = yes {
        return if expected == fresh_id {
            Gate::Proceed
        } else {
            Gate::QuoteChanged {
                expected: expected.to_string(),
                actual: fresh_id.to_string(),
            }
        };
    }
    if is_tty {
        match tty_confirmed {
            Some(true) => Gate::Proceed,
            _ => Gate::NeedConfirmation,
        }
    } else {
        Gate::NeedConfirmation
    }
}

/// Map a submit response to an order id, or a typed error.
pub fn parse_submit(resp: &Value) -> Result<String> {
    if let Some(id) = resp
        .get("orderID")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Ok(id.to_string());
    }
    let msg = resp
        .get("message")
        .or_else(|| resp.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("order response had no orderID");
    Err(Error::new(
        ErrorKind::Api,
        format!("order not confirmed: {msg}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use grabcli_core::money::Money;

    fn quote() -> Quote {
        Quote {
            merchant_name: "Example".into(),
            subtotal: Money(44000),
            delivery_before: Money(18000),
            delivery_after: Money(13100),
            fees: vec![],
            promos: vec![],
            total_before: Money(62000),
            total_after: Money(57100),
            eta_min: Some(45),
            quote_signature: "SIG-SECRET".into(),
            payment_type_id: "PAY-1".into(),
            address: "1 Example St".into(),
            address_detail: None,
            note_to_driver: None,
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

    fn loc_with_detail() -> Location {
        serde_json::from_str(
            r#"{"id":"POI-X","latitude":1.0,"longitude":2.0,"address":"1 Example St","addressDetail":"Unit 1A, Example Tower","noteToDriver":"Leave at lobby"}"#,
        )
        .expect("loc")
    }

    #[test]
    fn body_has_address_detail_and_appended_notes() {
        let b = build_submit_body(&quote(), &loc_with_detail(), true, Some("Ring bell"));
        assert_eq!(b["destination"]["addressDetail"], "Unit 1A, Example Tower");
        assert_eq!(b["deliveryNotes"], "Leave at lobby; Ring bell");
        let b = build_submit_body(&quote(), &loc_with_detail(), true, None);
        assert_eq!(b["deliveryNotes"], "Leave at lobby");
        let b = build_submit_body(&quote(), &loc(), true, Some("Ring bell"));
        assert_eq!(b["destination"]["addressDetail"], "");
        assert_eq!(b["deliveryNotes"], "Ring bell");
    }

    #[test]
    fn redaction_hides_detail_and_notes() {
        let b = redacted_body(&build_submit_body(
            &quote(),
            &loc_with_detail(),
            true,
            Some("Ring bell"),
        ));
        assert_eq!(b["destination"]["addressDetail"], "<redacted>");
        assert_eq!(b["deliveryNotes"], "<redacted>");
        let s = serde_json::to_string(&b).expect("json");
        assert!(!s.contains("Example Tower"));
        assert!(!s.contains("Leave at lobby"));
        assert!(!s.contains("Ring bell"));
    }

    #[test]
    fn body_has_cart_signature_and_payment() {
        let b = build_submit_body(&quote(), &loc(), true, None);
        assert_eq!(b["cartSignature"], "SIG-SECRET");
        assert_eq!(b["paymentTypeID"], "PAY-1");
        assert_eq!(b["supportComplexMenu"], true);
        assert_eq!(b["destination"]["poiID"], "POI-X");
    }

    #[test]
    fn redaction_hides_signature_and_address() {
        let b = redacted_body(&build_submit_body(&quote(), &loc(), true, None));
        assert_eq!(b["cartSignature"], "<redacted>");
        assert_eq!(b["destination"]["address"], "<redacted>");
        assert_eq!(b["destination"]["coordinates"], "<redacted>");
        let s = serde_json::to_string(&b).expect("json");
        assert!(!s.contains("SIG-SECRET"));
        assert!(!s.contains("1 Example St"));
    }

    #[test]
    fn yes_must_match_fresh_id() {
        assert_eq!(
            gate("abc123abc123", Some("abc123abc123"), false, None),
            Gate::Proceed
        );
        assert_eq!(
            gate("abc123abc123", Some("stale0000000"), false, None),
            Gate::QuoteChanged {
                expected: "stale0000000".into(),
                actual: "abc123abc123".into()
            }
        );
    }

    #[test]
    fn non_tty_without_yes_refuses() {
        assert_eq!(gate("id", None, false, None), Gate::NeedConfirmation);
    }

    #[test]
    fn tty_confirmed_proceeds() {
        assert_eq!(gate("id", None, true, Some(true)), Gate::Proceed);
        assert_eq!(gate("id", None, true, Some(false)), Gate::NeedConfirmation);
    }

    #[test]
    fn submit_parse() -> Result<()> {
        assert_eq!(parse_submit(&json!({"orderID": "ORD-9"}))?, "ORD-9");
        assert!(parse_submit(&json!({"message": "ITEM_NOT_AVAILABLE"})).is_err());
        Ok(())
    }
}
