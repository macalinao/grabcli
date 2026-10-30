//! GrabFood: a typed API over [`grabcli_core::Context`].
//!
//! Every function takes a `&mut Context` and returns plain structs; nothing
//! here reads config files, picks paths or prints. Carts are values the
//! caller keeps ([`Cart`]); the `grab` CLI stores them as JSON files, a
//! daemon can keep them anywhere.
//!
//! - Browse: [`browse::search`], [`browse::nearby`], [`browse::recommended`],
//!   [`browse::cuisines`], [`browse::category`], [`menu`].
//! - Cart: [`cart_add`], [`cart_remove`] (plus [`Cart`]'s own fields).
//! - Checkout: [`quote`], [`vouchers`], [`best_voucher`], [`prepare_order`],
//!   [`submit_order`], [`place_order`].
//! - After: [`orders`], [`track`], [`history`].
//!
//! Build a context with [`context`], which registers the GrabFood URL
//! allowlist (the order endpoint only when `allow_ordering` is set) and
//! turns the response cache off; browse callers opt back in with
//! [`Context::set_cache_mode`]. The cart → quote → order path never reads
//! the cache.

pub mod api;
pub mod browse;
pub mod cart;
pub mod datetime;
pub mod discover;
pub mod history;
pub mod index;
pub mod menu;
pub mod order;
pub mod quote;
pub mod tracking;

use std::sync::Arc;
use std::time::Duration;

use grabcli_core::cache::{Cache, CacheKey, CacheKind, CacheMode, Fetched};
use grabcli_core::error::{Error, ErrorKind, Result};
use grabcli_core::http::{Hydra, Request};
use grabcli_core::location::Location;
use grabcli_core::money::Money;
use grabcli_core::{Config, Context, SessionStore};
use serde_json::{Value, json};

pub use cart::{Cart, CartItem, CartModifier};
pub use menu::Menu;
pub use quote::Quote;
pub use tracking::Tracking;

/// Cache namespace for GrabFood entries.
pub const CACHE_NS: &str = "food";

/// Most item images [`download_menu_images`] fetches per call.
pub const MAX_MENU_IMAGES: usize = 200;

/// A GrabFood context: [`Context::new`] plus the GrabFood allowlist, with
/// the cache off (cart/quote/voucher/order/tracking/history always talk to
/// Grab live). Browse callers may then [`Context::set_cache_mode`].
pub async fn context(
    config: Config,
    store: Arc<dyn SessionStore>,
    cache: Arc<dyn Cache>,
) -> Result<Context> {
    let allow = api::allowlist(config.allow_ordering);
    Ok(Context::new(config, store, cache)
        .await?
        .with_allowlist(&allow)?
        .with_cache_mode(CacheMode::Off))
}

/// Where a menu may come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuSource {
    /// The response cache (≤ menu TTL old) when the context's mode allows it.
    /// For browsing and for [`cart_add`]'s id validation only.
    Cached,
    /// Always a live fetch, never read from or written to the cache. Required
    /// on the quote/voucher/order path so prices and availability are fresh.
    Live,
}

/// The payment method id used when none is given: config, then session.
pub fn payment_default(ctx: &Context) -> String {
    ctx.config
        .payment_type_id
        .clone()
        .or_else(|| ctx.session().payment_type_id.clone())
        .unwrap_or_default()
}

/// A merchant's menu, modifiers, hours and offers.
pub async fn menu(ctx: &mut Context, merchant_id: &str, source: MenuSource) -> Result<Menu> {
    Ok(menu_meta(ctx, merchant_id, source).await?.value)
}

