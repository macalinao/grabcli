//! A GrabFood cart: one merchant at a time, validated against the live menu
//! before quoting. The library passes carts by value; where one is kept is
//! the caller's choice (the `grab` CLI stores `<state>/food/cart.json`).

use grabcli_core::error::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::menu::Menu;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CartModifier {
    pub group_id: String,
    pub modifier_id: String,
    #[serde(default = "one")]
    pub quantity: i64,
}

fn one() -> i64 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CartItem {
    pub item_id: String,
    pub quantity: i64,
    #[serde(default)]
    pub modifiers: Vec<CartModifier>,
    #[serde(default)]
    pub comment: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cart {
    pub merchant_id: String,
    pub section: String,
    pub items: Vec<CartItem>,
    /// Promo code applied to the cart (plain code string), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voucher: Option<String>,
}

impl Cart {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Build the `merchants` array entry for the quote/cart request body.
    pub fn to_quote_merchant(&self) -> Value {
        let items: Vec<Value> = self
            .items
            .iter()
            .map(|it| {
                // Group cart modifiers by their group id.
                let mut groups: std::collections::BTreeMap<&str, Vec<Value>> =
                    std::collections::BTreeMap::new();
                for m in &it.modifiers {
                    groups
                        .entry(m.group_id.as_str())
                        .or_default()
                        .push(serde_json::json!({"ID": m.modifier_id, "quantity": m.quantity}));
                }
                let modifier_groups: Vec<Value> = groups
                    .into_iter()
                    .map(|(gid, mods)| serde_json::json!({"ID": gid, "modifiers": mods}))
                    .collect();
                serde_json::json!({
                    "ID": it.item_id,
                    "quantity": it.quantity,
                    "modifierGroups": modifier_groups,
                    "comment": it.comment,
                })
            })
            .collect();
        serde_json::json!({
            "ID": self.merchant_id,
            "section": self.section,
            "items": items,
        })
    }

    /// Validate the cart against a freshly fetched menu: items exist and are
    /// available, and every required modifier group is satisfied.
    pub fn validate(&self, menu: &Menu) -> Result<()> {
        if self.is_empty() {
            return Err(Error::new(ErrorKind::CartInvalid, "cart is empty"));
        }
        if self.merchant_id != menu.merchant_id {
            return Err(Error::new(
                ErrorKind::CartInvalid,
                format!(
                    "cart is for merchant {} but menu is {}",
                    self.merchant_id, menu.merchant_id
                ),
            ));
        }
        for ci in &self.items {
            if ci.quantity < 1 || ci.quantity > 99 {
                return Err(Error::new(
                    ErrorKind::CartInvalid,
                    format!("item {} quantity must be 1..=99", ci.item_id),
                ));
            }
            let item = menu.item(&ci.item_id).ok_or_else(|| {
                Error::new(
                    ErrorKind::CartInvalid,
                    format!("item {} is not on the menu", ci.item_id),
                )
            })?;
            if !item.available {
                return Err(Error::new(
                    ErrorKind::CartInvalid,
                    format!("item {} ({}) is unavailable", item.id, item.name),
                ));
            }
            for grp in &item.modifier_groups {
                let picked = ci.modifiers.iter().filter(|m| m.group_id == grp.id).count() as i64;
                if grp.required() && picked < grp.min {
                    return Err(Error::new(
                        ErrorKind::CartInvalid,
                        format!(
                            "item {} needs {}..={} choice(s) for required group \"{}\" (got {picked}); \
                             add with --mod {}:<option-id>",
                            item.name, grp.min, grp.max, grp.name, grp.id
                        ),
                    ));
                }
                if picked > grp.max {
                    return Err(Error::new(
                        ErrorKind::CartInvalid,
                        format!(
                            "item {} allows at most {} choice(s) for group \"{}\" (got {picked})",
                            item.name, grp.max, grp.name
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::Menu;

    fn menu() -> Menu {
        let v = serde_json::json!({
          "merchant": {"ID": "2-EX", "name": "Ex - D", "section": "S1", "isIntegrated": true,
            "openingHours": {"open": true, "displayedHours": "x"},
            "menu": {"categories": [{"name": "C", "items": [
              {"ID": "I1", "name": "Bowl", "priceInMinorUnit": 36000, "available": true,
               "modifierGroups": [{"ID": "G1", "name": "Spice", "selectionType": 0,
                 "selectionRangeMin": 1, "selectionRangeMax": 1,
                 "modifiers": [{"ID": "M1", "name": "Mild"}, {"ID": "M2", "name": "Hot"}]}]}
            ]}]}}
        });
        Menu::parse(&v).expect("parse")
    }

    #[test]
    fn requires_modifier_choice() {
        let cart = Cart {
            merchant_id: "2-EX".into(),
            section: "S1".into(),
            items: vec![CartItem {
                item_id: "I1".into(),
                quantity: 1,
                modifiers: vec![],
                comment: String::new(),
            }],
            voucher: None,
        };
        let e = cart.validate(&menu()).expect_err("should error");
        assert_eq!(e.kind, ErrorKind::CartInvalid);
    }

    #[test]
    fn valid_with_choice() -> Result<()> {
        let cart = Cart {
            merchant_id: "2-EX".into(),
            section: "S1".into(),
            items: vec![CartItem {
                item_id: "I1".into(),
                quantity: 1,
                modifiers: vec![CartModifier {
                    group_id: "G1".into(),
                    modifier_id: "M1".into(),
                    quantity: 1,
                }],
                comment: String::new(),
            }],
            voucher: None,
        };
        cart.validate(&menu())?;
        let body = cart.to_quote_merchant();
        assert_eq!(body["items"][0]["modifierGroups"][0]["ID"], "G1");
        assert_eq!(
            body["items"][0]["modifierGroups"][0]["modifiers"][0]["ID"],
            "M1"
        );
        Ok(())
    }

    #[test]
    fn empty_cart_invalid() {
        let e = Cart::default().validate(&menu()).expect_err("should error");
        assert_eq!(e.kind, ErrorKind::CartInvalid);
    }
}
