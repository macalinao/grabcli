//! GrabMart price quotes via `POST mart.grab.com/martwebapi/v1/protected/cart`.
//!
//! This endpoint only *prices* a cart; it never places an order. The body is
//! the one the mart web app builds (`merchants`, `paymentTypeID`,
//! `destination`, `promoCode`, `campaignIDs`). The response carries a
//! `quoteSignature`, `totalQuote`, `deliveryQuote` and `breakdowns[]` lines
//! (subtotal, fees, offers). We parse those leniently, always surface the
//! delivery address, and derive a short stable quote id for the order gate.

use grabcli_core::error::{Error, ErrorKind, Result};
use grabcli_core::location::Location;
use grabcli_core::money::Money;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::cart::Cart;

/// Build the quote body. `campaign_ids` are the store's campaigns plus any
/// item-level campaigns (the web sends both).
pub fn build_body(
    cart: &Cart,
    section: &str,
    loc: &Location,
    payment_type_id: &str,
    campaign_ids: &[String],
) -> Value {
    json!({
        "merchants": [cart.to_quote_merchant(section)],
        "paymentTypeID": payment_type_id,
        "destination": {
            "latitude": loc.latitude,
            "longitude": loc.longitude,
            "address": loc.address,
            "poiID": loc.id,
        },
        "promoCode": [],
        "campaignIDs": campaign_ids,
    })
}

/// Store campaigns followed by item campaigns, de-duplicated, order kept.
pub fn campaign_ids(store_campaigns: &[String], cart: &Cart) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in store_campaigns
        .iter()
        .cloned()
        .chain(cart.items.iter().filter_map(|i| i.campaign_id.clone()))
    {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// A copy of a quote/order body that is safe to print: the delivery address,
/// coordinates, POI, unit and driver notes are replaced, and any signature is
/// hidden.
pub fn redacted(body: &Value) -> Value {
    let mut b = body.clone();
    if let Some(obj) = b.as_object_mut() {
        for k in ["cartSignature", "deliveryNotes"] {
            if obj.contains_key(k) {
                obj.insert(k.into(), json!("<redacted>"));
            }
        }
        if let Some(dest) = obj.get_mut("destination").and_then(Value::as_object_mut) {
            for k in [
                "address",
                "keywords",
                "latitude",
                "longitude",
                "coordinates",
                "poiID",
                "addressDetail",
            ] {
                if dest.contains_key(k) {
                    dest.insert(k.into(), json!("<redacted>"));
                }
            }
        }
    }
    b
}

/// One priced line (fee, discount, ...).
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub key: String,
    pub name: String,
    pub amount: Money,
}

#[derive(Debug, Clone)]
pub struct Quote {
    pub merchant_name: String,
    pub subtotal: Money,
    pub delivery_before: Money,
    pub delivery_after: Money,
    /// Service, small-order, platform, bag and other fees.
    pub fees: Vec<Line>,
    /// Offers, promos and discounts (negative amounts).
    pub discounts: Vec<Line>,
    pub tax: Option<Money>,
    pub total_before: Money,
    pub total_after: Money,
    pub eta_min: Option<i64>,
    pub quote_signature: String,
    pub payment_type_id: String,
    pub address: String,
    pub poi_id: String,
    pub id: String,
}

fn money_at(v: &Value, ptrs: &[&str]) -> Option<Money> {
    ptrs.iter()
        .find_map(|p| v.pointer(p).and_then(Value::as_i64))
        .map(Money)
}

fn line_amount(v: &Value) -> Money {
    money_at(
        v,
        &[
            "/value/amountInMinor",
            "/amountInMinor",
            "/value/priceInMinorUnit",
        ],
    )
    .unwrap_or(Money::ZERO)
}

fn is_discount(key: &str, name: &str, amount: Money) -> bool {
    let k = format!("{key} {name}").to_ascii_lowercase();
    amount.minor() < 0
        || ["offer", "promo", "discount", "voucher", "campaign"]
            .iter()
            .any(|w| k.contains(w))
}

