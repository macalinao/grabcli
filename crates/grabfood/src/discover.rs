//! Discovery lists: parsing `/search`, `/category` and
//! `/recommended/merchants` responses into [`Merchant`]s (with any dish hits),
//! plus the client-side filters and sorting the upstream lacks (see
//! docs/discovery.md).

use std::collections::HashSet;

use grabcli_core::money::Money;
use serde_json::{Value, json};

/// A dish hit inside a search result (`dishes[]`), or a catalog item.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Dish {
    pub id: String,
    pub merchant_id: String,
    pub name: String,
    pub description: String,
    pub price: Option<Money>,
    pub discounted: Option<Money>,
    pub available: bool,
    pub image_url: Option<String>,
}

impl Dish {
    /// What it costs now: the discounted price if lower, else the list price.
    pub fn effective_price(&self) -> Option<Money> {
        match (self.discounted, self.price) {
            (Some(d), Some(p)) if d.minor() > 0 => Some(d.min(p)),
            (Some(d), None) => Some(d),
            (_, p) => p,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "merchant_id": self.merchant_id,
            "name": self.name,
            "description": self.description,
            "price_minor": self.price.map(Money::minor),
            "discounted_price_minor": self.discounted.map(Money::minor),
            "effective_price_minor": self.effective_price().map(Money::minor),
            "available": self.available,
            "image_url": self.image_url,
        })
    }
}

/// A restaurant/store in a discovery list.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Merchant {
    pub id: String,
    pub name: String,
    pub chain: Option<String>,
    pub branch: Option<String>,
    /// `FOOD` or `MART`.
    pub business_type: Option<String>,
    pub cuisines: Vec<String>,
    pub rating: Option<f64>,
    pub votes: Option<i64>,
    /// 1–3.
    pub price_level: Option<i64>,
    pub eta_min: Option<i64>,
    pub distance_km: Option<f64>,
    /// Delivery fee after any discount.
    pub fee: Option<Money>,
    /// Delivery fee before discount.
    pub fee_before: Option<Money>,
    pub free_delivery: bool,
    pub has_promo: bool,
    pub promo: Option<String>,
    pub promo_tags: Vec<String>,
    pub open: Option<bool>,
    pub status: Option<String>,
    pub hours: Option<String>,
    pub opening_hours: Option<Value>,
    pub photo_url: Option<String>,
    pub dishes: Vec<Dish>,
    /// Other branches of a chain (indexed, not listed).
    pub branches: Vec<Merchant>,
    /// When this data was fetched, for catalog (local) results.
    pub fetched_at: Option<i64>,
    pub raw: Option<Value>,
}

/// One page of a list response.
#[derive(Debug, Clone, Default)]
pub struct Page {
    pub merchants: Vec<Merchant>,
    pub has_more: bool,
    pub total: Option<i64>,
}