/// A menu plus when it was fetched (`cached_at` set if it came from the
/// cache). Live fetches through the cache are indexed into the catalog. For
/// browsing only, an expired login falls back to the guest menu endpoint.
pub async fn menu_meta(
    ctx: &mut Context,
    merchant_id: &str,
    source: MenuSource,
) -> Result<Fetched<Menu>> {
    let loc = Location::from_session(ctx.session())?;
    let req = Request::get(
        api::merchant_url(merchant_id, &loc.latlng()),
        Hydra::Required,
    );
    let resp = match source {
        MenuSource::Cached => {
            let key = CacheKey::new(CACHE_NS, CacheKind::Menu, "merchant")
                .param("id", merchant_id)
                .location(loc.latitude, loc.longitude);
            let fetched = match ctx.cached_call_meta(&key, &req).await {
                Err(e) if e.kind == ErrorKind::AuthExpired => {
                    let guest = Request::get(
                        api::guest_merchant_url(merchant_id, &loc.latlng()),
                        Hydra::NotNeeded,
                    );
                    ctx.cached_call_meta(&key, &guest).await.map_err(|_| e)?
                }
                other => other?,
            };
            if !fetched.is_cached() {
                index::index_menu(ctx.cache(), &fetched.value, (loc.latitude, loc.longitude)).await;
            }
            fetched
        }
        MenuSource::Live => Fetched::live(ctx.call(&req).await?),
    };
    if resp.value.get("merchant").is_some_and(Value::is_null) {
        return Err(Error::not_found(format!(
            "{merchant_id} has no food menu (mart stores are not supported yet)"
        )));
    }
    Ok(Fetched {
        value: Menu::parse(&resp.value)?,
        cached_at: resp.cached_at,
    })
}

/// Download up to [`MAX_MENU_IMAGES`] item images into the image cache, one
/// request at a time. Returns how many failed.
pub async fn download_menu_images(ctx: &Context, menu: &Menu) -> usize {
    let mut failed = 0;
    for it in menu.items.iter().take(MAX_MENU_IMAGES) {
        if let Some(url) = &it.image_url
            && ctx.cache().fetch_image(url).await.is_err()
        {
            failed += 1;
        }
    }
    failed
}

/// What [`cart_add`] added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    pub merchant_id: String,
    pub item_id: String,
    pub item_name: String,
    pub quantity: i64,
}

/// Add an item to `cart`, validating the ids against the merchant's menu.
/// `mods` are `GROUP_ID:OPTION_ID` choices. A cached menu (≤ menu TTL) is
/// fine here: the cart only stores ids, and [`quote`] re-validates against a
/// live menu before any price is shown.
pub async fn cart_add(
    ctx: &mut Context,
    cart: &mut Cart,
    merchant_id: &str,
    item_id: &str,
    quantity: i64,
    mods: &[String],
    note: Option<String>,
) -> Result<Added> {
    let menu = menu(ctx, merchant_id, MenuSource::Cached).await?;
    let item = menu.item(item_id).ok_or_else(|| {
        Error::new(
            ErrorKind::CartInvalid,
            format!("item {item_id} is not on {}'s menu", menu.name),
        )
    })?;
    let modifiers = parse_mods(mods, item)?;
    if !cart.is_empty() && cart.merchant_id != merchant_id {
        return Err(Error::new(
            ErrorKind::CartInvalid,
            format!(
                "cart already has items from {} — run `grab food cart clear` first",
                cart.merchant_id
            ),
        ));
    }
    cart.merchant_id = merchant_id.to_string();
    if !menu.section.is_empty() {
        cart.section = menu.section.clone();
    }
    cart.items.push(CartItem {
        item_id: item_id.to_string(),
        quantity,
        modifiers,
        comment: note.unwrap_or_default(),
    });
    Ok(Added {
        merchant_id: merchant_id.to_string(),
        item_id: item_id.to_string(),
        item_name: item.name.clone(),
        quantity,
    })
}

/// Remove every line with `item_id` from `cart`. Errors if there was none.
pub fn cart_remove(cart: &mut Cart, item_id: &str) -> Result<()> {
    let before = cart.items.len();
    cart.items.retain(|i| i.item_id != item_id);
    if cart.items.len() == before {
        return Err(Error::not_found(format!("no cart item {item_id}")));
    }
    Ok(())
}

