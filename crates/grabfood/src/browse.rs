//! Discovery: `search`, `nearby`, `recommended`, `cuisines` and `category`,
//! each returning a typed [`Listing`] (printing is the CLI's). Lists are fetched page by page through the response cache
//! (paced, stopping cleanly on a 429), indexed into the local catalog, then
//! filtered/sorted client-side. If the live call fails, results fall back to
//! the last stored response or the local catalog, marked with their age.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use grabcli_core::Context;
use grabcli_core::cache::{
    CacheKey, CacheKind, Fetched, ItemQuery, Near, RestaurantQuery, human_age,
};
use grabcli_core::error::{Error, ErrorKind, Result};
use grabcli_core::http::{Hydra, Request};
use grabcli_core::jwt;
use grabcli_core::location::Location;
use grabcli_core::money::Money;
use serde_json::{Value, json};

use crate::discover::{self, ListOpts, Merchant, Page};
use crate::{CACHE_NS, api, index};

/// Search/category page size the web app sends (the server mostly ignores
/// it and returns ~30).
const PAGE_SIZE: usize = 32;

/// Minimum gap between live page requests: ~10-12 quick calls trigger
/// `429 FW_RATE_EXCEEDED`.
const PAGE_GAP: Duration = Duration::from_secs(3);

/// Default radius for local (catalog) results; Grab's own is ~6 km.
const LOCAL_RADIUS_KM: f64 = 6.0;

/// Max pages per command, whatever `--pages` says.
const MAX_PAGES: u32 = 10;

/// GrabFood PH cuisine shortcuts linked from the web (docs/discovery.md).
pub const CUISINES: &[(&str, &str)] = &[
    ("45", "Chinese"),
    ("62", "Fast Food"),
    ("63", "Filipino"),
    ("89", "Japanese"),
    ("96", "Korean"),
    ("110", "Mexican"),
    ("151", "Seafood"),
    ("251", "Promotions"),
];

/// What a list command pages through.
#[derive(Debug, Clone)]
pub enum ListSource {
    /// `/search` (empty keyword = nearby).
    Search(String),
    /// `/category?categoryShortcutID=` with its display name.
    Category { id: String, name: String },
    /// `/recommended/merchants`.
    Recommended,
}

impl ListSource {
    fn page_step(&self) -> usize {
        match self {
            Self::Recommended => discover::RECOMMENDED_PAGE,
            _ => PAGE_SIZE,
        }
    }

    fn key(&self, loc: &Location, offset: usize) -> CacheKey {
        match self {
            Self::Search(kw) => CacheKey::new(CACHE_NS, CacheKind::List, "search")
                .param("keyword", kw.trim().to_lowercase())
                .param("offset", offset)
                .param("page_size", PAGE_SIZE)
                .param("country", "PH"),
            Self::Category { id, .. } => CacheKey::new(CACHE_NS, CacheKind::List, "category")
                .param("id", id)
                .param("offset", offset)
                .param("page_size", PAGE_SIZE),
            Self::Recommended => {
                CacheKey::new(CACHE_NS, CacheKind::List, "recommended").param("offset", offset)
            }
        }
        .location(loc.latitude, loc.longitude)
    }

    fn request(&self, loc: &Location, offset: usize) -> Request {
        match self {
            Self::Search(kw) => Request::post(
                api::SEARCH,
                json!({
                    "latlng": loc.latlng(),
                    "keyword": kw.trim(),
                    "offset": offset,
                    "pageSize": PAGE_SIZE,
                    "countryCode": "PH",
                }),
                Hydra::NotNeeded,
            ),
            Self::Category { id, .. } => Request::get(
                api::category_url(&loc.latlng(), id, offset, PAGE_SIZE),
                Hydra::NotNeeded,
            ),
            Self::Recommended => Request::get(
                api::recommended_url(loc.latitude, loc.longitude, offset),
                Hydra::NotNeeded,
            ),
        }
    }

    fn parse(&self, raw: &Value) -> Page {
        match self {
            Self::Recommended => discover::parse_recommended(raw),
            _ => discover::parse_search_page(raw),
        }
    }

    /// Free text for the local catalog query.
    fn local_text(&self) -> Option<String> {
        match self {
            Self::Search(kw) => Some(kw.trim().to_string()).filter(|k| !k.is_empty()),
            Self::Category { name, .. } => Some(name.clone()),
            Self::Recommended => None,
        }
    }
}

