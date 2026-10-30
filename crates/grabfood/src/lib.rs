//! GrabFood service: the `grab food …` subcommands and the API client.
//!
//! The binary crate composes [`FoodCmd`] (via the generated [`food_cmd`]
//! parser) under its own `food` command and calls [`run`]. Everything here goes
//! through [`grabcli_core::Context`], so a future service reuses the same
//! session/auth/HTTP plumbing.

pub mod api;
pub mod browse;
pub mod cart;
pub mod datetime;
pub mod discover;
pub mod history;
pub mod index;
pub mod map;
pub mod menu;
pub mod order;
pub mod quote;
pub mod tracking;

use bpaf::Bpaf;
use grabcli_core::Context;
use grabcli_core::cache::{CacheKey, CacheKind, CacheMode};
use grabcli_core::error::{Error, ErrorKind, Result};
use grabcli_core::http::{Hydra, Request};
use grabcli_core::location::Location;
use grabcli_core::money::Money;
use grabcli_core::output::Output;
use serde_json::{Value, json};

use cart::{Cart, CartItem, CartModifier};
use discover::{ListOpts, Pesos, list_opts};
use menu::Menu;
use quote::Quote;

/// `grab food <cmd>`.
#[derive(Debug, Clone, Bpaf)]
pub enum FoodCmd {
    /// Search merchants and dishes by keyword near the saved address.
    #[bpaf(command("search"))]
    Search {
        /// Output JSON.
        #[bpaf(long("json"), switch)]
        json: bool,
        /// List matching dishes across restaurants instead of restaurants.
        #[bpaf(long("dishes"), switch)]
        dishes: bool,
        /// Search the local catalog only (offline; no API call).
        #[bpaf(long("local"), switch)]
        local: bool,
        /// Only dishes at or under this price in pesos.
        #[bpaf(long("max-price"), argument("PESOS"))]
        max_price: Option<Pesos>,
        #[bpaf(external(list_opts))]
        opts: ListOpts,
        #[bpaf(positional("KEYWORD"))]
        keyword: String,
    },

    /// List restaurants that deliver to the saved address.
    #[bpaf(command("nearby"))]
    Nearby {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Use the local catalog only (offline; no API call).
        #[bpaf(long("local"), switch)]
        local: bool,
        #[bpaf(external(list_opts))]
        opts: ListOpts,
    },

    /// "Popular near you" restaurants.
    #[bpaf(command("recommended"))]
    Recommended {
        #[bpaf(long("json"), switch)]
        json: bool,
        #[bpaf(external(list_opts))]
        opts: ListOpts,
    },

    /// List cuisines and the home collections near the saved address.
    #[bpaf(command("cuisines"))]
    Cuisines {
        #[bpaf(long("json"), switch)]
        json: bool,
    },

    /// List restaurants in a cuisine or collection (id or name).
    #[bpaf(command("category"))]
    Category {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Use the local catalog only (offline; no API call).
        #[bpaf(long("local"), switch)]
        local: bool,
        #[bpaf(external(list_opts))]
        opts: ListOpts,
        #[bpaf(positional("ID|NAME"))]
        category: String,
    },

    /// Show a merchant's menu, modifiers, hours and offers.
    #[bpaf(command("menu"))]
    Menu {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Download item images into the local image cache (paths in --json).
        #[bpaf(long("images"), switch)]
        images: bool,
        #[bpaf(positional("MERCHANT_ID"))]
        merchant_id: String,
    },

    /// Manage the local cart (one merchant at a time).
    #[bpaf(command("cart"))]
    Cart {
        #[bpaf(external(cart_cmd))]
        cmd: CartCmd,
    },

    /// Price the current cart (no order is placed).
    #[bpaf(command("quote"))]
    Quote {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Apply a voucher/promo code.
        #[bpaf(long("voucher"), argument("CODE"))]
        voucher: Option<String>,
        /// Payment method id (overrides config/session default).
        #[bpaf(long("payment"), argument("ID"))]
        payment: Option<String>,
    },

    /// List eligible vouchers and merchant offers.
    #[bpaf(command("vouchers"))]
    Vouchers {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Re-quote with each candidate and report the cheapest.
        #[bpaf(long("best"), switch)]
        best: bool,
        /// Store the best voucher in the cart (implies --best).
        #[bpaf(long("apply"), switch)]
        apply: bool,
    },

    /// Re-quote the cart and (if enabled + confirmed) place the order.
    #[bpaf(command("order"))]
    Order {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Confirm non-interactively: must equal the fresh quote id.
        #[bpaf(long("yes"), argument("QUOTE_ID"))]
        yes: Option<String>,
        /// Print the exact (redacted) request that would be sent and stop.
        #[bpaf(long("dry-run"), switch)]
        dry_run: bool,
        /// Extra driver notes, appended to the saved note.
        #[bpaf(long("notes"), argument("TEXT"))]
        notes: Option<String>,
    },