/// Parse `GROUP_ID:OPTION_ID` modifier choices against a menu item.
pub fn parse_mods(mods: &[String], item: &menu::Item) -> Result<Vec<CartModifier>> {
    let mut out = Vec::new();
    for spec in mods {
        let (group, option) = spec.split_once(':').ok_or_else(|| {
            Error::invalid(format!("--mod must be GROUP_ID:OPTION_ID, got `{spec}`"))
        })?;
        let grp = item
            .modifier_groups
            .iter()
            .find(|g| g.id == group)
            .ok_or_else(|| Error::invalid(format!("no modifier group {group} on {}", item.name)))?;
        if !grp.modifiers.iter().any(|m| m.id == option) {
            return Err(Error::invalid(format!(
                "no option {option} in group {group}"
            )));
        }
        out.push(CartModifier {
            group_id: group.to_string(),
            modifier_id: option.to_string(),
            quantity: 1,
        });
    }
    Ok(out)
}

/// Price `cart` live (no order is placed). `voucher` overrides the cart's
/// stored code; `payment` overrides [`payment_default`]. The cart is
/// validated against the live menu first.
pub async fn quote(
    ctx: &mut Context,
    cart: &Cart,
    voucher: Option<&str>,
    payment: Option<&str>,
) -> Result<Quote> {
    if cart.is_empty() {
        return Err(Error::new(ErrorKind::CartInvalid, "cart is empty"));
    }
    let mut cart = cart.clone();
    // Validate against the live menu (catches removed items / missing required
    // mods). Never cached: this is the quote/order path.
    let menu = menu(ctx, &cart.merchant_id, MenuSource::Live).await?;
    cart.validate(&menu)?;
    // Items added from a guest menu (no section) take the live menu's.
    if cart.section.is_empty() {
        cart.section = menu.section.clone();
    }

    let loc = Location::from_session(ctx.session())?;
    let payment_id = payment
        .map(ToString::to_string)
        .unwrap_or_else(|| payment_default(ctx));
    let mut promos: Vec<String> = Vec::new();
    if let Some(v) = voucher.or(cart.voucher.as_deref()) {
        promos.push(v.to_string());
    }
    let body = quote::build_body(&cart, &loc, &payment_id, &promos);
    let resp = ctx
        .call(&Request::post(api::cart_url(), body, Hydra::Required))
        .await?;
    Quote::parse(&resp, &cart, &loc, &payment_id, &promos)
}

/// A quote as the CLI's `--json` shape.
pub fn quote_json(q: &Quote) -> Value {
    json!({
        "quote_id": q.id,
        "merchant": q.merchant_name,
        "subtotal_minor": q.subtotal.minor(),
        "delivery_before_minor": q.delivery_before.minor(),
        "delivery_after_minor": q.delivery_after.minor(),
        "fees": q.fees.iter().map(|f| json!({"name": f.name, "amount_minor": f.amount.minor()})).collect::<Vec<_>>(),
        "promos": q.promos.iter().map(|p| json!({"name": p.name, "amount_minor": p.amount.minor()})).collect::<Vec<_>>(),
        "total_before_minor": q.total_before.minor(),
        "total_minor": q.total_after.minor(),
        "total_php": q.total_after.as_major_f64(),
        "eta_min": q.eta_min,
        "payment_type_id": q.payment_type_id,
        "address": q.address,
        "address_detail": q.address_detail,
        "note_to_driver": q.note_to_driver,
    })
}

/// Eligible voucher codes for a cart, with the base (no-voucher) quote.
#[derive(Debug, Clone)]
pub struct VoucherList {
    pub base: Quote,
    pub candidates: Vec<String>,
}