fn s(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn f(v: &Value, ptr: &str) -> Option<f64> {
    v.pointer(ptr).and_then(Value::as_f64)
}

fn i(v: &Value, ptr: &str) -> Option<i64> {
    v.pointer(ptr).and_then(|x| {
        x.as_i64()
            .or_else(|| x.as_f64().map(|f| f.round() as i64))
            .or_else(|| x.as_str().and_then(|s| s.trim().parse().ok()))
    })
}

fn b(v: &Value, ptr: &str) -> Option<bool> {
    v.pointer(ptr).and_then(Value::as_bool)
}

/// Drop HTML tags from Grab's `*Html` display strings.
pub fn strip_html(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first amount in a display string (`"₱1,104.50 "` → 110450).
pub fn parse_amount(s: &str) -> Option<Money> {
    let start = s.find(|c: char| c.is_ascii_digit())?;
    let num: String = s[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
        .filter(|c| *c != ',')
        .collect();
    parse_major(&num)
}

/// `"49"`, `"49.5"`, `"₱49.50"`, `"P49"` → minor units.
pub fn parse_major(s: &str) -> Option<Money> {
    let t = s
        .trim()
        .trim_start_matches('₱')
        .trim_start_matches(['P', 'p'])
        .replace(',', "");
    let t = t.trim();
    if t.is_empty() || t.starts_with('-') {
        return None;
    }
    let (whole, frac) = t.split_once('.').unwrap_or((t, ""));
    if frac.len() > 2 || !whole.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let whole: i64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let cents: i64 = match frac.len() {
        0 => 0,
        1 => frac.parse::<i64>().ok()? * 10,
        _ => frac.parse().ok()?,
    };
    Some(Money(whole.checked_mul(100)?.checked_add(cents)?))
}

/// `(fee now, fee before discount, free?)` from `estimatedDeliveryFee`.
fn delivery_fee(fee: Option<&Value>) -> (Option<Money>, Option<Money>, bool) {
    let Some(fee) = fee else {
        return (None, None, false);
    };
    let before = fee.get("price").and_then(Value::as_i64).map(Money);
    let shown = s(fee, "/discountedPriceDisplayHtml")
        .or_else(|| s(fee, "/priceDisplayHtml"))
        .map(|h| strip_html(&h))
        .or_else(|| s(fee, "/priceDisplay"))
        .unwrap_or_default();
    if shown.to_lowercase().starts_with("free") {
        return (Some(Money::ZERO), before, true);
    }
    let discounted = b(fee, "/hasDiscountedPrice") == Some(true);
    let now = if discounted {
        s(fee, "/discountedPriceDisplayHtml")
            .and_then(|h| parse_amount(&strip_html(&h)))
            .or(before)
    } else {
        before.or_else(|| parse_amount(&shown))
    };
    (now, before, now == Some(Money::ZERO))
}

fn parse_dish(v: &Value, merchant_id: &str) -> Option<Dish> {
    let id = s(v, "/ID")?;
    let price = i(v, "/priceInMinorUnit")
        .or_else(|| i(v, "/priceV2/amountInMinor"))
        .map(Money);
    let discounted = i(v, "/discountedPriceInMin")
        .or_else(|| i(v, "/discountedPriceV2/amountInMinor"))
        .map(Money)
        .filter(|d| price.is_none_or(|p| *d != p));
    Some(Dish {
        id,
        merchant_id: s(v, "/merchantID").unwrap_or_else(|| merchant_id.to_string()),
        name: s(v, "/name").unwrap_or_default(),
        description: s(v, "/description").unwrap_or_default(),
        price,
        discounted,
        available: b(v, "/available").unwrap_or(true),
        image_url: s(v, "/imgHref").or_else(|| s(v, "/images/0")),
    })
}

/// A merchant from `searchMerchants[]` (search and category responses).
pub fn merchant_from_search(v: &Value) -> Option<Merchant> {
    let id = s(v, "/id")?;
    let brief = v.get("merchantBrief").cloned().unwrap_or(Value::Null);
    let (fee, fee_before, free) = delivery_fee(v.get("estimatedDeliveryFee"));
    let promo_tags: Vec<String> = v
        .pointer("/sideLabels/data")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|l| s(l, "/displayedText"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let has_promo = b(&brief, "/promo/hasPromo").unwrap_or(false) || !promo_tags.is_empty();
    let dishes = v
        .get("dishes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|d| parse_dish(d, &id)).collect())
        .unwrap_or_default();
    let branches = v
        .get("branchMerchants")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(merchant_from_search).collect())
        .unwrap_or_default();
    Some(Merchant {
        name: s(v, "/address/name")
            .or_else(|| s(&brief, "/displayInfo/primaryText"))
            .unwrap_or_else(|| id.clone()),
        chain: s(v, "/chainName"),
        branch: s(v, "/branchName"),
        business_type: s(v, "/businessType"),
        cuisines: brief
            .get("cuisine")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        rating: f(&brief, "/rating").filter(|r| *r > 0.0),
        votes: i(&brief, "/vote_count"),
        price_level: i(&brief, "/priceTag").filter(|p| *p > 0),
        eta_min: i(v, "/estimatedDeliveryTime"),
        distance_km: f(&brief, "/distanceInKm"),
        fee,
        fee_before,
        free_delivery: free,
        has_promo,
        promo: s(&brief, "/promo/description"),
        promo_tags,
        open: b(&brief, "/openHours/open"),
        status: s(v, "/merchantStatusInfo/status"),
        hours: s(&brief, "/openHours/displayedHours"),
        opening_hours: brief.get("openHours").cloned(),
        photo_url: s(&brief, "/photoHref"),
        dishes,
        branches,
        fetched_at: None,
        raw: Some(v.clone()),
        id,
    })
}

/// A merchant from `recommendedMerchants[]`.
pub fn merchant_from_recommended(v: &Value) -> Option<Merchant> {
    let id = s(v, "/id")?;
    let d = v.get("merchantData").cloned().unwrap_or(Value::Null);
    Some(Merchant {
        name: s(v, "/address/name").unwrap_or_else(|| id.clone()),
        rating: f(&d, "/rating").filter(|r| *r > 0.0),
        votes: i(&d, "/vote_count"),
        price_level: i(&d, "/priceTag").filter(|p| *p > 0),
        eta_min: i(&d, "/estimated_delivery_time"),
        distance_km: f(v, "/distanceInKm"),
        has_promo: b(&d, "/promo/hasPromo").unwrap_or(false),
        promo: s(&d, "/promo/description"),
        open: b(&d, "/service_hours/open"),
        hours: s(&d, "/service_hours/displayedHours"),
        opening_hours: d.get("service_hours").cloned(),
        photo_url: s(&d, "/photo_href"),
        raw: Some(v.clone()),
        id,
        ..Merchant::default()
    })
}

/// `searchResult` of a `/search` or `/category` response.
pub fn parse_search_page(root: &Value) -> Page {
    let sr = root.get("searchResult").unwrap_or(&Value::Null);
    Page {
        merchants: sr
            .get("searchMerchants")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(merchant_from_search).collect())
            .unwrap_or_default(),
        has_more: b(sr, "/hasMore").unwrap_or(false),
        total: i(sr, "/totalCount"),
    }
}

/// Merchants per `/recommended/merchants` page.
pub const RECOMMENDED_PAGE: usize = 8;

/// A `/recommended/merchants` response.
pub fn parse_recommended(root: &Value) -> Page {
    let merchants: Vec<Merchant> = root
        .get("recommendedMerchantGroups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|g| g.get("recommendedMerchants").and_then(Value::as_array))
        .flatten()
        .filter_map(merchant_from_recommended)
        .collect();
    Page {
        has_more: merchants.len() >= RECOMMENDED_PAGE,
        merchants,
        total: None,
    }
}

/// Append `page` to `all`, skipping merchant ids already present (pages
/// drift and can repeat merchants).
pub fn merge_dedupe(all: &mut Vec<Merchant>, seen: &mut HashSet<String>, page: Vec<Merchant>) {
    for m in page {
        if seen.insert(m.id.clone()) {
            all.push(m);
        }
    }
}

fn fold(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ä' | 'ã' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ñ' => 'n',
            c => c,
        })
        .collect()
}

impl Dish {
    /// Does every word of `keyword` appear in the name or description?
    pub fn matches_keyword(&self, keyword: &str) -> bool {
        let hay = fold(&format!("{} {}", self.name, self.description));
        let mut words = keyword
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .peekable();
        words.peek().is_some() && words.all(|w| hay.contains(&fold(w)))
    }
}

/// Grab's dish hits include loosely related items (sides, add-ons). Put the
/// dishes that actually mention the keyword first; with `strict`, drop the
/// rest unless nothing matches.
pub fn rank_dishes(ms: &mut [Merchant], keyword: &str, strict: bool) {
    for m in ms {
        let (hit, rest): (Vec<Dish>, Vec<Dish>) = std::mem::take(&mut m.dishes)
            .into_iter()
            .partition(|d| d.matches_keyword(keyword));
        m.dishes = if strict && !hit.is_empty() {
            hit
        } else {
            hit.into_iter().chain(rest).collect()
        };
    }
}

/// `--sort` keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sort {
    /// Upstream order.
    #[default]
    Relevance,
    Rating,
    Eta,
    Fee,
    Distance,
    Price,
}

impl std::str::FromStr for Sort {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "relevance" => Self::Relevance,
            "rating" => Self::Rating,
            "eta" | "time" => Self::Eta,
            "fee" => Self::Fee,
            "distance" => Self::Distance,
            "price" => Self::Price,
            other => {
                return Err(format!(
                    "unknown sort `{other}` (relevance|rating|eta|fee|distance|price)"
                ));
            }
        })
    }
}