/// Where a listing came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Every page fetched live.
    Api,
    /// Every page served from the response cache.
    Cache,
    /// Some pages cached, some live.
    Mixed,
    /// The last stored (expired) response, after a failed live call.
    Stale,
    /// The local catalog (`--local`, or after a failed live call).
    Local,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Cache => "cache",
            Self::Mixed => "mixed",
            Self::Stale => "stale",
            Self::Local => "local",
        }
    }
}

/// A fetched (or local) list of merchants.
#[derive(Debug, Clone)]
pub struct Listing {
    pub merchants: Vec<Merchant>,
    pub total: Option<i64>,
    pub has_more: bool,
    pub pages: u32,
    pub source: Source,
    /// The oldest data's fetch time, if any came from the cache/catalog.
    pub cached_at: Option<i64>,
    /// Why paging stopped early (e.g. a 429 on page 3).
    pub stopped: Option<String>,
    /// The live error that triggered a stale/local fallback.
    pub fallback_reason: Option<String>,
}

/// Paces live requests.
struct Pacer {
    last: Option<Instant>,
}

impl Pacer {
    async fn wait(&mut self) {
        if let Some(t) = self.last {
            let since = t.elapsed();
            if since < PAGE_GAP {
                tokio::time::sleep(PAGE_GAP - since).await;
            }
        }
    }
}

fn min_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    }
}

