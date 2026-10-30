//! Parsing of GrabMart responses into small, display-ready structs.
//!
//! Field names follow docs/mart.md; every parser is lenient (missing fields
//! become defaults) because the mart API is undocumented and varies between
//! endpoints (`id` vs `ID`, `subDepartments[].id`, ...).

use grabcli_core::money::Money;
use serde::Serialize;
use serde_json::{Value, json};

fn s(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn i(v: &Value, ptr: &str) -> Option<i64> {
    v.pointer(ptr).and_then(Value::as_i64)
}

fn arr<'a>(v: &'a Value, ptr: &str) -> &'a [Value] {
    v.pointer(ptr)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// A product (an item in a store's catalogue).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Product {
    pub id: String,
    pub name: String,
    pub merchant_id: String,
    /// List price.
    pub price: Money,
    /// Discounted price when lower than the list price.
    pub discounted: Option<Money>,
    pub discount_label: Option<String>,
    /// `available` (catalogue flag).
    pub available: bool,
    /// `soAvailable` (stock-out flag) when present. Grab exposes no stock
    /// counts, only these booleans.
    pub in_stock: Option<bool>,
    /// Pack size parsed from the name, else the item's "Size" attribute.
    pub size: Option<String>,
    pub campaign_id: Option<String>,
    pub department_id: Option<String>,
    pub barcode: Option<String>,
}

impl Product {
    pub fn parse(v: &Value) -> Option<Self> {
        let id = s(v, "/ID").or_else(|| s(v, "/id"))?;
        let name = s(v, "/name").unwrap_or_default();
        let price = Money(
            i(v, "/priceInMinorUnit")
                .or_else(|| i(v, "/priceV2/amountInMinor"))
                .unwrap_or(0),
        );
        let discounted = i(v, "/discountedPriceInMin")
            .or_else(|| i(v, "/discountedPriceV2/amountInMinor"))
            .map(Money)
            .filter(|d| d.minor() > 0 && *d < price);
        let size = pack_size(&name).or_else(|| size_attribute(v));
        Some(Self {
            id,
            merchant_id: s(v, "/merchantID").unwrap_or_default(),
            price,
            discounted,
            discount_label: s(v, "/campaignName").or_else(|| s(v, "/discountPercentage")),
            available: v.get("available").and_then(Value::as_bool).unwrap_or(true),
            in_stock: v.get("soAvailable").and_then(Value::as_bool),
            size,
            campaign_id: s(v, "/campaignID"),
            department_id: s(v, "/searchMeta/departmentID").or_else(|| s(v, "/itemClassID")),
            barcode: s(v, "/barcode"),
            name,
        })
    }

    /// What the customer pays per unit.
    pub fn effective_price(&self) -> Money {
        self.discounted.unwrap_or(self.price)
    }

    /// Available and not stocked out.
    pub fn orderable(&self) -> bool {
        self.available && self.in_stock != Some(false)
    }

    pub fn availability(&self) -> &'static str {
        match (self.available, self.in_stock) {
            (false, _) => "unavailable",
            (true, Some(false)) => "out of stock",
            _ => "available",
        }
    }

    /// `₱339.00` or `₱99.50 (was ₱199.00)`.
    pub fn price_text(&self) -> String {
        match self.discounted {
            Some(d) => format!("{} (was {})", d.peso(), self.price.peso()),
            None => self.price.peso(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "merchant_id": self.merchant_id,
            "price_minor": self.price.minor(),
            "discounted_price_minor": self.discounted.map(Money::minor),
            "effective_price_minor": self.effective_price().minor(),
            "discount_label": self.discount_label,
            "available": self.available,
            "in_stock": self.in_stock,
            "availability": self.availability(),
            "size": self.size,
            "campaign_id": self.campaign_id,
            "barcode": self.barcode,
        })
    }

    /// One human line: `ID  price  name  [size]  (availability)`.
    pub fn line(&self) -> String {
        let size = self
            .size
            .as_deref()
            .map(|s| format!("  [{s}]"))
            .unwrap_or_default();
        let avail = format!("  ({})", self.availability());
        format!(
            "{}  {:>24}  {}{size}{avail}",
            self.id,
            self.price_text(),
            self.name
        )
    }
}