/// `--type food|mart`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BizType {
    Food,
    Mart,
}

impl BizType {
    fn matches(self, t: Option<&str>) -> bool {
        let want = match self {
            Self::Food => "FOOD",
            Self::Mart => "MART",
        };
        t.is_some_and(|t| t.eq_ignore_ascii_case(want))
    }
}

impl std::str::FromStr for BizType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "food" => Ok(Self::Food),
            "mart" => Ok(Self::Mart),
            other => Err(format!("unknown type `{other}` (food|mart)")),
        }
    }
}

/// A peso amount argument (`49`, `49.50`, `₱49`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pesos(pub Money);

impl std::str::FromStr for Pesos {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_major(s)
            .map(Pesos)
            .ok_or_else(|| format!("not a peso amount: `{s}` (e.g. 49 or 49.50)"))
    }
}

/// Filters and sorting (applied client-side to the fetched pages):
/// With the `cli` feature this also derives the bpaf parser `list_opts()`.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "cli", derive(bpaf::Bpaf))]
pub struct ListOpts {
    /// Only merchants that are open now.
    #[cfg_attr(feature = "cli", bpaf(long("open"), switch))]
    pub open: bool,
    /// Minimum rating (e.g. 4.5).
    #[cfg_attr(feature = "cli", bpaf(long("min-rating"), argument("X")))]
    pub min_rating: Option<f64>,
    /// Maximum delivery fee in pesos, after discounts.
    #[cfg_attr(feature = "cli", bpaf(long("max-fee"), argument("PESOS")))]
    pub max_fee: Option<Pesos>,
    /// Maximum delivery ETA in minutes.
    #[cfg_attr(feature = "cli", bpaf(long("max-eta"), argument("MIN")))]
    pub max_eta: Option<i64>,
    /// Maximum price level, 1 (cheap) to 3.
    #[cfg_attr(feature = "cli", bpaf(long("price-level"), argument("N")))]
    pub price_level: Option<i64>,
    /// Only merchants with a promo.
    #[cfg_attr(feature = "cli", bpaf(long("promo"), switch))]
    pub promo: bool,
    /// Only merchants with free delivery.
    #[cfg_attr(feature = "cli", bpaf(long("free-delivery"), switch))]
    pub free_delivery: bool,
    /// Only merchants with this cuisine (case-insensitive substring).
    #[cfg_attr(feature = "cli", bpaf(long("cuisine"), argument("NAME")))]
    pub cuisine: Option<String>,
    /// Maximum distance in km.
    #[cfg_attr(feature = "cli", bpaf(long("max-distance"), argument("KM")))]
    pub max_distance: Option<f64>,
    /// food or mart (Grab mixes mart stores into food results).
    #[cfg_attr(feature = "cli", bpaf(long("type"), argument("TYPE")))]
    pub business_type: Option<BizType>,
    /// relevance (default), rating, eta, fee, distance or price.
    #[cfg_attr(
        feature = "cli",
        bpaf(long("sort"), argument("KEY"), fallback(Sort::Relevance))
    )]
    pub sort: Sort,
    /// Show at most N results.
    #[cfg_attr(feature = "cli", bpaf(long("limit"), argument("N")))]
    pub limit: Option<usize>,
    /// Upstream pages to pull (paced to respect Grab's rate limit).
    #[cfg_attr(feature = "cli", bpaf(long("pages"), argument("N"), fallback(1)))]
    pub pages: u32,
}