/// Fetch up to `pages` pages of `src`, cached and indexed, deduping merchants.
/// An error on the first page is returned; a later error stops paging and is
/// reported in [`Listing::stopped`].
pub async fn fetch_listing(
    ctx: &mut Context,
    loc: &Location,
    src: &ListSource,
    pages: u32,
) -> Result<Listing> {
    let pages = pages.clamp(1, MAX_PAGES);
    let mut pacer = Pacer { last: None };
    let mut all = Vec::new();
    let mut seen = HashSet::new();
    let mut listing = Listing {
        merchants: Vec::new(),
        total: None,
        has_more: false,
        pages: 0,
        source: Source::Api,
        cached_at: None,
        stopped: None,
        fallback_reason: None,
    };
    let (mut any_cached, mut any_live) = (false, false);
    for page in 0..pages {
        let offset = page as usize * src.page_step();
        let key = src.key(loc, offset);
        let cache = ctx.cache().clone();
        let fetched: Fetched<Value> = match cache.lookup(&key).await {
            Some(hit) => hit,
            None => {
                pacer.wait().await;
                let res = ctx.call(&src.request(loc, offset)).await;
                pacer.last = Some(Instant::now());
                match res {
                    Ok(v) => {
                        let _ = cache.put(&key, &v).await;
                        index::index_merchants(
                            &cache,
                            &src.parse(&v).merchants,
                            (loc.latitude, loc.longitude),
                        )
                        .await;
                        Fetched::live(v)
                    }
                    Err(e) if page > 0 => {
                        listing.stopped = Some(format!("{}: {}", e.kind.as_str(), e.message));
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
        };
        if fetched.is_cached() {
            any_cached = true;
        } else {
            any_live = true;
        }
        listing.cached_at = min_opt(listing.cached_at, fetched.cached_at);
        let parsed = src.parse(&fetched.value);
        let n = parsed.merchants.len();
        listing.total = parsed.total.or(listing.total);
        listing.has_more = parsed.has_more;
        listing.pages += 1;
        discover::merge_dedupe(&mut all, &mut seen, parsed.merchants);
        if !parsed.has_more || n == 0 {
            break;
        }
    }
    listing.source = match (any_cached, any_live) {
        (true, false) => Source::Cache,
        (true, true) => Source::Mixed,
        _ => Source::Api,
    };
    listing.merchants = all;
    Ok(listing)
}

fn near(loc: &Location, opts: &ListOpts) -> Near {
    Near {
        lat: loc.latitude,
        lon: loc.longitude,
        radius_km: opts.max_distance.unwrap_or(LOCAL_RADIUS_KM),
    }
}

/// Results from the local catalog: restaurants matching the text near the
/// saved address, plus (for searches) restaurants with matching dishes, with
/// those dishes attached.
pub async fn local_listing(
    ctx: &Context,
    loc: &Location,
    src: &ListSource,
    opts: &ListOpts,
    max_price: Option<Money>,
    with_dishes: bool,
) -> Result<Listing> {
    let cache = ctx.cache();
    let near = near(loc, opts);
    let text = src.local_text();
    let hits = cache.search_restaurants(&RestaurantQuery {
        text: text.clone(),
        near: Some(near),
        namespace: Some(index::NAMESPACE.into()),
        limit: None,
    })
    .await?;
    let mut merchants: Vec<Merchant> = hits.iter().map(index::merchant_from_hit).collect();
    if with_dishes && let Some(t) = text {
        let items = cache.search_items(&ItemQuery {
            text: Some(t),
            near: Some(near),
            max_price_minor: max_price.map(Money::minor),
            available_only: false,
            restaurant_id: None,
            limit: Some(500),
        })
        .await?;
        for it in &items {
            let dish = index::dish_from_hit(it);
            if let Some(m) = merchants.iter_mut().find(|m| m.id == dish.merchant_id) {
                m.dishes.push(dish);
            } else if let Some(r) = &it.restaurant {
                let mut m = index::merchant_from_hit(r);
                m.dishes.push(dish);
                merchants.push(m);
            }
        }
    }
    let cached_at = merchants.iter().filter_map(|m| m.fetched_at).min();
    Ok(Listing {
        total: Some(i64::try_from(merchants.len()).unwrap_or(0)),
        merchants,
        has_more: false,
        pages: 0,
        source: Source::Local,
        cached_at,
        stopped: None,
        fallback_reason: None,
    })
}

/// After a failed live fetch: the last stored first page (even if expired),
/// else the local catalog. `None` if neither has anything (or the cache is
/// off).
async fn fallback(
    ctx: &Context,
    loc: &Location,
    src: &ListSource,
    opts: &ListOpts,
    max_price: Option<Money>,
    with_dishes: bool,
    err: &Error,
) -> Option<Listing> {
    if matches!(err.kind, ErrorKind::InvalidInput | ErrorKind::UrlNotAllowed)
        || !ctx.cache().mode().writes()
    {
        return None;
    }
    let reason = Some(format!("{}: {}", err.kind.as_str(), err.message));
    if let Some(stale) = ctx.cache().lookup_stale::<Value>(&src.key(loc, 0)).await {
        let page = src.parse(&stale.value);
        if !page.merchants.is_empty() {
            return Some(Listing {
                merchants: page.merchants,
                total: page.total,
                has_more: page.has_more,
                pages: 1,
                source: Source::Stale,
                cached_at: stale.cached_at,
                stopped: None,
                fallback_reason: reason,
            });
        }
    }
    let mut local = local_listing(ctx, loc, src, opts, max_price, with_dishes)
        .await
        .ok()?;
    if local.merchants.is_empty() {
        return None;
    }
    local.fallback_reason = reason;
    Some(local)
}

/// Fetch live (via cache), falling back to stale/local results on failure.
async fn get_listing(
    ctx: &mut Context,
    loc: &Location,
    src: &ListSource,
    opts: &ListOpts,
    max_price: Option<Money>,
    with_dishes: bool,
) -> Result<Listing> {
    match fetch_listing(ctx, loc, src, opts.pages).await {
        Ok(l) => Ok(l),
        Err(e) => fallback(ctx, loc, src, opts, max_price, with_dishes, &e)
            .await
            .ok_or(e),
    }
}

/// The listing metadata the CLI's `--json` output carries.
pub fn meta_json(l: &Listing, shown: usize) -> Value {
    let now = jwt::now();
    json!({
        "count": shown,
        "fetched": l.merchants.len(),
        "total": l.total,
        "has_more": l.has_more,
        "pages": l.pages,
        "source": l.source.as_str(),
        "cached_at": l.cached_at,
        "age_secs": l.cached_at.map(|t| (now - t).max(0)),
        "stopped": l.stopped,
        "fallback_reason": l.fallback_reason,
    })
}

/// The one-line human summary under a listing.
pub fn footer(l: &Listing, shown: usize, noun: &str) -> String {
    let mut parts = vec![match l.total {
        Some(t) if l.source != Source::Local => {
            format!(
                "{shown} {noun} shown ({} fetched of {t})",
                l.merchants.len()
            )
        }
        _ => format!("{shown} {noun} shown"),
    }];
    if l.pages > 1 {
        parts.push(format!("{} pages", l.pages));
    }
    if let Some(t) = l.cached_at {
        let age = human_age(jwt::now() - t);
        parts.push(match l.source {
            Source::Local => format!("local catalog, data up to {age} old"),
            Source::Stale => format!("stale cache from {age} ago"),
            _ => format!("cached {age} ago"),
        });
    }
    if l.has_more && l.source != Source::Local {
        parts.push("more available (--pages N)".into());
    }
    if let Some(s) = &l.stopped {
        parts.push(format!("stopped early ({s})"));
    }
    if let Some(r) = &l.fallback_reason {
        parts.push(format!("live fetch failed ({r})"));
    }
    format!("({})", parts.join("; "))
}

/// Keep only dishes at or under `max` (dropping merchants left with none).
pub fn filter_dish_price(ms: Vec<Merchant>, max: Option<Money>) -> Vec<Merchant> {
    let Some(max) = max else {
        return ms;
    };
    ms.into_iter()
        .filter_map(|mut m| {
            m.dishes
                .retain(|d| d.effective_price().is_some_and(|p| p <= max));
            (!m.dishes.is_empty()).then_some(m)
        })
        .collect()
}

/// `grab food search KEYWORD`: merchants (with dish hits) ranked for the
/// keyword. With `dishes`, dishes are ranked first; apply
/// [`ListOpts::dishes`] or [`ListOpts::apply`] + [`filter_dish_price`] to
/// shape the result.
pub async fn search(
    ctx: &mut Context,
    keyword: &str,
    opts: &ListOpts,
    dishes: bool,
    local: bool,
    max_price: Option<Money>,
) -> Result<Listing> {
    let loc = Location::from_session(ctx.session())?;
    let src = ListSource::Search(keyword.to_string());
    let mut listing = if local {
        local_listing(ctx, &loc, &src, opts, max_price, true).await?
    } else {
        get_listing(ctx, &loc, &src, opts, max_price, true).await?
    };
    discover::rank_dishes(&mut listing.merchants, keyword, dishes);
    Ok(listing)
}

/// `grab food nearby`: the empty-keyword search, i.e. everything that
/// delivers to the saved address. Filters are not applied (see
/// [`ListOpts::apply`]).
pub async fn nearby(ctx: &mut Context, opts: &ListOpts, local: bool) -> Result<Listing> {
    let loc = Location::from_session(ctx.session())?;
    let src = ListSource::Search(String::new());
    if local {
        local_listing(ctx, &loc, &src, opts, None, false).await
    } else {
        get_listing(ctx, &loc, &src, opts, None, false).await
    }
}

/// `grab food recommended`: "Popular near you".
pub async fn recommended(ctx: &mut Context, opts: &ListOpts) -> Result<Listing> {
    let loc = Location::from_session(ctx.session())?;
    let mut listing = get_listing(ctx, &loc, &ListSource::Recommended, opts, None, false).await?;
    // Recommended entries lack cuisines/fees; fill gaps from the catalog.
    for m in &mut listing.merchants {
        if let Ok(Some(hit)) = ctx.cache().restaurant(&m.id).await {
            let known = index::merchant_from_hit(&hit);
            if m.cuisines.is_empty() {
                m.cuisines = known.cuisines;
            }
            if m.business_type.is_none() {
                m.business_type = known.business_type;
            }
        }
    }
    Ok(listing)
}

/// A home collection from `/category/shortcuts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortcut {
    pub id: String,
    pub name: String,
    pub description: String,
}

pub fn parse_shortcuts(raw: &Value) -> Vec<Shortcut> {
    raw.get("shortcuts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let id = match s.get("id")? {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => return None,
            };
            Some(Shortcut {
                id,
                name: s
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                description: s
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            })
        })
        .collect()
}