/// List the voucher codes Grab says are eligible for `cart`.
pub async fn vouchers(ctx: &mut Context, cart: &Cart) -> Result<VoucherList> {
    if cart.is_empty() {
        return Err(Error::new(
            ErrorKind::CartInvalid,
            "cart is empty; add items before checking vouchers",
        ));
    }
    let loc = Location::from_session(ctx.session())?;
    // A base quote gives us the basket amount and delivery fee for the voucher query.
    let base = quote(ctx, cart, None, None).await?;
    let body = json!({"params": {
        "latitude": loc.latitude, "longitude": loc.longitude,
        "merchantIDs": [cart.merchant_id], "poiID": loc.id,
        "currency": "PHP", "basketAmount": base.subtotal.as_major_f64(),
        "deliveryFee": base.delivery_after.as_major_f64(),
        "isGrabPay": false, "isDinein": false, "isTakeaway": false,
    }});
    let resp = ctx
        .call(&Request::post(
            api::eligible_vouchers_url(),
            body,
            Hydra::Required,
        ))
        .await?;
    let candidates = resp
        .pointer("/result/items")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|i| i.get("promoCode").and_then(Value::as_str))
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(VoucherList { base, candidates })
}

/// One code tried by [`best_voucher`]: its total, or `None` if the quote
/// with it failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoucherTry {
    pub code: String,
    pub total: Option<Money>,
}

/// The cheapest voucher for a cart.
#[derive(Debug, Clone)]
pub struct BestVoucher {
    pub base_total: Money,
    /// `None` if no code beats the base total.
    pub best_code: Option<String>,
    pub best_total: Money,
    pub tried: Vec<VoucherTry>,
}

/// Re-quote `cart` with each eligible code (sequentially, ~1.2s apart) and
/// pick the lowest total. To use it, set `cart.voucher = best.best_code`.
pub async fn best_voucher(ctx: &mut Context, cart: &Cart) -> Result<BestVoucher> {
    let list = vouchers(ctx, cart).await?;
    let mut best = BestVoucher {
        base_total: list.base.total_after,
        best_code: None,
        best_total: list.base.total_after,
        tried: Vec::new(),
    };
    for code in &list.candidates {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let total = quote(ctx, cart, Some(code), None)
            .await
            .ok()
            .map(|q| q.total_after);
        if let Some(t) = total
            && t < best.best_total
        {
            best.best_total = t;
            best.best_code = Some(code.clone());
        }
        best.tried.push(VoucherTry {
            code: code.clone(),
            total,
        });
    }
    Ok(best)
}

/// A fresh quote and the exact order body that would be submitted for it.
#[derive(Debug, Clone)]
pub struct PreparedOrder {
    pub quote: Quote,
    body: Value,
}

impl PreparedOrder {
    /// The request body with payment/signature fields redacted, for display.
    pub fn redacted_body(&self) -> Value {
        order::redacted_body(&self.body)
    }
}

/// Re-quote `cart` live and build the order body (nothing is sent). Show
/// the quote to the user, then pass their confirmed quote id to
/// [`submit_order`].
pub async fn prepare_order(
    ctx: &mut Context,
    cart: &Cart,
    notes: Option<&str>,
) -> Result<PreparedOrder> {
    let q = quote(ctx, cart, None, None).await?;
    let loc = Location::from_session(ctx.session())?;
    let menu_integrated = menu(ctx, &cart.merchant_id, MenuSource::Live)
        .await
        .map(|m| m.is_integrated)
        .unwrap_or(true);
    let body = order::build_submit_body(&q, &loc, menu_integrated, notes);
    Ok(PreparedOrder { quote: q, body })
}

/// The `ordering_disabled` error (with the redacted body that would have
/// been sent) unless `allow_ordering` is set.
pub fn ensure_ordering_enabled(ctx: &Context, prepared: &PreparedOrder) -> Result<()> {
    if ctx.config.allow_ordering {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::OrderingDisabled,
        "ordering disabled: set allow_ordering = true in ~/.config/grabcli/config.toml to enable",
    )
    .with_details(json!({"would_send": prepared.redacted_body()})))
}