    /// List recent orders.
    #[bpaf(command("orders"))]
    Orders {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Only ongoing orders.
        #[bpaf(long("ongoing"), switch)]
        ongoing: bool,
        /// Max orders to return.
        #[bpaf(long("limit"), argument("N"), fallback(20))]
        limit: u32,
    },

    /// Show one order's live status (driver, ETA, route).
    #[bpaf(command("track"))]
    Track {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Poll until the order reaches a final state (>=30s interval).
        #[bpaf(long("watch"), switch)]
        watch: bool,
        /// Render a static map PNG (driver, restaurant, drop-off, route) here.
        #[bpaf(long("map"), argument("PATH"))]
        map: Option<std::path::PathBuf>,
        #[bpaf(positional("ORDER_ID"))]
        order_id: String,
    },

    /// Export order history to a Markdown/Obsidian vault.
    #[bpaf(command("history"))]
    History {
        #[bpaf(external(history_cmd))]
        cmd: HistoryCmd,
    },
}

/// `grab food history <cmd>`.
#[derive(Debug, Clone, Bpaf)]
pub enum HistoryCmd {
    /// Sync completed orders into a vault directory (idempotent).
    #[bpaf(command("sync"))]
    Sync {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Target vault directory.
        #[bpaf(long("vault"), argument("DIR"))]
        vault: std::path::PathBuf,
        /// Max orders to scan from the API.
        #[bpaf(long("limit"), argument("N"), fallback(500))]
        limit: u32,
    },
}

/// `grab food cart <cmd>`.
#[derive(Debug, Clone, Bpaf)]
pub enum CartCmd {
    /// Add an item. Repeat --mod GROUP:OPTION for each modifier choice.
    #[bpaf(command("add"))]
    Add {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Quantity.
        #[bpaf(long("qty"), argument("N"), fallback(1))]
        qty: i64,
        /// Modifier choice as GROUP_ID:OPTION_ID (repeatable).
        #[bpaf(long("mod"), argument("GROUP:OPTION"))]
        mods: Vec<String>,
        /// Item comment.
        #[bpaf(long("note"), argument("TEXT"))]
        note: Option<String>,
        #[bpaf(positional("MERCHANT_ID"))]
        merchant_id: String,
        #[bpaf(positional("ITEM_ID"))]
        item_id: String,
    },
    /// Remove an item by id.
    #[bpaf(command("rm"))]
    Rm {
        #[bpaf(long("json"), switch)]
        json: bool,
        #[bpaf(positional("ITEM_ID"))]
        item_id: String,
    },
    /// Show the current cart.
    #[bpaf(command("show"))]
    Show {
        #[bpaf(long("json"), switch)]
        json: bool,
    },
    /// Empty the cart.
    #[bpaf(command("clear"))]
    Clear {
        #[bpaf(long("json"), switch)]
        json: bool,
    },
}

/// True if any leaf of the command carried `--json` (so `grab food quote --json`
/// works as well as a top-level flag).
pub fn cmd_wants_json(cmd: &FoodCmd) -> bool {
    match cmd {
        FoodCmd::Search { json, .. }
        | FoodCmd::Nearby { json, .. }
        | FoodCmd::Recommended { json, .. }
        | FoodCmd::Cuisines { json }
        | FoodCmd::Category { json, .. }
        | FoodCmd::Menu { json, .. }
        | FoodCmd::Quote { json, .. }
        | FoodCmd::Vouchers { json, .. }
        | FoodCmd::Order { json, .. }
        | FoodCmd::Orders { json, .. }
        | FoodCmd::Track { json, .. } => *json,
        FoodCmd::Cart { cmd } => match cmd {
            CartCmd::Add { json, .. }
            | CartCmd::Rm { json, .. }
            | CartCmd::Show { json, .. }
            | CartCmd::Clear { json, .. } => *json,
        },
        FoodCmd::History { cmd } => match cmd {
            HistoryCmd::Sync { json, .. } => *json,
        },
    }
}