/// The "Size" attribute from item detail (`itemAttributeGroupsV2`).
fn size_attribute(v: &Value) -> Option<String> {
    for g in arr(v, "/itemAttributeGroupsV2") {
        for a in arr(g, "/attributes") {
            let id = s(a, "/id").unwrap_or_default().to_ascii_lowercase();
            if id == "size" || id == "net weight" || id == "volume" {
                let c = s(a, "/content")?;
                let c = c.trim_start_matches("About ").trim().to_string();
                return (!c.is_empty()).then_some(c);
            }
        }
    }
    None
}

const UNITS: &[&str] = &[
    "ml", "l", "ltr", "liter", "liters", "litre", "cl", "g", "gm", "gms", "grams", "kg", "mg",
    "oz", "lb", "lbs", "pc", "pcs", "piece", "pieces", "pack", "packs", "pk", "s", "bottle",
    "bottles", "can", "cans", "sachet", "sachets", "tabs", "tablets", "capsules", "caps", "rolls",
    "roll", "sheets", "ply", "bags", "tea bags", "count", "ct",
];

/// Pull pack-size tokens out of a product name: `4 x 250ml`, `473mL`,
/// `6 bottles`, `5x74g`, `approx. 1.2kg`. Returns them joined with spaces, or
/// `None` when there are none.
pub fn pack_size(name: &str) -> Option<String> {
    let chars: Vec<char> = name.chars().collect();
    let n = chars.len();
    let mut found: Vec<String> = Vec::new();
    let mut idx = 0;
    while idx < n {
        let starts_number = chars[idx].is_ascii_digit()
            && (idx == 0 || !chars[idx - 1].is_alphanumeric() && chars[idx - 1] != '.');
        if !starts_number {
            idx += 1;
            continue;
        }
        let start = idx;
        let read_num = |mut j: usize| {
            while j < n && (chars[j].is_ascii_digit() || chars[j] == '.' || chars[j] == ',') {
                j += 1;
            }
            j
        };
        let skip_ws = |mut j: usize| {
            while j < n && chars[j] == ' ' {
                j += 1;
            }
            j
        };
        let mut j = read_num(idx);
        // Optional multiplier: "4 x 250ml", "5x74g".
        let k = skip_ws(j);
        if k < n && (chars[k] == 'x' || chars[k] == 'X' || chars[k] == '×') {
            let m = skip_ws(k + 1);
            if m < n && chars[m].is_ascii_digit() {
                j = read_num(m);
            }
        }
        let u0 = skip_ws(j);
        let mut u1 = u0;
        while u1 < n && chars[u1].is_alphabetic() {
            u1 += 1;
        }
        let unit: String = chars[u0..u1].iter().collect::<String>().to_lowercase();
        if !unit.is_empty() && UNITS.contains(&unit.as_str()) {
            let tok: String = chars[start..u1].iter().collect();
            found.push(tok);
            idx = u1;
        } else {
            idx = j.max(idx + 1);
        }
    }
    (!found.is_empty()).then(|| found.join(" "))
}

/// A store (merchant) from search, category or home-feed lists.
#[derive(Debug, Clone, Serialize)]
pub struct Store {
    pub id: String,
    pub name: String,
    pub chain: Option<String>,
    pub cuisine: Option<String>,
    pub status: Option<String>,
    pub open: Option<bool>,
    pub eta_min: Option<i64>,
    pub distance_km: Option<f64>,
    pub delivery_fee: Option<Money>,
    pub rating: Option<f64>,
    pub highlights: Vec<String>,
    /// Matched products (`dishes[]`) when the list came from a keyword search.
    pub products: Vec<Product>,
}

impl Store {
    pub fn parse(v: &Value) -> Option<Self> {
        let id = s(v, "/id").or_else(|| s(v, "/ID"))?;
        let name = s(v, "/address/name")
            .or_else(|| s(v, "/name"))
            .or_else(|| s(v, "/displayInfo/primaryText"))
            .unwrap_or_default();
        let cuisine = arr(v, "/merchantBrief/cuisine")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let highlights = arr(v, "/highlights")
            .iter()
            .filter_map(|h| s(h, "/title"))
            .collect();
        Some(Self {
            id,
            name,
            chain: s(v, "/chainName"),
            cuisine: (!cuisine.is_empty()).then_some(cuisine),
            status: s(v, "/merchantStatusInfo/status"),
            open: v
                .pointer("/merchantBrief/openHours/open")
                .and_then(Value::as_bool),
            eta_min: i(v, "/estimatedDeliveryTime"),
            distance_km: v
                .pointer("/merchantBrief/distanceInKm")
                .and_then(Value::as_f64),
            delivery_fee: i(v, "/estimatedDeliveryFee/price").map(Money),
            rating: v.pointer("/merchantBrief/rating").and_then(Value::as_f64),
            highlights,
            products: arr(v, "/dishes")
                .iter()
                .filter_map(Product::parse)
                .collect(),
        })
    }

