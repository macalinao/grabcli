//! Price quotes via `POST /order/cart`.
//!
//! This endpoint only *prices* a cart; it never places an order. It returns a
//! `quoteSignature` the checkout call later references. We parse the breakdown,
//! always surface the delivery address, and derive a short, stable quote id so
//! an order can be gated on "the thing the user confirmed is the thing we'd
//! send".

use grabcli_core::error::{Error, Result};
use grabcli_core::location::Location;
use grabcli_core::money::Money;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::cart::Cart;

#[derive(Debug, Clone)]
pub struct Fee {
    pub name: String,
    pub amount: Money,
}

#[derive(Debug, Clone)]
pub struct Promo {
    pub name: String,
    pub amount: Money,
}

#[derive(Debug, Clone)]
pub struct Quote {
    pub merchant_name: String,
    pub subtotal: Money,
    pub delivery_before: Money,
    pub delivery_after: Money,
    pub fees: Vec<Fee>,
    pub promos: Vec<Promo>,
    pub total_before: Money,
    pub total_after: Money,
    pub eta_min: Option<i64>,
    pub quote_signature: String,
    pub payment_type_id: String,
    pub address: String,
    /// Unit / building line from the saved location.
    pub address_detail: Option<String>,
    /// Saved driver note from the saved location.
    pub note_to_driver: Option<String>,
    pub poi_id: String,
    /// The stable short id for this quote (see [`Quote::id`]).
    pub id: String,
}

/// Build the request body for `POST /order/cart` (also the basis for the order
/// submission's `cartSignature`). `promo_codes` is a list of plain codes.
pub fn build_body(
    cart: &Cart,
    loc: &Location,
    payment_type_id: &str,
    promo_codes: &[String],
) -> Value {
    let promo: Vec<Value> = promo_codes
        .iter()
        .map(|c| json!({ "promoCode": c }))
        .collect();
    json!({
        "merchants": [cart.to_quote_merchant()],
        "paymentTypeID": payment_type_id,
        "promoCode": promo,
        "comment": "",
        "campaignIDs": [],
        "enterprise": null,
        "scheduledTime": null,
        "countryCode": "PH",
        "destination": {
            "longitude": loc.longitude,
            "latitude": loc.latitude,
            "address": loc.address,
            "keywords": "",
            "meta": "",
            "poiID": loc.id,
        },
    })
}

fn money(v: &Value, ptr: &str) -> Money {
    Money(v.pointer(ptr).and_then(Value::as_i64).unwrap_or(0))
}

