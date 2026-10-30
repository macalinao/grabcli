//! `grab food …`: the GrabFood subcommands over the `grabfood` library.
//! Parsing, printing and the cart / last-quote files live here.

mod map;
mod print;

use std::path::PathBuf;

use bpaf::Bpaf;
use grabcli_core::Context;
use grabcli_core::cache::CacheMode;
use grabcli_core::error::{Error, ErrorKind, Result};
use grabcli_core::money::Money;
use grabfood::discover::{ListOpts, Pesos, list_opts};
use grabfood::{Cart, MenuSource, Quote, api, browse, order};
use serde_json::{Value, json};

use crate::output::Output;
use crate::{local, paths};

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
            let mut ctx = browse_context(cache).await?;
            let max_price = max_price.map(|p| p.0);
            let listing =
                browse::search(&mut ctx, &keyword, &opts, dishes, local, max_price).await?;
            if dishes {
                let ds = opts.dishes(&listing.merchants, max_price);
                print::dishes(out, &listing, &ds);
            } else {
                let ms =
                    browse::filter_dish_price(opts.apply(listing.merchants.clone()), max_price);
                print::merchants(out, &listing, &ms, true);
            }
            Ok(())
        }
        FoodCmd::Nearby { local, opts, .. } => {
            let listing = browse::nearby(&mut browse_context(cache).await?, &opts, local).await?;
            print::merchants(out, &listing, &opts.apply(listing.merchants.clone()), false);
            Ok(())
        }
        FoodCmd::Recommended { opts, .. } => {
            let listing = browse::recommended(&mut browse_context(cache).await?, &opts).await?;
            print::merchants(out, &listing, &opts.apply(listing.merchants.clone()), false);
            Ok(())
        }
        FoodCmd::Cuisines { .. } => {
            let c = browse::cuisines(&mut browse_context(cache).await?).await?;
            print::cuisines(out, &c);
            Ok(())
        }
        FoodCmd::Category {
            local,
            opts,
            category,
            ..
        } => {
            let mut ctx = browse_context(cache).await?;
            let listing = browse::category(&mut ctx, &category, &opts, local).await?;
            print::merchants(out, &listing, &opts.apply(listing.merchants.clone()), false);
            Ok(())
        }
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

// A context whose allowlist reflects the current ordering setting. Its cache is
// off: cart/quote/voucher/order/tracking/history commands always talk to Grab
// live. Browse commands use [`browse_context`] instead.
async fn context() -> Result<Context> {
    local::context(|c| api::allowlist(c.allow_ordering), CacheMode::Off).await
}

// A context for read-only browse commands (search, menu, cart add), honouring
// the global cache mode.
async fn browse_context(cache: CacheMode) -> Result<Context> {
    local::context(|c| api::allowlist(c.allow_ordering), cache).await
}

// ------------------------------------------------------------ cart files

fn food_dir() -> Result<PathBuf> {
    Ok(paths::state_dir()?.join("food"))
}

/// `<state>/food/cart.json`.
fn load_cart() -> Result<Cart> {
    match std::fs::read(food_dir()?.join("cart.json")) {
        Ok(b) => Ok(serde_json::from_slice(&b)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Cart::default()),
        Err(e) => Err(e.into()),
    }
}

fn save_cart(cart: &Cart) -> Result<()> {
    paths::write_atomic(
        &food_dir()?.join("cart.json"),
        &serde_json::to_vec_pretty(cart)?,
    )
}

fn clear_cart() -> Result<()> {
    match std::fs::remove_file(food_dir()?.join("cart.json")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn save_last_quote(q: &Quote) -> Result<()> {
    let v = json!({"quote_id": q.id, "signature": q.quote_signature, "total_minor": q.total_after.minor()});
    paths::write_atomic(
        &food_dir()?.join("last-quote.json"),
        &serde_json::to_vec_pretty(&v)?,
    )
}

// ------------------------------------------------------------ commands

async fn menu_cmd(out: Output, cache: CacheMode, merchant_id: &str, images: bool) -> Result<()> {
    let mut ctx = browse_context(cache).await?;
    let fetched = grabfood::menu_meta(&mut ctx, merchant_id, MenuSource::Cached).await?;
    let menu = &fetched.value;
    let image_errors = if images {
        grabfood::download_menu_images(&ctx, menu).await
    } else {
        0
    };

    if out.is_json() {
        let mut j = print::menu_json(menu, ctx.cache()).await;
        j["cached_at"] = json!(fetched.cached_at);
        j["age_secs"] = json!(fetched.age_secs());
        if images {
            j["image_errors"] = json!(image_errors);
        }
        out.json(&j);
        return Ok(());
    }
    print::menu_human(
        out,
        menu,
        fetched.age_secs(),
        images.then_some(image_errors),
    );
    Ok(())
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
            let mut ctx = browse_context(cache).await?;
            let mut cart = load_cart()?;
            let added = grabfood::cart_add(
                &mut ctx,
                &mut cart,
                &merchant_id,
                &item_id,
                qty,
                &mods,
                note,
            )
            .await?;
            save_cart(&cart)?;
            if out.is_json() {
                out.json(&json!({"added": item_id, "quantity": qty, "merchant": merchant_id}));
            } else {
                out.line(format!("added {qty}x {} ({item_id})", added.item_name));
            }
            Ok(())
        }
        CartCmd::Rm { item_id, .. } => {
            let mut cart = load_cart()?;
            grabfood::cart_remove(&mut cart, &item_id)?;
            if cart.is_empty() {
                clear_cart()?;
            } else {
                save_cart(&cart)?;
            }
            if out.is_json() {
                out.json(&json!({"removed": item_id}));
            } else {
                out.line(format!("removed {item_id}"));
            }
            Ok(())
        }
        CartCmd::Show { .. } => {
            let cart = load_cart()?;
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
            clear_cart()?;
            if out.is_json() {
                out.json(&json!({"cleared": true}));
            } else {
                out.line("cart cleared");
            }
            Ok(())
        }
    }
}