    pub fn status_text(&self) -> String {
        match (&self.status, self.open) {
            (Some(s), _) => s.clone(),
            (None, Some(true)) => "OPEN".into(),
            (None, Some(false)) => "CLOSED".into(),
            (None, None) => "?".into(),
        }
    }

    pub fn line(&self) -> String {
        let mut parts = vec![
            format!("{}  {}", self.id, self.name),
            format!("[{}]", self.status_text()),
        ];
        if let Some(e) = self.eta_min {
            parts.push(format!("{e} min"));
        }
        if let Some(d) = self.distance_km {
            parts.push(format!("{d:.1} km"));
        }
        if let Some(f) = self.delivery_fee {
            parts.push(format!("delivery {}", f.peso()));
        }
        if let Some(c) = &self.cuisine {
            parts.push(c.clone());
        }
        parts.join("  ")
    }

    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "chain": self.chain,
            "cuisine": self.cuisine,
            "status": self.status_text(),
            "eta_min": self.eta_min,
            "distance_km": self.distance_km,
            "delivery_fee_minor": self.delivery_fee.map(Money::minor),
            "rating": self.rating,
            "highlights": self.highlights,
            "products": self.products.iter().map(Product::to_json).collect::<Vec<_>>(),
        })
    }
}

/// A list of stores plus paging info.
#[derive(Debug, Clone, Default)]
pub struct StoreList {
    pub title: Option<String>,
    pub stores: Vec<Store>,
    pub total: Option<i64>,
    pub next_offset: Option<i64>,
}

impl StoreList {
    /// `/search` and `/category` responses (`searchResult.searchMerchants`).
    pub fn parse_search(v: &Value) -> Self {
        Self {
            title: s(v, "/categoryName"),
            stores: arr(v, "/searchResult/searchMerchants")
                .iter()
                .filter_map(Store::parse)
                .collect(),
            total: i(v, "/searchResult/totalCount"),
            next_offset: if v
                .pointer("/searchResult/hasMore")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                i(v, "/searchResult/nextOffset")
            } else {
                None
            },
        }
    }

    /// The home feed's "more stores" (`MERCHANT`) widget.
    pub fn parse_homefeed(v: &Value) -> Self {
        let mut out = Self::default();
        for f in arr(v, "/feeds") {
            if f.get("type").and_then(Value::as_str) == Some("MERCHANT") {
                out.title = s(f, "/title");
                out.stores.extend(
                    arr(f, "/merchantWidget/merchants")
                        .iter()
                        .filter_map(Store::parse),
                );
            }
        }
        if v.get("hasMore").and_then(Value::as_bool).unwrap_or(false) {
            out.next_offset = i(v, "/nextOffset");
        }
        out
    }

    pub fn to_json(&self) -> Value {
        json!({
            "title": self.title,
            "total": self.total,
            "next_offset": self.next_offset,
            "stores": self.stores.iter().map(Store::to_json).collect::<Vec<_>>(),
        })
    }
}

/// A store category ("shopping mission") or a cross-store department.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Category {
    pub id: String,
    pub name: String,
}

/// Home-feed categories: store categories (for `stores --category`) and
/// cross-store product departments.
#[derive(Debug, Clone, Default)]
pub struct Categories {
    pub store_categories: Vec<Category>,
    pub departments: Vec<Category>,
}

