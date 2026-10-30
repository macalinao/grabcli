//! Menu parsing: categories, items, modifier groups, merchant hours/ETA and
//! the offer carousel.

use grabcli_core::error::{Error, Result};
use grabcli_core::money::Money;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct Modifier {
    pub id: String,
    pub name: String,
    pub price: Money,
    pub available: bool,
    pub is_default: bool,
}

#[derive(Debug, Clone)]
pub struct ModifierGroup {
    pub id: String,
    pub name: String,
    pub available: bool,
    /// 0 = single (radio), 1 = multiple, 2 = stepper.
    pub selection_type: i64,
    pub min: i64,
    pub max: i64,
    pub modifiers: Vec<Modifier>,
}

impl ModifierGroup {
    /// A group must be chosen when its minimum selection is > 0.
    pub fn required(&self) -> bool {
        self.min > 0
    }
}

#[derive(Debug, Clone)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub category: String,
    pub price: Money,
    pub available: bool,
    pub description: String,
    pub image_url: Option<String>,
    pub modifier_groups: Vec<ModifierGroup>,
}

#[derive(Debug, Clone)]
pub struct Offer {
    pub title: String,
    pub subtitle: String,
    pub redemption_uuid: Option<String>,
    pub offer_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Menu {
    pub merchant_id: String,
    pub name: String,
    pub section: String,
    pub open: bool,
    pub hours: String,
    pub eta_min: Option<i64>,
    pub is_integrated: bool,
    pub items: Vec<Item>,
    pub offers: Vec<Offer>,
}

fn money_of(v: &Value, key: &str) -> Money {
    Money(v.get(key).and_then(Value::as_i64).unwrap_or(0))
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn parse_modifier(v: &Value) -> Modifier {
    Modifier {
        id: str_of(v, "ID"),
        name: str_of(v, "name"),
        price: money_of(v, "priceInMinorUnit"),
        available: v.get("available").and_then(Value::as_bool).unwrap_or(true),
        is_default: v.get("isDefault").and_then(Value::as_bool).unwrap_or(false),
    }
}

fn parse_group(v: &Value) -> ModifierGroup {
    ModifierGroup {
        id: str_of(v, "ID"),
        name: str_of(v, "name"),
        available: v.get("available").and_then(Value::as_bool).unwrap_or(true),
        selection_type: v.get("selectionType").and_then(Value::as_i64).unwrap_or(0),
        // Grab omits min when 0 and sometimes omits max; default max to 1.
        min: v
            .get("selectionRangeMin")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        max: v
            .get("selectionRangeMax")
            .and_then(Value::as_i64)
            .unwrap_or(1),
        modifiers: v
            .get("modifiers")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(parse_modifier).collect())
            .unwrap_or_default(),
    }
}

impl Menu {
    pub fn parse(root: &Value) -> Result<Self> {
        let m = root
            .get("merchant")
            .ok_or_else(|| Error::parse("menu response has no `merchant`"))?;

        let hours = m.get("openingHours");
        let mut items = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let cats = m
            .pointer("/menu/categories")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for cat in &cats {
            let cat_name = str_of(cat, "name");
            for it in cat
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let id = str_of(it, "ID");
                if id.is_empty() || !seen.insert(id.clone()) {
                    continue;
                }
                let groups = it
                    .get("modifierGroups")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(parse_group).collect())
                    .unwrap_or_default();
                items.push(Item {
                    id,
                    name: str_of(it, "name"),
                    category: cat_name.clone(),
                    price: money_of(it, "priceInMinorUnit"),
                    available: it.get("available").and_then(Value::as_bool).unwrap_or(true),
                    description: str_of(it, "description"),
                    image_url: it
                        .get("imgHref")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(ToString::to_string),
                    modifier_groups: groups,
                });
            }
        }

        let offers = m
            .pointer("/offerCarousel/offerHighlights")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(parse_offer).collect())
            .unwrap_or_default();

        Ok(Menu {
            merchant_id: str_of(m, "ID"),
            name: str_of(m, "name"),
            section: str_of(m, "section"),
            open: hours
                .and_then(|h| h.get("open"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            hours: hours
                .map(|h| str_of(h, "displayedHours"))
                .unwrap_or_default(),
            eta_min: m.get("ETA").and_then(Value::as_i64),
            is_integrated: m
                .get("isIntegrated")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            items,
            offers,
        })
    }

    pub fn item(&self, id: &str) -> Option<&Item> {
        self.items.iter().find(|i| i.id == id)
    }
}

fn parse_offer(v: &Value) -> Option<Offer> {
    let h = v.get("highlight")?;
    let meta: Value = v
        .get("meta")
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null);
    Some(Offer {
        title: str_of(h, "title"),
        subtitle: str_of(h, "subtitle"),
        redemption_uuid: meta
            .get("redemptionUUID")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        offer_id: meta
            .get("offerID")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        // Synthetic menu matching the real schema; invented IDs/names/prices.
        serde_json::json!({
          "merchant": {
            "ID": "2-EXAMPLE123",
            "name": "Example Noodles - Downtown",
            "section": "SEC-EXAMPLE",
            "isIntegrated": true,
            "ETA": 40,
            "openingHours": {"open": true, "displayedHours": "11:00-22:00"},
            "menu": {"categories": [
              {"name": "Bowls", "items": [
                {"ID": "ITEM-1", "name": "Karaage Bowl", "priceInMinorUnit": 36000, "available": true,
                 "modifierGroups": [
                   {"ID": "GRP-SPICE", "name": "Spice level", "selectionType": 0,
                    "selectionRangeMin": 1, "selectionRangeMax": 1,
                    "modifiers": [
                      {"ID": "MOD-MILD", "name": "Mild", "priceInMinorUnit": 0, "isDefault": true},
                      {"ID": "MOD-HOT", "name": "Hot", "priceInMinorUnit": 0}
                    ]}
                 ]}
              ]}
            ]},
            "offerCarousel": {"offerHighlights": [
              {"highlight": {"title": "₱49 off Delivery", "subtitle": "Min spend ₱390"},
               "meta": "{\"redemptionUUID\":\"999\"}"}
            ]}
          }
        })
    }

    #[test]
    fn parses_items_modifiers_offers() -> Result<()> {
        let menu = Menu::parse(&fixture())?;
        assert_eq!(menu.name, "Example Noodles - Downtown");
        assert_eq!(menu.section, "SEC-EXAMPLE");
        assert!(menu.open);
        let item = menu.item("ITEM-1").ok_or_else(|| Error::parse("item"))?;
        assert_eq!(item.price, Money(36000));
        let grp = &item.modifier_groups[0];
        assert!(grp.required());
        assert_eq!(grp.modifiers.len(), 2);
        assert!(grp.modifiers[0].is_default);
        assert_eq!(menu.offers[0].redemption_uuid.as_deref(), Some("999"));
        Ok(())
    }
}