fn cmp_opt_f64(a: Option<f64>, b: Option<f64>) -> std::cmp::Ordering {
    // Unknowns sort last.
    a.unwrap_or(f64::MAX).total_cmp(&b.unwrap_or(f64::MAX))
}

impl ListOpts {
    /// Does `m` pass every merchant filter? Unknown values fail a filter
    /// that needs them.
    pub fn matches(&self, m: &Merchant) -> bool {
        (!self.open || m.open == Some(true))
            && self
                .min_rating
                .is_none_or(|r| m.rating.is_some_and(|x| x >= r))
            && self
                .max_fee
                .is_none_or(|Pesos(max)| m.fee.is_some_and(|f| f <= max))
            && self
                .max_eta
                .is_none_or(|e| m.eta_min.is_some_and(|x| x <= e))
            && self
                .price_level
                .is_none_or(|p| m.price_level.is_some_and(|x| x <= p))
            && (!self.promo || m.has_promo)
            && (!self.free_delivery || m.free_delivery)
            && self.cuisine.as_deref().is_none_or(|c| {
                let c = c.to_lowercase();
                m.cuisines.iter().any(|x| x.to_lowercase().contains(&c))
            })
            && self
                .max_distance
                .is_none_or(|d| m.distance_km.is_some_and(|x| x <= d))
            && self
                .business_type
                .is_none_or(|t| t.matches(m.business_type.as_deref()))
    }