impl Quote {
    pub fn parse(resp: &Value, cart: &Cart, loc: &Location, payment_type_id: &str) -> Result<Self> {
        for k in ["errMessage", "errorMessage"] {
            if let Some(msg) = resp.get(k).and_then(Value::as_str)
                && !msg.is_empty()
            {
                return Err(Error::new(ErrorKind::Api, format!("quote error: {msg}")));
            }
        }
        if let Some(reasons) = resp.get("errReasons").and_then(Value::as_array)
            && !reasons.is_empty()
        {
            return Err(Error::new(
                ErrorKind::Api,
                format!("quote error: {}", Value::Array(reasons.clone())),
            ));
        }
        if let Some(d) = resp.get("errorDialog").filter(|d| !d.is_null()) {
            let msg = d
                .get("message")
                .or_else(|| d.get("title"))
                .and_then(Value::as_str)
                .unwrap_or("error dialog");
            return Err(Error::new(ErrorKind::Api, format!("quote error: {msg}")));
        }

        let mut fees = Vec::new();
        let mut discounts = Vec::new();
        let mut subtotal_line = None;
        for b in resp
            .get("breakdowns")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let key = b.get("key").and_then(Value::as_str).unwrap_or_default();
            let name = b.get("name").and_then(Value::as_str).unwrap_or(key);
            if key.starts_with("subtotal") {
                subtotal_line.get_or_insert(line_amount(b));
                continue;
            }
            if key.starts_with("total") {
                continue;
            }
            let details: Vec<&Value> = b
                .get("details")
                .and_then(Value::as_array)
                .map(|d| d.iter().collect())
                .unwrap_or_default();
            // Aggregate lines ("Applicable fees") are expanded into details.
            let lines: Vec<(String, String, Money)> = if details.is_empty() {
                vec![(key.to_string(), name.to_string(), line_amount(b))]
            } else {
                details
                    .iter()
                    .map(|d| {
                        let n = d.get("name").and_then(Value::as_str).unwrap_or(name);
                        let k = d.get("key").and_then(Value::as_str).unwrap_or(key);
                        (k.to_string(), n.to_string(), line_amount(d))
                    })
                    .collect()
            };
            for (k, n, amount) in lines {
                let line = Line {
                    key: k.clone(),
                    name: n.clone(),
                    amount,
                };
                if is_discount(&k, &n, amount) {
                    discounts.push(line);
                } else {
                    fees.push(line);
                }
            }
        }

        let subtotal = money_at(
            resp,
            &[
                "/foodQuote/priceInMinorUnit",
                "/martQuote/priceInMinorUnit",
                "/itemQuote/priceInMinorUnit",
                "/subtotalQuote/priceInMinorUnit",
            ],
        )
        .or(subtotal_line)
        .unwrap_or(Money::ZERO);
        let delivery_before = money_at(
            resp,
            &[
                "/deliveryQuote/priceInMinorUnit",
                "/deliveryQuote/priceV2/amountInMinor",
            ],
        )
        .unwrap_or(Money::ZERO);
        let delivery_after = money_at(
            resp,
            &[
                "/deliveryQuote/reducedPriceInMinorUnit",
                "/deliveryQuote/reducedPriceV2/amountInMinor",
            ],
        )
        .filter(|m| m.minor() != 0)
        .unwrap_or(delivery_before);
        let total_before = money_at(
            resp,
            &[
                "/totalQuote/priceInMinorUnit",
                "/totalQuote/priceV2/amountInMinor",
            ],
        )
        .unwrap_or(Money::ZERO);
        let total_after = money_at(
            resp,
            &[
                "/totalQuote/reducedPriceV2/amountInMinor",
                "/totalQuote/reducedPriceInMinorUnit",
            ],
        )
        .filter(|m| m.minor() != 0)
        .unwrap_or(total_before);