/// Run a GrabFood command. `cache` is the global cache mode (`--no-cache` /
/// `--refresh`); it only affects read-only browse commands.
pub async fn run(out: Output, cache: CacheMode, cmd: FoodCmd) -> Result<()> {
    match cmd {
        FoodCmd::Search {
            keyword,
            dishes,
            local,
            max_price,
            opts,
            ..
        } => {
            let mut ctx = browse_context(out, cache)?;
            browse::search(&mut ctx, &keyword, &opts, dishes, local, max_price).await
        }
        FoodCmd::Nearby { local, opts, .. } => {
            browse::nearby(&mut browse_context(out, cache)?, &opts, local).await
        }
        FoodCmd::Recommended { opts, .. } => {
            browse::recommended(&mut browse_context(out, cache)?, &opts).await
        }
        FoodCmd::Cuisines { .. } => browse::cuisines(&mut browse_context(out, cache)?).await,
        FoodCmd::Category {
            local,
            opts,
            category,
            ..
        } => browse::category(&mut browse_context(out, cache)?, &category, &opts, local).await,
        FoodCmd::Menu {
            merchant_id,
            images,
            ..
        } => menu_cmd(out, cache, &merchant_id, images).await,
        FoodCmd::Cart { cmd } => cart_cmd_run(out, cache, cmd).await,
        FoodCmd::Quote {
            voucher, payment, ..
        } => quote_cmd(out, voucher.as_deref(), payment.as_deref()).await,
        FoodCmd::Vouchers { best, apply, .. } => vouchers_cmd(out, best || apply, apply).await,
        FoodCmd::Order {
            yes,
            dry_run,
            notes,
            ..
        } => order_cmd(out, yes.as_deref(), dry_run, notes.as_deref()).await,
        FoodCmd::Orders { ongoing, limit, .. } => orders_cmd(out, ongoing, limit).await,
        FoodCmd::Track {
            watch,
            map,
            order_id,
            ..
        } => track_cmd(out, &order_id, watch, map.as_deref()).await,
        FoodCmd::History { cmd } => match cmd {
            HistoryCmd::Sync { vault, limit, .. } => history_sync(out, &vault, limit).await,
        },
    }
}

/// Cache namespace for GrabFood entries.
const CACHE_NS: &str = "food";

// A context whose allowlist reflects the current ordering setting. Its cache is
// off: cart/quote/voucher/order/tracking/history commands always talk to Grab
// live. Browse commands use [`browse_context`] instead.
fn context(out: Output) -> Result<Context> {
    let allow_ordering = {
        let cfg = grabcli_core::Config::load(&grabcli_core::paths::config_path()?)?;
        cfg.allow_ordering
    };
    Ok(Context::new(out, api::allowlist(allow_ordering))?.with_cache_mode(CacheMode::Off))
}

// A context for read-only browse commands (search, menu, cart add), honouring
// the global cache mode.
fn browse_context(out: Output, cache: CacheMode) -> Result<Context> {
    Ok(context(out)?.with_cache_mode(cache))
}

/// Where a menu may come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuSource {
    /// The response cache (≤ menu TTL old) when the context's mode allows it.
    /// For browsing and for `cart add`'s id validation only.
    Cached,
    /// Always a live fetch, never read from or written to the cache. Required
    /// on the quote/voucher/order path so prices and availability are fresh.
    Live,
}

fn payment_default(ctx: &Context) -> String {
    ctx.config
        .payment_type_id
        .clone()
        .or_else(|| ctx.session().payment_type_id.clone())
        .unwrap_or_default()
}

async fn fetch_menu(ctx: &mut Context, merchant_id: &str, source: MenuSource) -> Result<Menu> {
    Ok(fetch_menu_meta(ctx, merchant_id, source).await?.value)
}

/// A menu plus when it was fetched (`cached_at` set if it came from the
/// cache). Live fetches through the cache are indexed into the catalog. For
/// browsing only, an expired login falls back to the guest menu endpoint.
async fn fetch_menu_meta(
    ctx: &mut Context,
    merchant_id: &str,
    source: MenuSource,
) -> Result<grabcli_core::Fetched<Menu>> {
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
                index::index_menu(ctx.cache(), &fetched.value, (loc.latitude, loc.longitude));
            }
            fetched
        }
        MenuSource::Live => grabcli_core::Fetched::live(ctx.call(&req).await?),
    };
    if resp.value.get("merchant").is_some_and(Value::is_null) {
        return Err(Error::not_found(format!(
            "{merchant_id} has no food menu (mart stores are not supported yet)"
        )));
    }
    Ok(grabcli_core::Fetched {
        value: Menu::parse(&resp.value)?,
        cached_at: resp.cached_at,
    })
}