impl Quote {
    pub fn parse(
        resp: &Value,
        cart: &Cart,
        loc: &Location,
        payment_type_id: &str,
        promo_codes: &[String],
    ) -> Result<Self> {
        if let Some(msg) = resp.get("errMessage").and_then(Value::as_str)
            && !msg.is_empty()
        {
            return Err(Error::new(
                grabcli_core::error::ErrorKind::Api,
                format!("quote error: {msg}"),
            ));
        }

        let merchant_name = resp
            .pointer("/merchants/0/name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        // Fees: each non-aggregate fee line from the breakdowns, skipping the
        // "Applicable fees" summary line.
        let mut fees = Vec::new();
        let mut promos = Vec::new();
        if let Some(bs) = resp.get("breakdowns").and_then(Value::as_array) {
            for b in bs {
                let key = b.get("key").and_then(Value::as_str).unwrap_or_default();
                match key {
                    "service_fees" => {
                        for d in b
                            .get("details")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                        {
                            fees.push(Fee {
                                name: d
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("Fee")
                                    .to_string(),
                                amount: money(d, "/value/amountInMinor"),
                            });
                        }
                    }
                    "stackable_offer" => {
                        promos.push(Promo {
                            name: b
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("Promo")
                                .to_string(),
                            amount: money(b, "/value/amountInMinor"),
                        });
                    }
                    _ => {}
                }
            }
        }

        let total_before = money(resp, "/totalQuote/priceInMinorUnit");
        let total_after = {
            let r = money(resp, "/totalQuote/reducedPriceInMinorUnit");
            if r.minor() == 0 { total_before } else { r }
        };

        let quote_signature = resp
            .get("quoteSignature")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let address = resp
            .pointer("/destination/address")
            .and_then(Value::as_str)
            .unwrap_or(&loc.address)
            .to_string();
        let poi_id = resp
            .pointer("/destination/poiID")
            .and_then(Value::as_str)
            .unwrap_or(&loc.id)
            .to_string();

        let mut q = Quote {
            address_detail: loc.address_detail.clone(),
            note_to_driver: loc.note_to_driver.clone(),
            merchant_name,
            subtotal: money(resp, "/foodQuote/priceInMinorUnit"),
            delivery_before: money(resp, "/deliveryQuote/priceInMinorUnit"),
            delivery_after: {
                let r = money(resp, "/deliveryQuote/reducedPriceInMinorUnit");
                if r.minor() == 0 {
                    money(resp, "/deliveryQuote/priceInMinorUnit")
                } else {
                    r
                }
            },
            fees,
            promos,
            total_before,
            total_after,
            eta_min: resp
                .pointer("/merchants/0/merchantETA/maxETAInMinute")
                .and_then(Value::as_i64),
            quote_signature,
            payment_type_id: payment_type_id.to_string(),
            address,
            poi_id,
            id: String::new(),
        };
        q.id = q.compute_id(cart, promo_codes);
        Ok(q)
    }

    /// Short stable id: first 12 hex of sha256 over canonical JSON of the
    /// price-determining inputs. Two quotes with the same cart, address,
    /// payment, promos and total get the same id; any of those changing flips
    /// it. The `quoteSignature` is deliberately NOT part of the id: Grab mints a
    /// fresh signature on every `/order/cart` call, so including it would make
    /// the id change between an identical `quote` and the `order` re-quote and
    /// the `--yes <id>` confirmation could never match. The id answers "is this
    /// the same order at the same price?", which is what the confirmation gate
    /// needs; the signature still flows into the actual submit body.
    fn compute_id(&self, cart: &Cart, promo_codes: &[String]) -> String {
        let canon = json!({
            "cart": cart,
            "poiID": self.poi_id,
            "payment": self.payment_type_id,
            "promos": promo_codes,
            "total": self.total_after.minor(),
        });
        // serde_json serializes maps in a stable (sorted for BTreeMap, insertion
        // for Map) order; our structs/json! use fixed key order, so this is
        // deterministic for identical inputs.
        let bytes = serde_json::to_vec(&canon).unwrap_or_default();
        let digest = Sha256::digest(&bytes);
        digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cart::{Cart, CartItem};

    fn loc() -> Location {
        // Synthetic location.
        let json = r#"{"id":"POI-X","latitude":1.0,"longitude":2.0,"address":"1 Example St","countryCode":"PH"}"#;
        serde_json::from_str(json).expect("loc")
    }

    fn cart() -> Cart {
        Cart {
            merchant_id: "2-EX".into(),
            section: "S1".into(),
            items: vec![CartItem {
                item_id: "I1".into(),
                quantity: 1,
                modifiers: vec![],
                comment: String::new(),
            }],
            voucher: None,
        }
    }

    fn resp() -> Value {
        serde_json::json!({
            "quoteSignature": "SIG-EXAMPLE",
            "merchants": [{"name": "Example Noodles", "merchantETA": {"maxETAInMinute": 45}}],
            "destination": {"address": "1 Example St", "poiID": "POI-X"},
            "foodQuote": {"priceInMinorUnit": 44000},
            "deliveryQuote": {"priceInMinorUnit": 18000, "reducedPriceInMinorUnit": 13100},
            "totalQuote": {"priceInMinorUnit": 62000, "reducedPriceInMinorUnit": 57100},
            "breakdowns": [
                {"key": "subtotal", "value": {"amountInMinor": 44000}},
                {"key": "service_fees", "name": "Applicable fees",
                 "details": [{"name": "Delivery fee", "value": {"amountInMinor": 18000}}]},
                {"key": "stackable_offer", "name": "₱49 off delivery", "value": {"amountInMinor": -4900}}
            ]
        })
    }

    #[test]
    fn parses_quote() -> Result<()> {
        let q = Quote::parse(&resp(), &cart(), &loc(), "PAY-1", &[])?;
        assert_eq!(q.subtotal, Money(44000));
        assert_eq!(q.delivery_after, Money(13100));
        assert_eq!(q.total_after, Money(57100));
        assert_eq!(q.promos[0].amount, Money(-4900));
        assert_eq!(q.fees[0].name, "Delivery fee");
        assert_eq!(q.eta_min, Some(45));
        assert_eq!(q.quote_signature, "SIG-EXAMPLE");
        assert_eq!(q.id.len(), 12);
        Ok(())
    }

    #[test]
    fn quote_id_is_stable_and_sensitive() -> Result<()> {
        let a = Quote::parse(&resp(), &cart(), &loc(), "PAY-1", &[])?;
        let b = Quote::parse(&resp(), &cart(), &loc(), "PAY-1", &[])?;
        assert_eq!(a.id, b.id);
        // Different payment -> different id.
        let c = Quote::parse(&resp(), &cart(), &loc(), "PAY-2", &[])?;
        assert_ne!(a.id, c.id);
        Ok(())
    }

    #[test]
    fn build_body_shape() {
        let body = build_body(&cart(), &loc(), "PAY-1", &["SAVE10".into()]);
        assert_eq!(body["paymentTypeID"], "PAY-1");
        assert_eq!(body["promoCode"][0]["promoCode"], "SAVE10");
        assert_eq!(body["destination"]["poiID"], "POI-X");
        assert_eq!(body["merchants"][0]["ID"], "2-EX");
    }
}