impl Categories {
    pub fn parse_homefeed(v: &Value) -> Self {
        let mut out = Self::default();
        for f in arr(v, "/feeds") {
            match f.get("type").and_then(Value::as_str) {
                Some("CATEGORY") => {
                    for c in arr(f, "/categoryWidget/categories") {
                        // Deep links (Mart/Mall entry tiles) aren't categories.
                        if c.get("categoryShortcutType").and_then(Value::as_str)
                            == Some("CUSTOM_ACTION")
                        {
                            continue;
                        }
                        if let (Some(id), Some(name)) = (s(c, "/id"), s(c, "/name")) {
                            out.store_categories.push(Category { id, name });
                        }
                    }
                }
                Some("ITEMCATEGORY") => {
                    let featured = arr(f, "/itemCategoryWidget/categories");
                    let all = arr(f, "/itemCategoryWidget/allCategories");
                    for c in featured.iter().chain(all) {
                        if let (Some(id), Some(name)) = (s(c, "/id"), s(c, "/name"))
                            && !out.departments.iter().any(|d| d.id == id)
                        {
                            out.departments.push(Category { id, name });
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }
}

/// A department in a store's catalogue.
#[derive(Debug, Clone)]
pub struct Department {
    pub id: String,
    pub name: String,
    /// `1` = synthetic "Discounted items", `2` = "For You", else a real one.
    pub department_type: i64,
    pub available: bool,
    pub has_more: bool,
    pub items: Vec<Product>,
}

/// A store page (`/merchants/{id}`).
#[derive(Debug, Clone)]
pub struct StorePage {
    pub id: String,
    pub name: String,
    pub cuisine: Option<String>,
    pub status: Option<String>,
    pub open: Option<bool>,
    pub hours: Option<String>,
    pub eta_min: Option<i64>,
    pub distance_km: Option<f64>,
    pub delivery_fee: Option<Money>,
    /// Needed by the cart body; absent from the public response.
    pub section: Option<String>,
    pub is_integrated: Option<bool>,
    pub order_value_limit: Option<Money>,
    /// Store-level campaigns: (id, name).
    pub campaigns: Vec<(String, String)>,
    pub departments: Vec<Department>,
}

impl StorePage {
    pub fn parse(v: &Value) -> Option<Self> {
        let m = v.get("merchant").filter(|m| !m.is_null())?;
        let id = s(m, "/ID")?;
        let departments = arr(m, "/menu/departments")
            .iter()
            .filter_map(|d| {
                // `elementCards[].item` carries the discounted price that the
                // plain `items[]` copy lacks; prefer it.
                let mut items: Vec<Product> = arr(d, "/elementCards")
                    .iter()
                    .filter_map(|c| c.get("item").and_then(Product::parse))
                    .collect();
                if items.is_empty() {
                    items = arr(d, "/items").iter().filter_map(Product::parse).collect();
                }
                Some(Department {
                    id: s(d, "/ID")?,
                    name: s(d, "/name").unwrap_or_default(),
                    department_type: i(d, "/departmentType").unwrap_or(0),
                    available: d.get("available").and_then(Value::as_bool).unwrap_or(true),
                    has_more: d.get("hasMore").and_then(Value::as_bool).unwrap_or(false),
                    items,
                })
            })
            .collect();
        Some(Self {
            id,
            name: s(m, "/name").unwrap_or_default(),
            cuisine: s(m, "/cuisine"),
            status: s(m, "/status"),
            open: m
                .pointer("/openingHours/open")
                .and_then(Value::as_bool)
                .or_else(|| {
                    s(m, "/openingHours/displayedHours")
                        .filter(|h| h.eq_ignore_ascii_case("closed"))
                        .map(|_| false)
                }),
            hours: s(m, "/openingHours/displayedHours"),
            eta_min: i(m, "/ETA"),
            distance_km: m.get("distanceInKm").and_then(Value::as_f64),
            delivery_fee: i(m, "/estimatedDeliveryFee/price").map(Money),
            section: s(m, "/section"),
            is_integrated: m.get("isIntegrated").and_then(Value::as_bool),
            order_value_limit: i(m, "/menu/menuMeta/orderValueLimit").map(Money),
            campaigns: arr(m, "/menu/campaigns")
                .iter()
                .filter_map(|c| Some((s(c, "/ID")?, s(c, "/name").unwrap_or_default())))
                .collect(),
            departments,
        })
    }

    pub fn department(&self, id: &str) -> Option<&Department> {
        self.departments
            .iter()
            .find(|d| d.id == id || d.id.ends_with(&format!("-{id}")))
    }

    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "cuisine": self.cuisine,
            "status": self.status,
            "open": self.open,
            "hours": self.hours,
            "eta_min": self.eta_min,
            "distance_km": self.distance_km,
            "delivery_fee_minor": self.delivery_fee.map(Money::minor),
            "order_value_limit_minor": self.order_value_limit.map(Money::minor),
            "campaigns": self.campaigns.iter().map(|(id, n)| json!({"id": id, "name": n})).collect::<Vec<_>>(),
            "departments": self.departments.iter().map(|d| json!({
                "id": d.id, "name": d.name, "department_type": d.department_type,
                "available": d.available, "has_more": d.has_more,
                "items": d.items.iter().map(Product::to_json).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        })
    }
}

/// A page of products (`/merchants/{id}/items` or `/merchants/{id}/search`).
#[derive(Debug, Clone, Default)]
pub struct ProductPage {
    pub items: Vec<Product>,
    pub total: Option<i64>,
    pub next_offset: Option<i64>,
    pub sub_departments: Vec<Category>,
}

impl ProductPage {
    pub fn parse(v: &Value) -> Self {
        Self {
            items: arr(v, "/items").iter().filter_map(Product::parse).collect(),
            total: i(v, "/totalCount"),
            next_offset: if v.get("hasMore").and_then(Value::as_bool).unwrap_or(false) {
                i(v, "/nextOffset")
            } else {
                None
            },
            sub_departments: arr(v, "/subDepartments")
                .iter()
                .filter_map(|d| {
                    Some(Category {
                        id: s(d, "/id").or_else(|| s(d, "/ID"))?,
                        name: s(d, "/name").unwrap_or_default(),
                    })
                })
                .collect(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "total": self.total,
            "next_offset": self.next_offset,
            "sub_departments": self.sub_departments,
            "items": self.items.iter().map(Product::to_json).collect::<Vec<_>>(),
        })
    }
}

/// `/item/{id}`: one product with its attributes and store.
#[derive(Debug, Clone)]
pub struct ItemDetail {
    pub product: Product,
    pub merchant_name: Option<String>,
    pub merchant_open: Option<bool>,
    pub attributes: Vec<(String, String)>,
}

impl ItemDetail {
    pub fn parse(v: &Value) -> Option<Self> {
        let it = v.get("item")?;
        let mut product = Product::parse(it)?;
        if product.merchant_id.is_empty() {
            product.merchant_id = s(v, "/merchant/ID").unwrap_or_default();
        }
        let attributes = arr(it, "/itemAttributeGroupsV2")
            .iter()
            .flat_map(|g| arr(g, "/attributes"))
            .filter_map(|a| Some((s(a, "/title")?, s(a, "/content")?)))
            .collect();
        Some(Self {
            product,
            merchant_name: s(v, "/merchant/name"),
            merchant_open: v
                .pointer("/merchant/openingHours/open")
                .and_then(Value::as_bool),
            attributes,
        })
    }

    pub fn to_json(&self) -> Value {
        let mut j = self.product.to_json();
        j["merchant_name"] = json!(self.merchant_name);
        j["attributes"] = self
            .attributes
            .iter()
            .map(|(t, c)| json!({"title": t, "content": c}))
            .collect();
        j
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let p = format!("{}/../../docs/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let b = std::fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}"));
        serde_json::from_slice(&b).unwrap_or_else(|e| panic!("{p}: {e}"))
    }

    #[test]
    fn pack_sizes() {
        assert_eq!(
            pack_size("Red Bull Energy Drink 4 x 250ml").as_deref(),
            Some("4 x 250ml")
        );
        assert_eq!(
            pack_size("Monster Energy Drink 473mL").as_deref(),
            Some("473mL")
        );
        assert_eq!(
            pack_size("Pocari Sweat 6 bottles").as_deref(),
            Some("6 bottles")
        );
        assert_eq!(
            pack_size("Pocari Sweat Powder 5x74g").as_deref(),
            Some("5x74g")
        );
        assert_eq!(
            pack_size("S&R Red Onion approx. 1.2kg").as_deref(),
            Some("1.2kg")
        );
        assert_eq!(
            pack_size("Carnival Double Wall Rippled Red Cup 50pcs").as_deref(),
            Some("50pcs")
        );
        assert_eq!(
            pack_size("Gatorode Mix 6x500mL").as_deref(),
            Some("6x500mL")
        );
        assert_eq!(
            pack_size("Red Bull Energy Drink 4 cans").as_deref(),
            Some("4 cans")
        );
        assert_eq!(pack_size("Monster Ultra Energy Assorted"), None);
        // Digits glued to letters (model numbers) are not sizes.
        assert_eq!(pack_size("SPF50 Sunscreen"), None);
    }

    #[test]
    fn parses_search_fixture() {
        let l = StoreList::parse_search(&fixture("mart_search.json"));
        assert!(!l.stores.is_empty());
        let w = &l.stores[0];
        assert_eq!(w.id, "2-C221R641CXWGEE");
        assert!(w.name.starts_with("Watsons"));
        assert_eq!(w.delivery_fee, Some(Money(6900)));
        assert_eq!(w.status.as_deref(), Some("OPENED"));
        assert!(!w.products.is_empty());
        let p = &w.products[0];
        assert!(p.id.starts_with("PHITE"));
        assert!(p.price.minor() > 0);
        assert_eq!(p.size.as_deref(), Some("180ml"));
    }

    #[test]
    fn parses_category_fixture() {
        let l = StoreList::parse_search(&fixture("mart_category.json"));
        assert_eq!(l.title.as_deref(), Some("Grocery Stores"));
        assert_eq!(l.total, Some(19));
        assert!(!l.stores.is_empty());
    }

    #[test]
    fn parses_homefeed_fixture() {
        let v = fixture("mart_homefeed.json");
        let l = StoreList::parse_homefeed(&v);
        assert!(!l.stores.is_empty());
        assert_eq!(l.next_offset, Some(12));
        let c = Categories::parse_homefeed(&v);
        assert!(c.store_categories.iter().any(|c| c.id == "19675"));
        assert!(!c.store_categories.iter().any(|c| c.id == "MART_ENTRY"));
        assert!(c.departments.iter().any(|d| d.name == "Pantry Supplies"));
    }

    #[test]
    fn parses_merchant_fixture() {
        let p = StorePage::parse(&fixture("mart_merchant.json")).expect("merchant");
        assert_eq!(p.id, "2-C221R641CXWGEE");
        assert!(p.departments.len() > 2);
        let disc = &p.departments[0];
        assert_eq!(disc.department_type, 1);
        assert!(!disc.items.is_empty());
        let it = disc
            .items
            .iter()
            .find(|i| i.id == "PHITE20260331165647313654")
            .expect("discounted item");
        assert_eq!(it.discounted, Some(Money(9950)));
        assert_eq!(it.effective_price(), Money(9950));
        assert!(it.campaign_id.is_some());
        assert!(!p.campaigns.is_empty());
        assert!(p.section.is_none());
        assert!(p.department("PHITEDP20260225062437014128").is_some());
    }

    #[test]
    fn parses_items_and_instore_fixtures() {
        let p = ProductPage::parse(&fixture("mart_items.json"));
        assert!(!p.items.is_empty());
        assert_eq!(p.next_offset, Some(20));
        assert!(!p.sub_departments.is_empty());
        let s = ProductPage::parse(&fixture("mart_instore.json"));
        assert_eq!(s.total, Some(276));
        assert_eq!(s.items[0].size.as_deref(), Some("50ML"));
    }

    #[test]
    fn parses_item_detail_fixture() {
        let d = ItemDetail::parse(&fixture("mart_item.json")).expect("item");
        assert_eq!(d.product.name, "Red Bull Energy Drink 4 x 250ml");
        assert_eq!(d.product.price, Money(33900));
        assert_eq!(d.product.discounted, None);
        assert_eq!(d.product.size.as_deref(), Some("4 x 250ml"));
        assert_eq!(d.product.in_stock, Some(true));
        assert_eq!(d.merchant_name.as_deref(), Some("S&R - Example Branch"));
        assert!(d.attributes.iter().any(|(t, _)| t == "Brand"));
    }

    #[test]
    fn availability_text() {
        let mut p =
            Product::parse(&json!({"ID": "I", "name": "X", "priceInMinorUnit": 100})).expect("p");
        assert_eq!(p.availability(), "available");
        p.in_stock = Some(false);
        assert_eq!(p.availability(), "out of stock");
        assert!(!p.orderable());
        p.available = false;
        assert_eq!(p.availability(), "unavailable");
    }
}