async fn menu_cmd(out: Output, cache: CacheMode, merchant_id: &str, images: bool) -> Result<()> {
    let mut ctx = browse_context(out, cache)?;
    let fetched = fetch_menu_meta(&mut ctx, merchant_id, MenuSource::Cached).await?;
    let menu = &fetched.value;
    let mut image_errors = 0usize;
    if images {
        // Sequential and capped: one CDN request at a time.
        for it in menu.items.iter().take(MAX_MENU_IMAGES) {
            if let Some(url) = &it.image_url
                && ctx.cache().fetch_image(url).await.is_err()
            {
                image_errors += 1;
            }
        }
    }

    if out.is_json() {
        let mut j = menu_json(menu, ctx.cache());
        j["cached_at"] = json!(fetched.cached_at);
        j["age_secs"] = json!(fetched.age_secs());
        if images {
            j["image_errors"] = json!(image_errors);
        }
        out.json(&j);
        return Ok(());
    }
    let eta = menu
        .eta_min
        .map(|e| format!("{e} min"))
        .unwrap_or_else(|| "?".into());
    out.line(format!(
        "{}  {}  hours {}  eta {eta}  section {}",
        menu.name,
        if menu.open { "OPEN" } else { "CLOSED" },
        menu.hours,
        menu.section
    ));
    if let Some(age) = fetched.age_secs() {
        out.line(format!(
            "(cached {} ago)",
            grabcli_core::cache::human_age(age)
        ));
    }
    if images {
        let n = menu
            .items
            .iter()
            .take(MAX_MENU_IMAGES)
            .filter(|i| i.image_url.is_some())
            .count();
        out.line(format!(
            "images: {} cached, {image_errors} failed (see --json for paths)",
            n.saturating_sub(image_errors)
        ));
    }
    if !menu.offers.is_empty() {
        out.line("offers:");
        for o in &menu.offers {
            let r = o
                .redemption_uuid
                .as_deref()
                .map(|u| format!("  redemption {u}"))
                .unwrap_or_default();
            out.line(format!("  {} — {}{r}", o.title, o.subtitle));
        }
    }
    for it in &menu.items {
        let avail = if it.available { "" } else { "  (unavailable)" };
        out.line(format!(
            "  {}  {:>10}  [{}] {}{avail}",
            it.id,
            it.price.peso(),
            it.category,
            it.name
        ));
        for g in &it.modifier_groups {
            let req = if g.required() { "required" } else { "optional" };
            out.line(format!(
                "      group {} \"{}\" ({req}, {}..={})",
                g.id, g.name, g.min, g.max
            ));
            for m in &g.modifiers {
                let def = if m.is_default { " [default]" } else { "" };
                let extra = if m.price.minor() != 0 {
                    format!(" (+{})", m.price.peso())
                } else {
                    String::new()
                };
                out.line(format!("        {} {}{extra}{def}", m.id, m.name));
            }
        }
    }
    Ok(())
}

/// Most item images `grab food menu --images` downloads per run.
const MAX_MENU_IMAGES: usize = 200;