        let mut q = Quote {
            merchant_name: resp
                .pointer("/merchants/0/name")
                .and_then(Value::as_str)
                .unwrap_or(&cart.merchant_name)
                .to_string(),
            subtotal,
            delivery_before,
            delivery_after,
            fees,
            discounts,
            tax: money_at(resp, &["/totalQuote/taxV2/amountInMinor"]),
            total_before,
            total_after,
            eta_min: resp
                .pointer("/merchants/0/merchantETA/maxETAInMinute")
                .and_then(Value::as_i64),
            quote_signature: resp
                .get("quoteSignature")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            payment_type_id: payment_type_id.to_string(),
            address: resp
                .pointer("/destination/address")
                .and_then(Value::as_str)
                .unwrap_or(&loc.address)
                .to_string(),
            poi_id: resp
                .pointer("/destination/poiID")
                .and_then(Value::as_str)
                .unwrap_or(&loc.id)
                .to_string(),
            id: String::new(),
        };
        if q.quote_signature.is_empty() {
            return Err(Error::new(
                ErrorKind::Api,
                "quote response had no quoteSignature",
            ));
        }
        q.id = q.compute_id(cart);
        Ok(q)
    }

    /// Short stable id over the price-determining inputs (cart, address,
    /// payment, total), like GrabFood's. The signature is excluded because Grab
    /// mints a new one per call; `--yes <id>` must match a fresh re-quote.
    fn compute_id(&self, cart: &Cart) -> String {
        let canon = json!({
            "service": "mart",
            "merchant": cart.merchant_id,
            "items": cart.items.iter().map(|i| json!([i.item_id, i.quantity])).collect::<Vec<_>>(),
            "poiID": self.poi_id,
            "payment": self.payment_type_id,
            "total": self.total_after.minor(),
        });
        let bytes = serde_json::to_vec(&canon).unwrap_or_default();
        Sha256::digest(&bytes)
            .iter()
            .take(6)
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    pub fn to_json(&self) -> Value {
        let lines = |ls: &[Line]| {
            ls.iter()
                .map(|l| json!({"key": l.key, "name": l.name, "amount_minor": l.amount.minor()}))
                .collect::<Vec<_>>()
        };
        json!({
            "quote_id": self.id,
            "merchant": self.merchant_name,
            "subtotal_minor": self.subtotal.minor(),
            "delivery_before_minor": self.delivery_before.minor(),
            "delivery_after_minor": self.delivery_after.minor(),
            "fees": lines(&self.fees),
            "discounts": lines(&self.discounts),
            "tax_minor": self.tax.map(Money::minor),
            "total_before_minor": self.total_before.minor(),
            "total_minor": self.total_after.minor(),
            "total_php": self.total_after.as_major_f64(),
            "eta_min": self.eta_min,
            "payment_type_id": self.payment_type_id,
            "address": self.address,
        })
    }

    pub fn human(&self) -> Vec<String> {
        let mut out = vec![format!("{}  (quote {})", self.merchant_name, self.id)];
        out.push(format!("  subtotal      {:>12}", self.subtotal.peso()));
        if self.delivery_after != self.delivery_before {
            out.push(format!(
                "  delivery      {:>12}  (was {})",
                self.delivery_after.peso(),
                self.delivery_before.peso()
            ));
        } else {
            out.push(format!(
                "  delivery      {:>12}",
                self.delivery_after.peso()
            ));
        }
        for f in &self.fees {
            if !f.name.eq_ignore_ascii_case("delivery fee") {
                out.push(format!("  {:<13} {:>12}", f.name, f.amount.peso()));
            }
        }
        for d in &self.discounts {
            out.push(format!("  {:<13} {:>12}", d.name, d.amount.peso()));
        }
        if let Some(t) = self.tax.filter(|t| t.minor() != 0) {
            out.push(format!("  tax (incl.)   {:>12}", t.peso()));
        }
        out.push(format!(
            "  total         {:>12}  (before discounts {})",
            self.total_after.peso(),
            self.total_before.peso()
        ));
        if let Some(e) = self.eta_min {
            out.push(format!("  eta           {e} min"));
        }
        let pay = if self.payment_type_id.is_empty() {
            "(none set)"
        } else {
            &self.payment_type_id
        };
        out.push(format!("  payment       {pay}"));
        out.push(format!("  deliver to    {}", self.address));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cart::CartItem;

    fn loc() -> Location {
        serde_json::from_str(
            r#"{"id":"POI-X","latitude":1.0,"longitude":2.0,"address":"1 Example St","countryCode":"PH"}"#,
        )
        .expect("loc")
    }

    fn cart() -> Cart {
        Cart {
            merchant_id: "2-SR".into(),
            merchant_name: "S&R - Example Branch".into(),
            items: vec![CartItem {
                item_id: "PHITE1".into(),
                quantity: 2,
                name: "Red Bull 4 x 250ml".into(),
                unit_price_minor: 33900,
                campaign_id: Some("CAMP-ITEM".into()),
                comment: String::new(),
            }],
        }
    }

    /// Shape from the mart web bundle (totalQuote.reducedPriceV2, breakdowns).
    fn resp() -> Value {
        json!({
            "quoteSignature": "SIG-SECRET",
            "merchants": [{"name": "S&R - Example Branch", "merchantETA": {"maxETAInMinute": 70}}],
            "destination": {"address": "1 Example St", "poiID": "POI-X"},
            "deliveryQuote": {"priceInMinorUnit": 9900, "reducedPriceInMinorUnit": 4900},
            "totalQuote": {
                "priceInMinorUnit": 90700,
                "reducedPriceV2": {"amountInMinor": 85700, "amountDisplay": "857.00"},
                "taxV2": {"amountInMinor": 9183}
            },
            "breakdowns": [
                {"key": "subtotal", "name": "Subtotal", "value": {"amountInMinor": 67800}},
                {"key": "service_fees", "name": "Applicable fees", "details": [
                    {"name": "Delivery fee", "value": {"amountInMinor": 9900}},
                    {"name": "Service fee", "value": {"amountInMinor": 1900}},
                    {"name": "Small order fee", "value": {"amountInMinor": 2000}}
                ]},
                {"key": "stackable_offer", "name": "₱50 off delivery", "value": {"amountInMinor": -5000}}
            ]
        })
    }

    #[test]
    fn body_matches_web_shape() {
        let c = cart();
        let ids = campaign_ids(&["CAMP-STORE".into(), "CAMP-ITEM".into()], &c);
        assert_eq!(ids, vec!["CAMP-STORE".to_string(), "CAMP-ITEM".to_string()]);
        let b = build_body(&c, "SEC", &loc(), "PAY-1", &ids);
        assert_eq!(b["merchants"][0]["ID"], "2-SR");
        assert_eq!(b["merchants"][0]["section"], "SEC");
        assert_eq!(b["merchants"][0]["items"][0]["ID"], "PHITE1");
        assert_eq!(b["merchants"][0]["items"][0]["quantity"], 2);
        assert_eq!(b["paymentTypeID"], "PAY-1");
        assert_eq!(b["destination"]["poiID"], "POI-X");
        assert_eq!(b["destination"]["latitude"], 1.0);
        assert_eq!(b["promoCode"], json!([]));
        assert_eq!(b["campaignIDs"][1], "CAMP-ITEM");
    }

    #[test]
    fn redaction_hides_address() {
        let b = redacted(&build_body(&cart(), "", &loc(), "PAY-1", &[]));
        let s = serde_json::to_string(&b).expect("json");
        assert!(!s.contains("1 Example St"));
        assert!(!s.contains("POI-X"));
        assert_eq!(b["destination"]["latitude"], "<redacted>");
        assert_eq!(b["merchants"][0]["ID"], "2-SR");
    }

    #[test]
    fn parses_quote() -> Result<()> {
        let q = Quote::parse(&resp(), &cart(), &loc(), "PAY-1")?;
        assert_eq!(q.subtotal, Money(67800));
        assert_eq!(q.delivery_before, Money(9900));
        assert_eq!(q.delivery_after, Money(4900));
        assert_eq!(q.total_after, Money(85700));
        assert_eq!(q.total_before, Money(90700));
        assert_eq!(q.tax, Some(Money(9183)));
        let names: Vec<&str> = q.fees.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Delivery fee", "Service fee", "Small order fee"]
        );
        assert_eq!(q.discounts.len(), 1);
        assert_eq!(q.discounts[0].amount, Money(-5000));
        assert_eq!(q.eta_min, Some(70));
        assert_eq!(q.id.len(), 12);
        let text = q.human().join("\n");
        assert!(text.contains("Small order fee"));
        assert!(text.contains("₱857.00"));
        assert!(!text.contains("SIG-SECRET"));
        Ok(())
    }

    #[test]
    fn quote_id_stable_and_sensitive() -> Result<()> {
        let a = Quote::parse(&resp(), &cart(), &loc(), "PAY-1")?;
        let b = Quote::parse(&resp(), &cart(), &loc(), "PAY-1")?;
        assert_eq!(a.id, b.id);
        let c = Quote::parse(&resp(), &cart(), &loc(), "PAY-2")?;
        assert_ne!(a.id, c.id);
        let mut more = cart();
        more.items[0].quantity = 3;
        assert_ne!(a.id, Quote::parse(&resp(), &more, &loc(), "PAY-1")?.id);
        Ok(())
    }

    #[test]
    fn errors_surface() {
        let e = Quote::parse(
            &json!({"errMessage": "item unavailable"}),
            &cart(),
            &loc(),
            "P",
        )
        .expect_err("err");
        assert!(e.message.contains("item unavailable"));
        assert!(Quote::parse(&json!({"totalQuote": {}}), &cart(), &loc(), "P").is_err());
    }
}