async fn fetch_shortcuts(ctx: &mut Context, loc: &Location) -> Result<Fetched<Vec<Shortcut>>> {
    let key = CacheKey::new(CACHE_NS, CacheKind::Categories, "shortcuts")
        .location(loc.latitude, loc.longitude);
    let req = Request::get(api::shortcuts_url(&loc.latlng()), Hydra::NotNeeded);
    let f = match ctx.cached_call_meta(&key, &req).await {
        Ok(f) => f,
        Err(e) => match ctx.cache().lookup_stale::<Value>(&key).await {
            Some(stale) => stale,
            None => return Err(e),
        },
    };
    Ok(Fetched {
        value: parse_shortcuts(&f.value),
        cached_at: f.cached_at,
    })
}

/// The known cuisine shortcuts ([`CUISINES`]) plus the home collections
/// near the saved address.
#[derive(Debug)]
pub struct Cuisines {
    pub collections: Vec<Shortcut>,
    pub cached_at: Option<i64>,
    /// Why the collections couldn't be fetched (the cuisines still apply).
    pub error: Option<Error>,
}

/// `grab food cuisines`.
pub async fn cuisines(ctx: &mut Context) -> Result<Cuisines> {
    let loc = Location::from_session(ctx.session())?;
    Ok(match fetch_shortcuts(ctx, &loc).await {
        Ok(f) => Cuisines {
            collections: f.value,
            cached_at: f.cached_at,
            error: None,
        },
        Err(e) => Cuisines {
            collections: Vec::new(),
            cached_at: None,
            error: Some(e),
        },
    })
}