fn menu_json(menu: &Menu, cache: &grabcli_core::Cache) -> Value {
    let items: Vec<Value> = menu
        .items
        .iter()
        .map(|it| {
            let groups: Vec<Value> = it
                .modifier_groups
                .iter()
                .map(|g| {
                    json!({
                        "id": g.id, "name": g.name, "required": g.required(),
                        "selection_type": g.selection_type, "min": g.min, "max": g.max,
                        "options": g.modifiers.iter().map(|m| json!({
                            "id": m.id, "name": m.name, "price_minor": m.price.minor(),
                            "available": m.available, "default": m.is_default,
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect();
            json!({
                "id": it.id, "name": it.name, "category": it.category,
                "price_minor": it.price.minor(), "available": it.available,
                "description": it.description, "image_url": it.image_url,
                "image_path": it.image_url.as_deref()
                    .and_then(|u| cache.cached_image(u))
                    .map(|i| i.path.display().to_string()),
                "modifier_groups": groups,
            })
        })
        .collect();
    json!({
        "merchant_id": menu.merchant_id, "name": menu.name, "section": menu.section,
        "open": menu.open, "hours": menu.hours, "eta_min": menu.eta_min,
        "offers": menu.offers.iter().map(|o| json!({
            "title": o.title, "subtitle": o.subtitle,
            "redemption_uuid": o.redemption_uuid, "offer_id": o.offer_id,
        })).collect::<Vec<_>>(),
        "items": items,
    })
}

async fn cart_cmd_run(out: Output, cache: CacheMode, cmd: CartCmd) -> Result<()> {
    match cmd {
        CartCmd::Add {
            qty,
            mods,
            note,
            merchant_id,
            item_id,
            ..
        } => {
            let mut ctx = browse_context(out, cache)?;
            // Validate ids against the menu. A cached menu (≤ menu TTL) is
            // fine here: the cart only stores ids, and `quote`/`order`
            // re-validate against a live menu before any price is shown.
            let menu = fetch_menu(&mut ctx, &merchant_id, MenuSource::Cached).await?;
            let item = menu.item(&item_id).ok_or_else(|| {
                Error::new(
                    ErrorKind::CartInvalid,
                    format!("item {item_id} is not on {}'s menu", menu.name),
                )
            })?;
            let modifiers = parse_mods(&mods, item)?;

            let mut cart = Cart::load()?;
            if !cart.is_empty() && cart.merchant_id != merchant_id {
                return Err(Error::new(
                    ErrorKind::CartInvalid,
                    format!(
                        "cart already has items from {} — run `grab food cart clear` first",
                        cart.merchant_id
                    ),
                ));
            }
            cart.merchant_id = merchant_id.clone();
            if !menu.section.is_empty() {
                cart.section = menu.section.clone();
            }
            cart.items.push(CartItem {
                item_id: item_id.clone(),
                quantity: qty,
                modifiers,
                comment: note.unwrap_or_default(),
            });
            cart.save()?;
            if out.is_json() {
                out.json(&json!({"added": item_id, "quantity": qty, "merchant": merchant_id}));
            } else {
                out.line(format!("added {qty}x {} ({item_id})", item.name));
            }
            Ok(())
        }
        CartCmd::Rm { item_id, .. } => {
            let mut cart = Cart::load()?;
            let before = cart.items.len();
            cart.items.retain(|i| i.item_id != item_id);
            if cart.items.len() == before {
                return Err(Error::not_found(format!("no cart item {item_id}")));
            }
            if cart.items.is_empty() {
                Cart::clear()?;
            } else {
                cart.save()?;
            }
            if out.is_json() {
                out.json(&json!({"removed": item_id}));
            } else {
                out.line(format!("removed {item_id}"));
            }
            Ok(())
        }
        CartCmd::Show { .. } => {
            let cart = Cart::load()?;
            if out.is_json() {
                out.json(&serde_json::to_value(&cart)?);
            } else if cart.is_empty() {
                out.line("cart is empty");
            } else {
                out.line(format!(
                    "merchant {}  section {}",
                    cart.merchant_id, cart.section
                ));
                for i in &cart.items {
                    out.line(format!(
                        "  {}x {}{}",
                        i.quantity,
                        i.item_id,
                        if i.modifiers.is_empty() {
                            String::new()
                        } else {
                            format!("  ({} mods)", i.modifiers.len())
                        }
                    ));
                }
                if let Some(v) = &cart.voucher {
                    out.line(format!("voucher {v}"));
                }
            }
            Ok(())
        }
        CartCmd::Clear { .. } => {
            Cart::clear()?;
            if out.is_json() {
                out.json(&json!({"cleared": true}));
            } else {
                out.line("cart cleared");
            }
            Ok(())
        }
    }
}

fn parse_mods(mods: &[String], item: &menu::Item) -> Result<Vec<CartModifier>> {
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

// Re-quote the current cart; returns (quote, raw response).
async fn do_quote(
    ctx: &mut Context,
    voucher: Option<&str>,
    payment: Option<&str>,
) -> Result<Quote> {
    let mut cart = Cart::load()?;
    if cart.is_empty() {
        return Err(Error::new(ErrorKind::CartInvalid, "cart is empty"));
    }
    // Validate against the live menu (catches removed items / missing required
    // mods). Never cached: this is the quote/order path.
    let menu = fetch_menu(ctx, &cart.merchant_id, MenuSource::Live).await?;
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

fn quote_json(q: &Quote) -> Value {
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

fn print_quote_human(out: Output, q: &Quote) {
    out.line(format!("{}  (quote {})", q.merchant_name, q.id));
    out.line(format!("  subtotal      {:>12}", q.subtotal.peso()));
    if q.delivery_after != q.delivery_before {
        out.line(format!(
            "  delivery      {:>12}  (was {})",
            q.delivery_after.peso(),
            q.delivery_before.peso()
        ));
    } else {
        out.line(format!("  delivery      {:>12}", q.delivery_after.peso()));
    }
    for f in &q.fees {
        if f.name != "Delivery fee" {
            out.line(format!("  {:<13} {:>12}", f.name, f.amount.peso()));
        }
    }
    for p in &q.promos {
        out.line(format!("  {:<13} {:>12}", p.name, p.amount.peso()));
    }
    out.line(format!(
        "  total         {:>12}  (before promos {})",
        q.total_after.peso(),
        q.total_before.peso()
    ));
    if let Some(e) = q.eta_min {
        out.line(format!("  eta           {e} min"));
    }
    let pay = if q.payment_type_id.is_empty() {
        "(none set)".to_string()
    } else {
        q.payment_type_id.clone()
    };
    out.line(format!("  payment       {pay}"));
    out.line(format!("  deliver to    {}", q.address));
    if let Some(d) = &q.address_detail {
        out.line(format!("  unit          {d}"));
    }
    if let Some(n) = &q.note_to_driver {
        out.line(format!("  driver note   {n}"));
    }
}

async fn quote_cmd(out: Output, voucher: Option<&str>, payment: Option<&str>) -> Result<()> {
    let mut ctx = context(out)?;
    let q = do_quote(&mut ctx, voucher, payment).await?;
    save_last_quote(&q)?;
    if out.is_json() {
        out.json(&quote_json(&q));
    } else {
        print_quote_human(out, &q);
    }
    Ok(())
}

fn last_quote_path() -> Result<std::path::PathBuf> {
    Ok(grabcli_core::paths::state_dir()?
        .join("food")
        .join("last-quote.json"))
}

fn save_last_quote(q: &Quote) -> Result<()> {
    let v = json!({"quote_id": q.id, "signature": q.quote_signature, "total_minor": q.total_after.minor()});
    grabcli_core::paths::write_atomic(&last_quote_path()?, &serde_json::to_vec_pretty(&v)?)
}

async fn vouchers_cmd(out: Output, best: bool, apply: bool) -> Result<()> {
    let mut ctx = context(out)?;
    let cart = Cart::load()?;
    if cart.is_empty() {
        return Err(Error::new(
            ErrorKind::CartInvalid,
            "cart is empty; add items before checking vouchers",
        ));
    }
    let loc = Location::from_session(ctx.session())?;
    // A base quote gives us the basket amount and delivery fee for the voucher query.
    let base = do_quote(&mut ctx, None, None).await?;
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
    let candidates: Vec<String> = resp
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

    if !best {
        if out.is_json() {
            out.json(
                &json!({"base_total_minor": base.total_after.minor(), "candidates": candidates}),
            );
        } else {
            out.line(format!("base total {}", base.total_after.peso()));
            if candidates.is_empty() {
                out.line("no eligible voucher codes");
            }
            for c in &candidates {
                out.line(format!("  {c}"));
            }
        }
        return Ok(());
    }

    // Re-quote with each candidate, gently and sequentially, pick the lowest.
    let mut best_code: Option<String> = None;
    let mut best_total = base.total_after;
    let mut rows = Vec::new();
    for code in &candidates {
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        match do_quote(&mut ctx, Some(code), None).await {
            Ok(q) => {
                rows.push((code.clone(), q.total_after));
                if q.total_after < best_total {
                    best_total = q.total_after;
                    best_code = Some(code.clone());
                }
            }
            Err(_) => rows.push((code.clone(), Money(i64::MAX))),
        }
    }

    if apply {
        let mut cart = Cart::load()?;
        cart.voucher = best_code.clone();
        cart.save()?;
    }

    if out.is_json() {
        out.json(&json!({
            "base_total_minor": base.total_after.minor(),
            "best_code": best_code,
            "best_total_minor": best_total.minor(),
            "applied": apply,
            "tried": rows.iter().map(|(c, t)| json!({"code": c, "total_minor": t.minor()})).collect::<Vec<_>>(),
        }));
    } else {
        out.line(format!("base total {}", base.total_after.peso()));
        for (c, t) in &rows {
            let shown = if t.minor() == i64::MAX {
                "error".to_string()
            } else {
                t.peso()
            };
            out.line(format!("  {c}: {shown}"));
        }
        match &best_code {
            Some(c) => out.line(format!(
                "best: {c} -> {}{}",
                best_total.peso(),
                if apply { " (applied)" } else { "" }
            )),
            None => out.line("no voucher beats the base total"),
        }
    }
    Ok(())
}

async fn order_cmd(
    out: Output,
    yes: Option<&str>,
    dry_run: bool,
    notes: Option<&str>,
) -> Result<()> {
    let mut ctx = context(out)?;
    let cart = Cart::load()?;
    let q = do_quote(&mut ctx, None, None).await?;
    save_last_quote(&q)?;

    let loc = Location::from_session(ctx.session())?;
    let menu_integrated = fetch_menu(&mut ctx, &cart.merchant_id, MenuSource::Live)
        .await
        .map(|m| m.is_integrated)
        .unwrap_or(true);
    let body = order::build_submit_body(&q, &loc, menu_integrated, notes);

    // Always show the full quote first.
    if out.is_json() {
        let mut j = quote_json(&q);
        j["would_send"] = order::redacted_body(&body);
        out.json(&j);
    } else {
        print_quote_human(out, &q);
    }

    if dry_run {
        if !out.is_json() {
            out.line("dry run — would POST /order/orders:");
            out.line(
                serde_json::to_string_pretty(&order::redacted_body(&body)).unwrap_or_default(),
            );
        }
        return Ok(());
    }

    // Confirmation gate.
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let tty_confirmed = if yes.is_none() && is_tty && !out.is_json() {
        Some(prompt_confirm(&q))
    } else {
        None
    };
    match order::gate(&q.id, yes, is_tty, tty_confirmed) {
        order::Gate::QuoteChanged { expected, actual } => {
            return Err(Error::new(
                ErrorKind::QuoteChanged,
                format!("quote changed: you confirmed {expected} but the fresh quote is {actual}; re-review and retry"),
            )
            .with_details(json!({"expected": expected, "actual": actual, "total_minor": q.total_after.minor()})));
        }
        order::Gate::NeedConfirmation => {
            return Err(Error::new(
                ErrorKind::ConfirmationRequired,
                "refusing to order without confirmation: pass --yes <quote-id> (non-TTY) or confirm interactively",
            ));
        }
        order::Gate::Proceed => {}
    }

    // Ordering must be explicitly enabled. Even here, the allowlist is the real
    // gate: if ordering is off the URL isn't registered and the send fails.
    if !ctx.config.allow_ordering {
        if !out.is_json() {
            out.line("ordering is DISABLED (set allow_ordering = true in config to enable).");
            out.line("would POST /order/orders:");
            out.line(
                serde_json::to_string_pretty(&order::redacted_body(&body)).unwrap_or_default(),
            );
        }
        return Err(Error::new(
            ErrorKind::OrderingDisabled,
            "ordering disabled: set allow_ordering = true in ~/.config/grabcli/config.toml to enable",
        )
        .with_details(json!({"would_send": order::redacted_body(&body)})));
    }

    // Refresh the session BEFORE submitting so a stale gfc_session can't cause a
    // mid-submit 401 (we never retry submit — no idempotency key). Best-effort.
    ctx.refresh_session().await.ok();

    // Submit. No idempotency key exists for this endpoint, so on ANY ambiguous
    // failure we do not retry — tell the caller to check ongoing orders.
    let req = Request::post(api::ORDERS, body, Hydra::Required).no_refresh();
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
    let order_id = order::parse_submit(&resp)?;
    Cart::clear()?;
    if out.is_json() {
        out.json(&json!({"ordered": true, "order_id": order_id, "quote_id": q.id}));
    } else {
        out.line(format!("order placed: {order_id}"));
    }
    Ok(())
}

fn prompt_confirm(q: &Quote) -> bool {
    use std::io::Write;
    print!(
        "Place this order for {} ? type the quote id ({}) to confirm: ",
        q.total_after.peso(),
        q.id
    );
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    line.trim() == q.id
}

async fn orders_cmd(out: Output, ongoing: bool, limit: u32) -> Result<()> {
    let mut ctx = context(out)?;
    let url = format!(
        "{}?input.limit={limit}&input.offset=0&input.onlyOngoingOrders={}",
        api::history_url(),
        ongoing
    );
    let resp = ctx.call(&Request::get(url, Hydra::Required)).await?;
    let orders = resp
        .pointer("/response/orders")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if out.is_json() {
        let items: Vec<Value> = orders.iter().map(order_summary_json).collect();
        out.json(&json!({"orders": items}));
    } else if orders.is_empty() {
        out.line("no orders");
    } else {
        for o in &orders {
            let id = o.get("orderID").and_then(Value::as_str).unwrap_or("?");
            let state = o.get("orderState").and_then(Value::as_str).unwrap_or("?");
            let short = o
                .get("shortOrderNumber")
                .and_then(Value::as_str)
                .unwrap_or("");
            let created = o.get("createdAt").and_then(Value::as_str).unwrap_or("");
            out.line(format!("{id}  {state}  {short}  {created}"));
        }
    }
    Ok(())
}

fn order_summary_json(o: &Value) -> Value {
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

async fn fetch_tracking(ctx: &mut Context, order_id: &str) -> Result<tracking::Tracking> {
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

async fn track_cmd(
    out: Output,
    order_id: &str,
    watch: bool,
    map: Option<&std::path::Path>,
) -> Result<()> {
    tracking::check_order_id(order_id)?;
    let mut ctx = context(out)?;
    let mut last_sig = String::new();
    loop {
        let model = match fetch_tracking(&mut ctx, order_id).await {
            Ok(m) => m,
            // In --watch, a 429 shouldn't abort the session: back off and retry.
            Err(e) if watch && e.kind == grabcli_core::error::ErrorKind::RateLimited => {
                if !out.is_json() {
                    out.line("rate limited; backing off 60s");
                }
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                continue;
            }
            Err(e) => return Err(e),
        };

        if let Some(path) = map {
            match map::render(&model, path).await {
                Ok(()) => {
                    if !out.is_json() {
                        out.line(format!("map written to {}", path.display()));
                    }
                }
                Err(e) => {
                    if !out.is_json() {
                        out.line(format!("map not rendered: {}", e.message));
                    }
                }
            }
        }

        let sig = model.watch_signature();
        let changed = sig != last_sig;
        last_sig = sig;

        if !watch || changed {
            if out.is_json() {
                out.json(&model.to_json());
            } else {
                out.line(model.human());
            }
        }

        if !watch || model.is_final {
            return Ok(());
        }
        // Poll politely: >=30s.
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

async fn history_sync(out: Output, vault: &std::path::Path, limit: u32) -> Result<()> {
    history::ensure_vault(vault)?;
    let mut ctx = context(out)?;

    // Paginate gently; only completed orders. Stop at `limit` or when the API
    // says there are no more.
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
        for o in &orders {
            if let Some(rec) = history::parse_order(o) {
                records.push(rec);
            }
        }
        let has_more = resp
            .pointer("/response/hasMore")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !has_more {
            break;
        }
        offset += page;
        // Be gentle with the history endpoint (429s when hammered).
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    }

    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
    let (created, updated, unchanged) = history::apply(vault, &records, &user)?;
    if out.is_json() {
        out.json(&json!({
            "synced": records.len(), "created": created,
            "updated": updated, "unchanged": unchanged,
            "vault": vault.display().to_string(),
        }));
    } else {
        out.line(format!(
            "synced {} orders: {created} created, {updated} updated, {unchanged} unchanged",
            records.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bpaf::Parser;

    fn parser() -> impl Parser<FoodCmd> {
        food_cmd()
    }

    #[test]
    fn parses_quote_with_flags() {
        let r = parser()
            .to_options()
            .run_inner(&["quote", "--voucher", "SAVE10", "--json"])
            .expect("parse");
        match r {
            FoodCmd::Quote { json, voucher, .. } => {
                assert!(json);
                assert_eq!(voucher.as_deref(), Some("SAVE10"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_cart_add_with_mods() {
        let r = parser()
            .to_options()
            .run_inner(&[
                "cart", "add", "2-EX", "ITEM-1", "--qty", "2", "--mod", "G1:M1",
            ])
            .expect("parse");
        match r {
            FoodCmd::Cart {
                cmd:
                    CartCmd::Add {
                        qty,
                        mods,
                        merchant_id,
                        item_id,
                        ..
                    },
            } => {
                assert_eq!(qty, 2);
                assert_eq!(mods, vec!["G1:M1".to_string()]);
                assert_eq!(merchant_id, "2-EX");
                assert_eq!(item_id, "ITEM-1");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn order_yes_parses() {
        let r = parser()
            .to_options()
            .run_inner(&["order", "--yes", "abc123abc123"])
            .expect("parse");
        match r {
            FoodCmd::Order { yes, dry_run, .. } => {
                assert_eq!(yes.as_deref(), Some("abc123abc123"));
                assert!(!dry_run);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_discovery_commands() {
        let p = parser().to_options();
        match p.run_inner(&[
            "search",
            "sisig",
            "--dishes",
            "--local",
            "--max-price",
            "200",
            "--open",
            "--min-rating",
            "4.5",
            "--max-fee",
            "₱49",
            "--sort",
            "price",
            "--limit",
            "5",
            "--pages",
            "2",
            "--type",
            "food",
        ]) {
            Ok(FoodCmd::Search {
                keyword,
                dishes,
                local,
                max_price,
                opts,
                ..
            }) => {
                assert_eq!(keyword, "sisig");
                assert!(dishes && local && opts.open);
                assert_eq!(max_price, Some(Pesos(Money(20_000))));
                assert_eq!(opts.max_fee, Some(Pesos(Money(4_900))));
                assert_eq!(opts.min_rating, Some(4.5));
                assert_eq!(opts.sort, discover::Sort::Price);
                assert_eq!((opts.limit, opts.pages), (Some(5), 2));
                assert_eq!(opts.business_type, Some(discover::BizType::Food));
            }
            other => panic!("wrong parse: {other:?}"),
        }
        assert!(matches!(
            p.run_inner(&["nearby", "--free-delivery", "--max-eta", "30"]),
            Ok(FoodCmd::Nearby { opts, .. }) if opts.free_delivery && opts.max_eta == Some(30)
        ));
        assert!(matches!(
            p.run_inner(&["category", "japanese", "--json"]),
            Ok(FoodCmd::Category { json: true, category, .. }) if category == "japanese"
        ));
        assert!(matches!(
            p.run_inner(&["recommended", "--promo"]),
            Ok(FoodCmd::Recommended { opts, .. }) if opts.promo
        ));
        assert!(matches!(
            p.run_inner(&["cuisines"]),
            Ok(FoodCmd::Cuisines { .. })
        ));
        assert!(matches!(
            p.run_inner(&["menu", "2-EX", "--images"]),
            Ok(FoodCmd::Menu { images: true, .. })
        ));
        assert!(p.run_inner(&["search", "x", "--sort", "vibes"]).is_err());
        assert!(p.run_inner(&["search", "x", "--max-fee", "free"]).is_err());
    }

    #[test]
    fn cmd_wants_json_detects_leaf_flag() {
        let r = parser()
            .to_options()
            .run_inner(&["search", "--json", "pizza"])
            .expect("parse");
        assert!(cmd_wants_json(&r));
    }
}