async fn quote_cmd(out: Output, voucher: Option<&str>, payment: Option<&str>) -> Result<()> {
    let mut ctx = context().await?;
    let q = grabfood::quote(&mut ctx, &load_cart()?, voucher, payment).await?;
    save_last_quote(&q)?;
    if out.is_json() {
        out.json(&grabfood::quote_json(&q));
    } else {
        print::quote_human(out, &q);
    }
    Ok(())
}

async fn vouchers_cmd(out: Output, best: bool, apply: bool) -> Result<()> {
    let mut ctx = context().await?;
    let cart = load_cart()?;
    if !best {
        let list = grabfood::vouchers(&mut ctx, &cart).await?;
        if out.is_json() {
            out.json(&json!({
                "base_total_minor": list.base.total_after.minor(),
                "candidates": list.candidates,
            }));
        } else {
            out.line(format!("base total {}", list.base.total_after.peso()));
            if list.candidates.is_empty() {
                out.line("no eligible voucher codes");
            }
            for c in &list.candidates {
                out.line(format!("  {c}"));
            }
        }
        return Ok(());
    }

    let b = grabfood::best_voucher(&mut ctx, &cart).await?;
    if apply {
        let mut cart = load_cart()?;
        cart.voucher = b.best_code.clone();
        save_cart(&cart)?;
    }
    let total_of = |t: Option<Money>| t.unwrap_or(Money(i64::MAX));
    if out.is_json() {
        out.json(&json!({
            "base_total_minor": b.base_total.minor(),
            "best_code": b.best_code,
            "best_total_minor": b.best_total.minor(),
            "applied": apply,
            "tried": b.tried.iter().map(|t| json!({"code": t.code, "total_minor": total_of(t.total).minor()})).collect::<Vec<_>>(),
        }));
    } else {
        out.line(format!("base total {}", b.base_total.peso()));
        for t in &b.tried {
            let shown = t.total.map_or_else(|| "error".to_string(), |m| m.peso());
            out.line(format!("  {}: {shown}", t.code));
        }
        match &b.best_code {
            Some(c) => out.line(format!(
                "best: {c} -> {}{}",
                b.best_total.peso(),
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
    let mut ctx = context().await?;
    let cart = load_cart()?;
    let prepared = grabfood::prepare_order(&mut ctx, &cart, notes).await?;
    let q = &prepared.quote;
    save_last_quote(q)?;

    // Always show the full quote first.
    if out.is_json() {
        let mut j = grabfood::quote_json(q);
        j["would_send"] = prepared.redacted_body();
        out.json(&j);
    } else {
        print::quote_human(out, q);
    }

    if dry_run {
        if !out.is_json() {
            out.line("dry run — would POST /order/orders:");
            out.line(serde_json::to_string_pretty(&prepared.redacted_body()).unwrap_or_default());
        }
        return Ok(());
    }

    // Confirmation gate.
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let tty_confirmed = if yes.is_none() && is_tty && !out.is_json() {
        Some(prompt_confirm(q))
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

    // Ordering must be explicitly enabled (submit_order checks again).
    if let Err(e) = grabfood::ensure_ordering_enabled(&ctx, &prepared) {
        if !out.is_json() {
            out.line("ordering is DISABLED (set allow_ordering = true in config to enable).");
            out.line("would POST /order/orders:");
            out.line(serde_json::to_string_pretty(&prepared.redacted_body()).unwrap_or_default());
        }
        return Err(e);
    }

    // The gate matched the confirmation to this fresh quote.
    let quote_id = q.id.clone();
    let order_id = grabfood::submit_order(&mut ctx, &prepared, &quote_id).await?;
    clear_cart()?;
    if out.is_json() {
        out.json(&json!({"ordered": true, "order_id": order_id, "quote_id": quote_id}));
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
    let mut ctx = context().await?;
    let orders = grabfood::orders(&mut ctx, ongoing, limit).await?;
    if out.is_json() {
        let items: Vec<Value> = orders.iter().map(grabfood::OrderSummary::to_json).collect();
        out.json(&json!({"orders": items}));
    } else if orders.is_empty() {
        out.line("no orders");
    } else {
        for o in &orders {
            out.line(format!(
                "{}  {}  {}  {}",
                o.order_id, o.state, o.short_number, o.created_at
            ));
        }
    }
    Ok(())
}

async fn track_cmd(
    out: Output,
    order_id: &str,
    watch: bool,
    map: Option<&std::path::Path>,
) -> Result<()> {
    grabfood::tracking::check_order_id(order_id)?;
    let mut ctx = context().await?;
    let mut last_sig = String::new();
    loop {
        let model = match grabfood::track(&mut ctx, order_id).await {
            Ok(m) => m,
            // In --watch, a 429 shouldn't abort the session: back off and retry.
            Err(e) if watch && e.kind == ErrorKind::RateLimited => {
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
    grabfood::history::ensure_vault(vault)?;
    let mut ctx = context().await?;
    let records = grabfood::history(&mut ctx, limit).await?;
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
    let (created, updated, unchanged) = grabfood::history::apply(vault, &records, &user)?;
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
    use grabfood::discover;

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