    /// Sort merchants in place (stable, so ties keep upstream order).
    pub fn sort_merchants(&self, ms: &mut [Merchant]) {
        match self.sort {
            Sort::Relevance => {}
            Sort::Rating => ms.sort_by(|a, b| {
                cmp_opt_f64(a.rating.map(|r| -r), b.rating.map(|r| -r))
                    .then(b.votes.unwrap_or(0).cmp(&a.votes.unwrap_or(0)))
            }),
            Sort::Eta => ms.sort_by_key(|m| m.eta_min.unwrap_or(i64::MAX)),
            Sort::Fee => ms.sort_by_key(|m| m.fee.map_or(i64::MAX, Money::minor)),
            Sort::Distance => ms.sort_by(|a, b| cmp_opt_f64(a.distance_km, b.distance_km)),
            Sort::Price => ms.sort_by_key(|m| m.price_level.unwrap_or(i64::MAX)),
        }
    }

    /// Filter, sort and limit merchants.
    pub fn apply(&self, ms: Vec<Merchant>) -> Vec<Merchant> {
        let mut out: Vec<Merchant> = ms.into_iter().filter(|m| self.matches(m)).collect();
        self.sort_merchants(&mut out);
        if let Some(l) = self.limit {
            out.truncate(l);
        }
        out
    }

    /// Dishes across the (filtered) merchants, cheapest first for
    /// `--sort price`, else in merchant order. `max_price` filters on the
    /// effective price.
    pub fn dishes<'a>(
        &self,
        ms: &'a [Merchant],
        max_price: Option<Money>,
    ) -> Vec<(&'a Dish, &'a Merchant)> {
        let mut merchants: Vec<&Merchant> = ms.iter().filter(|m| self.matches(m)).collect();
        if self.sort != Sort::Price {
            let mut owned: Vec<Merchant> = merchants.iter().map(|m| (*m).clone()).collect();
            self.sort_merchants(&mut owned);
            let order: Vec<&str> = owned.iter().map(|m| m.id.as_str()).collect();
            merchants.sort_by_key(|m| order.iter().position(|id| *id == m.id));
        }
        let mut out: Vec<(&Dish, &Merchant)> = merchants
            .into_iter()
            .flat_map(|m| m.dishes.iter().map(move |d| (d, m)))
            .filter(|(d, _)| {
                max_price.is_none_or(|max| d.effective_price().is_some_and(|p| p <= max))
            })
            .collect();
        if self.sort == Sort::Price {
            out.sort_by_key(|(d, _)| d.effective_price().map_or(i64::MAX, Money::minor));
        }
        if let Some(l) = self.limit {
            out.truncate(l);
        }
        out
    }
}