/// Submit a prepared order. Refuses unless `confirmed_quote_id` equals the
/// prepared (fresh) quote's id and `allow_ordering` is set; both checks run
/// before any request. Never retried: on an ambiguous failure the error says
/// to check ongoing orders. Returns the order id.
pub async fn submit_order(
    ctx: &mut Context,
    prepared: &PreparedOrder,
    confirmed_quote_id: &str,
) -> Result<String> {
    let q = &prepared.quote;
    if confirmed_quote_id.trim() != q.id {
        return Err(Error::new(
            ErrorKind::QuoteChanged,
            format!(
                "quote changed: you confirmed {confirmed_quote_id} but the fresh quote is {}; re-review and retry",
                q.id
            ),
        )
        .with_details(json!({
            "expected": confirmed_quote_id,
            "actual": q.id,
            "total_minor": q.total_after.minor(),
        })));
    }
    // Ordering must be explicitly enabled. Even past this, the allowlist is
    // the real gate: if ordering is off the URL isn't registered.
    ensure_ordering_enabled(ctx, prepared)?;

    // Refresh the session BEFORE submitting so a stale gfc_session can't cause a
    // mid-submit 401 (we never retry submit — no idempotency key). Best-effort.
    ctx.refresh_session().await.ok();

    // Submit. No idempotency key exists for this endpoint, so on ANY ambiguous
    // failure we do not retry — tell the caller to check ongoing orders.
    let req = Request::post(api::ORDERS, prepared.body.clone(), Hydra::Required).no_refresh();
    let resp = match ctx.call(&req).await {
        Ok(v) => v,
        Err(e) => {
            let ambiguous = matches!(
                e.kind,
                ErrorKind::Network | ErrorKind::RateLimited | ErrorKind::Api
            );
            let msg = if ambiguous {
                format!(
                    "{} — DID NOT retry; check `grab food orders --ongoing` before trying again",
                    e.message
                )
            } else {
                e.message.clone()
            };
            return Err(Error::new(e.kind, msg));
        }
    };
    order::parse_submit(&resp)
}

/// A placed order.
#[derive(Debug, Clone)]
pub struct Placed {
    pub order_id: String,
    pub quote: Quote,
}

/// Re-quote `cart`, and if the fresh quote id equals `quote_id` (the one the
/// user approved) and ordering is enabled, submit it once and empty the
/// cart. A mismatch returns `quote_changed` and nothing is sent.
pub async fn place_order(
    ctx: &mut Context,
    cart: &mut Cart,
    quote_id: &str,
    notes: Option<&str>,
) -> Result<Placed> {
    let prepared = prepare_order(ctx, cart, notes).await?;
    let order_id = submit_order(ctx, &prepared, quote_id).await?;
    *cart = Cart::default();
    Ok(Placed {
        order_id,
        quote: prepared.quote,
    })
}

/// One order in the order list.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderSummary {
    pub order_id: String,
    pub state: String,
    pub short_number: String,
    pub created_at: String,
    pub merchant_id: Option<String>,
    /// The upstream object, for fields not lifted out here.
    pub raw: Value,
}

impl OrderSummary {
    fn parse(o: &Value) -> Self {
        let s = |k: &str| o.get(k).and_then(Value::as_str).map(ToString::to_string);
        Self {
            order_id: s("orderID").unwrap_or_else(|| "?".into()),
            state: s("orderState").unwrap_or_else(|| "?".into()),
            short_number: s("shortOrderNumber").unwrap_or_default(),
            created_at: s("createdAt").unwrap_or_default(),
            merchant_id: s("merchantID"),
            raw: o.clone(),
        }
    }

    /// The CLI's `--json` shape.
    pub fn to_json(&self) -> Value {
        let o = &self.raw;
        json!({
            "order_id": o.get("orderID"),
            "booking_code": o.get("bookingCode"),
            "short_number": o.get("shortOrderNumber"),
            "state": o.get("orderState"),
            "final_state": o.get("finalState"),
            "order_type": o.get("orderType"),
            "merchant_id": o.get("merchantID"),
            "created_at": o.get("createdAt"),
            "updated_at": o.get("updatedAt"),
        })
    }
}

/// Recent orders (only ongoing ones with `ongoing`).
pub async fn orders(ctx: &mut Context, ongoing: bool, limit: u32) -> Result<Vec<OrderSummary>> {
    let url = format!(
        "{}?input.limit={limit}&input.offset=0&input.onlyOngoingOrders={ongoing}",
        api::history_url(),
    );
    let resp = ctx.call(&Request::get(url, Hydra::Required)).await?;
    Ok(resp
        .pointer("/response/orders")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(OrderSummary::parse)
        .collect())
}

