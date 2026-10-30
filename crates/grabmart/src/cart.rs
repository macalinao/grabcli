//! A GrabMart cart: one store at a time, separate from the GrabFood cart.
//! A value the caller keeps (the `grab` CLI stores `<state>/mart/cart.json`). Never
//! cached; it only records ids, quantities and a price snapshot for display.

use grabcli_core::error::{Error, ErrorKind, Result};
use grabcli_core::money::Money;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CartItem {
    pub item_id: String,
    pub quantity: i64,
    /// Name and unit price when added (display only; the quote is authoritative).
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub unit_price_minor: i64,
    /// Item-level campaign (discount) id, sent in `campaignIDs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign_id: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub comment: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cart {
    pub merchant_id: String,
    #[serde(default)]
    pub merchant_name: String,
    pub items: Vec<CartItem>,
}

impl Cart {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Add `item` (merging quantities for the same item id). Refuses a second
    /// store: the cart holds one store at a time.
    pub fn add(&mut self, merchant_id: &str, merchant_name: &str, item: CartItem) -> Result<()> {
        if !self.is_empty() && self.merchant_id != merchant_id {
            return Err(Error::new(
                ErrorKind::CartInvalid,
                format!(
                    "the mart cart already has items from {} — run `grab mart cart clear` first",
                    if self.merchant_name.is_empty() {
                        &self.merchant_id
                    } else {
                        &self.merchant_name
                    }
                ),
            ));
        }
        if item.quantity < 1 || item.quantity > 99 {
            return Err(Error::new(
                ErrorKind::CartInvalid,
                "quantity must be 1..=99",
            ));
        }
        self.merchant_id = merchant_id.to_string();
        if !merchant_name.is_empty() {
            self.merchant_name = merchant_name.to_string();
        }
        if let Some(existing) = self.items.iter_mut().find(|i| i.item_id == item.item_id) {
            existing.quantity = (existing.quantity + item.quantity).min(99);
            existing.unit_price_minor = item.unit_price_minor;
            existing.campaign_id = item.campaign_id;
        } else {
            self.items.push(item);
        }
        Ok(())
    }

    /// Remove an item; returns false when it wasn't in the cart.
    pub fn remove(&mut self, item_id: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|i| i.item_id != item_id);
        self.items.len() != before
    }

    /// Price snapshot from when items were added (display only).
    pub fn snapshot_subtotal(&self) -> Money {
        Money(
            self.items
                .iter()
                .map(|i| i.unit_price_minor * i.quantity)
                .sum(),
        )
    }

    /// Basic validation before quoting.
    pub fn validate(&self) -> Result<()> {
        if self.is_empty() {
            return Err(Error::new(ErrorKind::CartInvalid, "mart cart is empty"));
        }
        for i in &self.items {
            if i.quantity < 1 || i.quantity > 99 {
                return Err(Error::new(
                    ErrorKind::CartInvalid,
                    format!("item {} quantity must be 1..=99", i.item_id),
                ));
            }
        }
        Ok(())
    }

    /// The `merchants[0]` entry of the quote body, mirroring the web's
    /// `{ID, items:[{ID, quantity, comment?}], section}`. Mart items have no
    /// modifier groups.
    pub fn to_quote_merchant(&self, section: &str) -> Value {
        let items: Vec<Value> = self
            .items
            .iter()
            .map(|it| {
                let mut v = json!({"ID": it.item_id, "quantity": it.quantity});
                if !it.comment.is_empty() {
                    v["comment"] = json!(it.comment);
                }
                v
            })
            .collect();
        json!({"ID": self.merchant_id, "items": items, "section": section})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, qty: i64) -> CartItem {
        CartItem {
            item_id: id.into(),
            quantity: qty,
            name: format!("Item {id}"),
            unit_price_minor: 33900,
            campaign_id: None,
            comment: String::new(),
        }
    }

    #[test]
    fn add_merges_and_rejects_second_store() -> Result<()> {
        let mut c = Cart::default();
        c.add("2-SR", "S&R - Example Branch", item("I1", 1))?;
        c.add("2-SR", "", item("I1", 2))?;
        assert_eq!(c.items.len(), 1);
        assert_eq!(c.items[0].quantity, 3);
        assert_eq!(c.merchant_name, "S&R - Example Branch");
        assert_eq!(c.snapshot_subtotal(), Money(101700));
        let e = c
            .add("2-OTHER", "Other", item("I2", 1))
            .expect_err("second store");
        assert_eq!(e.kind, ErrorKind::CartInvalid);
        Ok(())
    }

    #[test]
    fn bad_quantity_and_empty() {
        let mut c = Cart::default();
        assert!(c.add("2-SR", "S&R", item("I1", 0)).is_err());
        assert!(c.validate().is_err());
    }

    #[test]
    fn remove_and_body() -> Result<()> {
        let mut c = Cart::default();
        c.add("2-SR", "S&R", item("I1", 1))?;
        c.add("2-SR", "S&R", item("I2", 2))?;
        assert!(c.remove("I1"));
        assert!(!c.remove("I1"));
        let m = c.to_quote_merchant("SEC");
        assert_eq!(m["ID"], "2-SR");
        assert_eq!(m["section"], "SEC");
        assert_eq!(m["items"][0]["ID"], "I2");
        assert_eq!(m["items"][0]["quantity"], 2);
        assert!(m["items"][0].get("modifierGroups").is_none());
        Ok(())
    }
}