impl Merchant {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "chain": self.chain,
            "business_type": self.business_type,
            "cuisines": self.cuisines,
            "rating": self.rating,
            "vote_count": self.votes,
            "price_level": self.price_level,
            "eta_min": self.eta_min,
            "distance_km": self.distance_km,
            "delivery_fee_minor": self.fee.map(Money::minor),
            "delivery_fee_before_minor": self.fee_before.map(Money::minor),
            "free_delivery": self.free_delivery,
            "has_promo": self.has_promo,
            "promo": self.promo,
            "promo_tags": self.promo_tags,
            "open": self.open,
            "status": self.status.clone().or_else(|| self.open.map(|o| {
                if o { "OPEN".to_string() } else { "CLOSED".to_string() }
            })),
            "hours": self.hours,
            "photo_url": self.photo_url,
            "dishes": self.dishes.iter().map(Dish::to_json).collect::<Vec<_>>(),
            "fetched_at": self.fetched_at,
        })
    }

    /// One human line: id, name, rating, ETA, distance, fee, price level,
    /// open state, promo.
    pub fn human_line(&self) -> String {
        let mut parts = vec![format!("{}  {}", self.id, self.name)];
        if let Some(r) = self.rating {
            parts.push(match self.votes {
                Some(v) => format!("★{r:.1} ({v})"),
                None => format!("★{r:.1}"),
            });
        }
        if let Some(e) = self.eta_min {
            parts.push(format!("{e} min"));
        }
        if let Some(d) = self.distance_km {
            parts.push(format!("{d:.1} km"));
        }
        if self.free_delivery {
            parts.push("free delivery".into());
        } else if let Some(f) = self.fee {
            parts.push(format!("fee {}", f.peso()));
        }
        if let Some(p) = self.price_level {
            parts.push("₱".repeat(usize::try_from(p.clamp(1, 4)).unwrap_or(1)));
        }
        match self.open {
            Some(true) => parts.push("OPEN".into()),
            Some(false) => parts.push("CLOSED".into()),
            None => {}
        }
        if self.business_type.as_deref() == Some("MART") {
            parts.push("[mart]".into());
        }
        if let Some(t) = self.promo_tags.first().or(self.promo.as_ref()) {
            parts.push(format!("promo: {t}"));
        } else if self.has_promo {
            parts.push("promo".into());
        }
        parts.join("  ")
    }
}