fn fold(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Resolve `<id|name>` against the known cuisines, then the collections.
fn resolve_in(arg: &str, collections: &[Shortcut]) -> Option<(String, String)> {
    let want = fold(arg);
    let known = CUISINES
        .iter()
        .map(|(id, name)| ((*id).to_string(), (*name).to_string()));
    let colls = collections.iter().map(|s| (s.id.clone(), s.name.clone()));
    let all: Vec<(String, String)> = known.chain(colls).collect();
    all.iter()
        .find(|(_, n)| fold(n) == want)
        .or_else(|| all.iter().find(|(_, n)| fold(n).starts_with(&want)))
        .or_else(|| all.iter().find(|(_, n)| fold(n).contains(&want)))
        .cloned()
}

/// `grab food category <id|name>`.
pub async fn category(
    ctx: &mut Context,
    arg: &str,
    opts: &ListOpts,
    local: bool,
) -> Result<Listing> {
    let loc = Location::from_session(ctx.session())?;
    let arg = arg.trim();
    if arg.is_empty() {
        return Err(Error::invalid("category id or name is empty"));
    }
    let (id, name) = if arg.chars().all(|c| c.is_ascii_digit()) {
        let name = CUISINES
            .iter()
            .find(|(i, _)| *i == arg)
            .map(|(_, n)| (*n).to_string())
            .unwrap_or_default();
        (arg.to_string(), name)
    } else if let Some(hit) = resolve_in(arg, &[]) {
        hit
    } else {
        let colls = fetch_shortcuts(ctx, &loc)
            .await
            .map(|f| f.value)
            .unwrap_or_default();
        resolve_in(arg, &colls).ok_or_else(|| {
            Error::not_found(format!(
                "no cuisine or collection named `{arg}`; see `grab food cuisines`"
            ))
        })?
    };
    let src = ListSource::Category {
        id,
        name: if name.is_empty() {
            arg.to_string()
        } else {
            name
        },
    };
    if local {
        local_listing(ctx, &loc, &src, opts, None, false).await
    } else {
        get_listing(ctx, &loc, &src, opts, None, false).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::Dish;

    #[test]
    fn shortcuts_and_resolution() {
        let raw: Value = serde_json::from_str(include_str!(
            "../../../docs/fixtures/discovery_shortcuts.json"
        ))
        .unwrap_or(Value::Null);
        let s = parse_shortcuts(&raw);
        assert!(s.iter().any(|s| s.id == "5047" && s.name == "Nearby"));
        assert_eq!(
            resolve_in("japanese", &s),
            Some(("89".into(), "Japanese".into()))
        );
        assert_eq!(
            resolve_in("fast", &s),
            Some(("62".into(), "Fast Food".into()))
        );
        assert_eq!(resolve_in("5-star", &s).map(|r| r.0), Some("23865".into()));
        assert_eq!(
            resolve_in("deal drop", &s).map(|r| r.0),
            Some("25865".into())
        );
        assert_eq!(resolve_in("nope nope", &s), None);
    }

    #[test]
    fn keys_differ_per_page_and_source() {
        let loc = Location {
            id: String::new(),
            latitude: 1.23,
            longitude: 103.8,
            address: String::new(),
            country_code: "PH".into(),
            address_detail: None,
            note_to_driver: None,
        };
        let s = ListSource::Search("Sisig ".into());
        assert_ne!(s.key(&loc, 0).hash(), s.key(&loc, 32).hash());
        assert_eq!(
            s.key(&loc, 0).hash(),
            ListSource::Search("sisig".into()).key(&loc, 0).hash()
        );
        let c = ListSource::Category {
            id: "89".into(),
            name: "Japanese".into(),
        };
        assert_ne!(c.key(&loc, 0).hash(), s.key(&loc, 0).hash());
        assert_eq!(ListSource::Recommended.page_step(), 8);
        let req = c.request(&loc, 32);
        assert!(req.url.contains("categoryShortcutID=89"));
        assert!(req.url.contains("offset=32"));
    }

    #[test]
    fn dish_price_filter_drops_empty_merchants() {
        let d = |p: i64| Dish {
            id: format!("D{p}"),
            price: Some(Money(p)),
            available: true,
            ..Dish::default()
        };
        let ms = vec![
            Merchant {
                id: "A".into(),
                dishes: vec![d(10_000), d(40_000)],
                ..Merchant::default()
            },
            Merchant {
                id: "B".into(),
                dishes: vec![d(50_000)],
                ..Merchant::default()
            },
        ];
        let out = filter_dish_price(ms, Some(Money(20_000)));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].dishes.len(), 1);
    }
}