/// One order's live status: state, driver, ETA, route, items.
pub async fn track(ctx: &mut Context, order_id: &str) -> Result<Tracking> {
    tracking::check_order_id(order_id)?;
    let track = ctx
        .call(&Request::get(
            api::order_track_url(order_id),
            Hydra::Required,
        ))
        .await?;
    // Detail enriches with driver/merchant/items; tolerate its absence.
    let detail = ctx
        .call(&Request::get(api::order_get_url(order_id), Hydra::Required))
        .await
        .ok();
    tracking::build(order_id, &track, detail.as_ref())
}

/// Completed orders, newest first, up to `limit` scanned (paged gently).
pub async fn history(ctx: &mut Context, limit: u32) -> Result<Vec<history::OrderRecord>> {
    let mut records = Vec::new();
    let mut offset = 0u32;
    let page = 20u32;
    while offset < limit {
        let url = format!(
            "{}?input.limit={page}&input.offset={offset}&input.onlyOngoingOrders=false",
            api::history_url()
        );
        let resp = ctx.call(&Request::get(url, Hydra::Required)).await?;
        let orders = resp
            .pointer("/response/orders")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if orders.is_empty() {
            break;
        }
        records.extend(orders.iter().filter_map(history::parse_order));
        let has_more = resp
            .pointer("/response/hasMore")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !has_more {
            break;
        }
        offset += page;
        // Be gentle with the history endpoint (429s when hammered).
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use grabcli_core::cache::memory::NoCache;
    use grabcli_core::{MemorySessionStore, Session};

    async fn ctx(allow_ordering: bool) -> Result<Context> {
        let config = Config {
            allow_ordering,
            ..Config::default()
        };
        let store = Arc::new(MemorySessionStore::new(Some(Session::default())));
        context(config, store, Arc::new(NoCache)).await
    }

    fn prepared() -> PreparedOrder {
        PreparedOrder {
            quote: Quote {
                id: "abc123abc123".into(),
                ..Quote::default()
            },
            body: json!({"cartSignature": "sig"}),
        }
    }

    // Both refusals happen before any request: the context has no session
    // cookies and an unreachable network would surface as a different error.
    #[tokio::test]
    async fn submit_refuses_a_different_quote_id() -> Result<()> {
        let mut c = ctx(true).await?;
        let err = submit_order(&mut c, &prepared(), "zzz999zzz999")
            .await
            .err()
            .ok_or_else(|| Error::invalid("submitted"))?;
        assert_eq!(err.kind, ErrorKind::QuoteChanged);
        Ok(())
    }

    #[tokio::test]
    async fn submit_refuses_when_ordering_is_disabled() -> Result<()> {
        let mut c = ctx(false).await?;
        let err = submit_order(&mut c, &prepared(), "abc123abc123")
            .await
            .err()
            .ok_or_else(|| Error::invalid("submitted"))?;
        assert_eq!(err.kind, ErrorKind::OrderingDisabled);
        assert!(err.details.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn empty_cart_is_not_quoted() -> Result<()> {
        let mut c = ctx(false).await?;
        let err = quote(&mut c, &Cart::default(), None, None)
            .await
            .err()
            .ok_or_else(|| Error::invalid("quoted"))?;
        assert_eq!(err.kind, ErrorKind::CartInvalid);
        Ok(())
    }

    #[test]
    fn cart_remove_needs_the_item() {
        let mut cart = Cart {
            merchant_id: "2-EX".into(),
            items: vec![CartItem {
                item_id: "I1".into(),
                quantity: 1,
                modifiers: Vec::new(),
                comment: String::new(),
            }],
            ..Cart::default()
        };
        assert!(cart_remove(&mut cart, "I2").is_err());
        assert!(cart_remove(&mut cart, "I1").is_ok());
        assert!(cart.is_empty());
    }
}