impl Dish {
    pub fn human_line(&self) -> String {
        let price = match (self.discounted, self.price) {
            (Some(d), Some(p)) if d < p => format!("{} (was {})", d.peso(), p.peso()),
            _ => self
                .effective_price()
                .map(Money::peso)
                .unwrap_or_else(|| "?".into()),
        };
        let avail = if self.available {
            ""
        } else {
            "  (unavailable)"
        };
        format!("{}  {}  {price}{avail}", self.id, self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let s = match name {
            "search" => include_str!("../../../docs/fixtures/discovery_search.json"),
            "dishes" => include_str!("../../../docs/fixtures/discovery_search_dishes.json"),
            "category" => include_str!("../../../docs/fixtures/discovery_category.json"),
            "recommended" => include_str!("../../../docs/fixtures/discovery_recommended.json"),
            _ => "null",
        };
        serde_json::from_str(s).unwrap_or(Value::Null)
    }

    #[test]
    fn parses_search_merchants_fees_and_dishes() {
        let page = parse_search_page(&fixture("search"));
        assert!(page.has_more);
        assert_eq!(page.total, Some(260));
        let mcd = &page.merchants[0];
        assert_eq!(mcd.id, "AWkxrGp0cEjWIUmPtEPk");
        assert_eq!(mcd.cuisines, vec!["Western", "Burger"]);
        assert_eq!(mcd.rating, Some(4.1));
        assert_eq!(mcd.price_level, Some(1));
        assert_eq!(mcd.open, Some(true));
        // ₱42 struck through, shown as Free.
        assert!(mcd.free_delivery);
        assert_eq!(mcd.fee, Some(Money::ZERO));
        assert_eq!(mcd.fee_before, Some(Money(4200)));
        assert!(mcd.has_promo);
        assert_eq!(mcd.promo_tags[0], "5% off");

        let dishes = parse_search_page(&fixture("dishes"));
        let g = &dishes.merchants[0];
        assert!(g.free_delivery);
        assert_eq!(g.fee_before, None);
        assert_eq!(g.dishes[0].name, "Tofu Sisig");
        assert_eq!(g.dishes[0].price, Some(Money(33000)));
        assert_eq!(g.dishes[0].discounted, None); // same as list price
        assert_eq!(g.dishes[0].merchant_id, "2-C4JUG6XVGVMGWE");
        assert!(g.dishes[0].image_url.is_some());

        let cat = parse_search_page(&fixture("category"));
        assert!(!cat.merchants.is_empty());
        assert_eq!(cat.total, Some(116));
    }

    #[test]
    fn discounted_fee_amounts() {
        let fee = json!({"price": 6900, "priceDisplay": "₱69.00", "hasDiscountedPrice": true,
            "discountedPriceDisplayHtml": "<font color='#F88530'>₱20.00 </font>"});
        assert_eq!(
            delivery_fee(Some(&fee)),
            (Some(Money(2000)), Some(Money(6900)), false)
        );
        let plain = json!({"price": 4900, "priceDisplay": "₱49.00"});
        assert_eq!(
            delivery_fee(Some(&plain)),
            (Some(Money(4900)), Some(Money(4900)), false)
        );
        assert_eq!(parse_amount("₱1,104.50 "), Some(Money(110_450)));
        assert_eq!(parse_major("₱49"), Some(Money(4900)));
        assert_eq!(parse_major("49.5"), Some(Money(4950)));
        assert_eq!(parse_major("-1"), None);
        assert_eq!(parse_major("1.234"), None);
    }

    #[test]
    fn parses_recommended() {
        let page = parse_recommended(&fixture("recommended"));
        assert_eq!(page.merchants.len(), 3);
        assert!(!page.has_more);
        let m = &page.merchants[0];
        assert_eq!(m.name, "McDonald's - Example Branch");
        assert_eq!(m.eta_min, Some(45));
        assert_eq!(m.distance_km, Some(4.5));
        assert_eq!(m.votes, Some(14830));
    }

    fn m(id: &str, rating: f64, eta: i64, fee: i64, km: f64) -> Merchant {
        Merchant {
            id: id.into(),
            name: id.into(),
            rating: Some(rating),
            eta_min: Some(eta),
            fee: Some(Money(fee)),
            distance_km: Some(km),
            open: Some(true),
            cuisines: vec!["Filipino".into()],
            business_type: Some("FOOD".into()),
            ..Merchant::default()
        }
    }

    #[test]
    fn filters_and_sorts() {
        let ms = vec![
            m("A", 4.2, 30, 4900, 1.0),
            m("B", 4.8, 45, 0, 2.5),
            Merchant {
                open: Some(false),
                ..m("C", 4.9, 20, 2000, 0.5)
            },
            Merchant {
                business_type: Some("MART".into()),
                cuisines: vec!["Pharmacy".into()],
                ..m("D", 4.0, 15, 0, 0.3)
            },
        ];
        let opts = |f: &dyn Fn(&mut ListOpts)| {
            let mut o = ListOpts::default();
            f(&mut o);
            o
        };
        let ids = |v: Vec<Merchant>| v.into_iter().map(|m| m.id).collect::<Vec<_>>();
        assert_eq!(
            ids(opts(&|o| o.open = true).apply(ms.clone())),
            ["A", "B", "D"]
        );
        assert_eq!(
            ids(opts(&|o| o.min_rating = Some(4.5)).apply(ms.clone())),
            ["B", "C"]
        );
        assert_eq!(
            ids(opts(&|o| o.max_fee = Some(Pesos(Money(2000)))).apply(ms.clone())),
            ["B", "C", "D"]
        );
        assert_eq!(
            ids(opts(&|o| o.cuisine = Some("filip".into())).apply(ms.clone())),
            ["A", "B", "C"]
        );
        assert_eq!(
            ids(opts(&|o| o.business_type = Some(BizType::Food)).apply(ms.clone())),
            ["A", "B", "C"]
        );
        assert_eq!(
            ids(opts(&|o| o.max_distance = Some(1.0)).apply(ms.clone())),
            ["A", "C", "D"]
        );
        assert_eq!(
            ids(opts(&|o| o.sort = Sort::Rating).apply(ms.clone())),
            ["C", "B", "A", "D"]
        );
        assert_eq!(
            ids(opts(&|o| o.sort = Sort::Eta).apply(ms.clone())),
            ["D", "C", "A", "B"]
        );
        assert_eq!(
            ids(opts(&|o| {
                o.sort = Sort::Distance;
                o.limit = Some(2);
            })
            .apply(ms.clone())),
            ["D", "C"]
        );
        // Unknown values fail filters that need them.
        let unknown = Merchant {
            id: "U".into(),
            ..Merchant::default()
        };
        assert!(!opts(&|o| o.min_rating = Some(1.0)).matches(&unknown));
        assert!(ListOpts::default().matches(&unknown));
    }

    #[test]
    fn dish_keyword_ranking() {
        let d = |name: &str, desc: &str| Dish {
            id: name.into(),
            name: name.into(),
            description: desc.into(),
            ..Dish::default()
        };
        let mut ms = vec![Merchant {
            dishes: vec![
                d("Rice", ""),
                d("Pork Sísig", ""),
                d("Combo", "with sisig and rice"),
            ],
            ..Merchant::default()
        }];
        rank_dishes(&mut ms, "sisig", false);
        let names: Vec<&str> = ms[0].dishes.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Pork Sísig", "Combo", "Rice"]);
        rank_dishes(&mut ms, "sisig", true);
        assert_eq!(ms[0].dishes.len(), 2);
        // Nothing matches: strict keeps everything.
        rank_dishes(&mut ms, "ramen", true);
        assert_eq!(ms[0].dishes.len(), 2);
        assert!(!d("x", "").matches_keyword(""));
    }

    #[test]
    fn dish_listing_and_dedupe() {
        let page = parse_search_page(&fixture("dishes"));
        let mut all = Vec::new();
        let mut seen = HashSet::new();
        merge_dedupe(&mut all, &mut seen, page.merchants.clone());
        merge_dedupe(&mut all, &mut seen, page.merchants.clone());
        assert_eq!(all.len(), page.merchants.len());

        let opts = ListOpts {
            sort: Sort::Price,
            ..ListOpts::default()
        };
        let dishes = opts.dishes(&all, Some(Money(30_000)));
        assert!(!dishes.is_empty());
        let prices: Vec<i64> = dishes
            .iter()
            .filter_map(|(d, _)| d.effective_price().map(Money::minor))
            .collect();
        assert!(prices.windows(2).all(|w| w[0] <= w[1]));
        assert!(prices.iter().all(|p| *p <= 30_000));
    }
}
