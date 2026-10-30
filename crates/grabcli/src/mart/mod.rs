//! `grab mart …`: the GrabMart subcommands over the `grabmart` library (see
//! docs/mart.md). Parsing, printing, the cart / last-quote files and the
//! cross-process call pacing live here.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bpaf::Bpaf;
use grabcli_core::Allowlist;
use grabcli_core::cache::{CacheKind, CacheMode};
use grabcli_core::error::{Error, ErrorKind, Result};
use grabmart::cart::{Cart, CartItem};
use grabmart::client::{MIN_GAP, Mart, Pacer};
use grabmart::model::{Categories, ItemDetail, ProductPage, StoreList, StorePage};
use grabmart::quote::{self, Quote};
use grabmart::recaptcha::action;
use grabmart::{api, order};
use serde_json::{Value, json};

use crate::output::Output;
use crate::{local, paths};

fn mart_dir() -> Result<PathBuf> {
    Ok(paths::state_dir()?.join("mart"))
}

/// Paces mart calls across `grab` processes through `<state>/mart/last-call`.
struct FilePacer;

#[async_trait]
impl Pacer for FilePacer {
    /// Sleep so consecutive mart calls (in this or other `grab` processes) are
    /// at least [`MIN_GAP`] apart, then record this call. Best-effort.
    async fn wait(&self) {
        let now_ms = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
        };
        if let Ok(p) = mart_dir().map(|d| d.join("last-call")) {
            let last: i64 = std::fs::read_to_string(&p)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            let wait = MIN_GAP.as_millis() as i64 - (now_ms() - last);
            if wait > 0 {
                tokio::time::sleep(Duration::from_millis(wait as u64)).await;
            }
            let _ = paths::write_atomic(&p, now_ms().to_string().as_bytes());
        }
    }
}

/// A mart client over the CLI's session and cache. `cache` applies to browse
/// calls; the quote path passes [`CacheMode::Off`].
async fn mart(out: Output, cache: CacheMode) -> Result<Mart> {
    let ctx = local::context(|_| Allowlist::new(), cache).await?;
    let mut m = Mart::new(ctx)?.with_pacer(Arc::new(FilePacer));
    m.notices = !out.is_json();
    Ok(m)
}

/// `<state>/mart/cart.json`.
fn load_cart() -> Result<Cart> {
    match std::fs::read(mart_dir()?.join("cart.json")) {
        Ok(b) => Ok(serde_json::from_slice(&b)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Cart::default()),
        Err(e) => Err(e.into()),
    }
}

fn save_cart(cart: &Cart) -> Result<()> {
    paths::write_atomic(
        &mart_dir()?.join("cart.json"),
        &serde_json::to_vec_pretty(cart)?,
    )
}

fn clear_cart() -> Result<()> {
    match std::fs::remove_file(mart_dir()?.join("cart.json")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Page size for list calls.
const PAGE: u32 = 20;
/// Upper bound on pages fetched by `--all` (rate limits).
const MAX_PAGES: u32 = 10;

/// `grab mart <cmd>`.
#[derive(Debug, Clone, Bpaf)]
pub enum MartCmd {
    /// List stores near the saved address (home feed, a category, or a keyword).
    #[bpaf(command("stores"))]
    Stores {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Store category id from `grab mart categories` (e.g. 23396 Grocery).
        #[bpaf(long("category"), argument("ID"))]
        category: Option<String>,
        /// Only stores matching a keyword (name or products).
        #[bpaf(long("keyword"), argument("TEXT"))]
        keyword: Option<String>,
        /// Paging offset (from a previous `next offset`).
        #[bpaf(long("offset"), argument("N"), fallback(0))]
        offset: u32,
    },

    /// Store categories and cross-store departments.
    #[bpaf(command("categories"))]
    Categories {
        #[bpaf(long("json"), switch)]
        json: bool,
    },

    /// Search stores and matching products across stores.
    #[bpaf(command("search"))]
    Search {
        #[bpaf(long("json"), switch)]
        json: bool,
        #[bpaf(long("offset"), argument("N"), fallback(0))]
        offset: u32,
        #[bpaf(positional("KEYWORD"))]
        keyword: String,
    },

    /// A store: hours, fees, campaigns, departments and their first items.
    #[bpaf(command("store"))]
    Store {
        #[bpaf(long("json"), switch)]
        json: bool,
        #[bpaf(positional("STORE_ID"))]
        store: String,
    },

    /// List a department's items (paginated).
    #[bpaf(command("items"))]
    Items {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Department id from `grab mart store`.
        #[bpaf(long("department"), argument("ID"))]
        department: String,
        /// Sub-department id (listed with the first page).
        #[bpaf(long("sub"), argument("ID"))]
        sub: Option<String>,
        #[bpaf(long("offset"), argument("N"), fallback(0))]
        offset: u32,
        /// Fetch every page (up to 10, paced).
        #[bpaf(long("all"), switch)]
        all: bool,
        #[bpaf(positional("STORE_ID"))]
        store: String,
    },

    /// Search products within one store.
    #[bpaf(command("find"))]
    Find {
        #[bpaf(long("json"), switch)]
        json: bool,
        #[bpaf(long("offset"), argument("N"), fallback(0))]
        offset: u32,
        /// Fetch every page (up to 10, paced).
        #[bpaf(long("all"), switch)]
        all: bool,
        #[bpaf(positional("STORE_ID"))]
        store: String,
        #[bpaf(positional("KEYWORD"))]
        keyword: String,
    },

    /// One product: price, size, availability and attributes.
    #[bpaf(command("item"))]
    Item {
        #[bpaf(long("json"), switch)]
        json: bool,
        #[bpaf(positional("STORE_ID"))]
        store: String,
        #[bpaf(positional("ITEM_ID"))]
        item: String,
    },

    /// Manage the local mart cart (one store at a time; separate from food's).
    #[bpaf(command("cart"))]
    Cart {
        #[bpaf(external(mart_cart_cmd))]
        cmd: MartCartCmd,
    },

    /// Price the cart: subtotal, delivery, fees, discounts, total (no order).
    #[bpaf(command("quote"))]
    Quote {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Payment method id (overrides config/session default).
        #[bpaf(long("payment"), argument("ID"))]
        payment: Option<String>,
        /// Print the (redacted) request body and stop without calling Grab.
        #[bpaf(long("dry-run"), switch)]
        dry_run: bool,
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
        /// Payment method id (overrides config/session default).
        #[bpaf(long("payment"), argument("ID"))]
        payment: Option<String>,
    },
}

/// `grab mart cart <cmd>`.
#[derive(Debug, Clone, Bpaf)]
pub enum MartCartCmd {
    /// Add a product (validated against the live catalogue).
    #[bpaf(command("add"))]
    Add {
        #[bpaf(long("json"), switch)]
        json: bool,
        #[bpaf(long("qty"), argument("N"), fallback(1))]
        qty: i64,
        /// Note for the shopper.
        #[bpaf(long("note"), argument("TEXT"))]
        note: Option<String>,
        #[bpaf(positional("STORE_ID"))]
        store: String,
        #[bpaf(positional("ITEM_ID"))]
        item: String,
    },
    /// Remove a product by id.
    #[bpaf(command("remove"))]
    Remove {
        #[bpaf(long("json"), switch)]
        json: bool,
        #[bpaf(positional("ITEM_ID"))]
        item: String,
    },
    /// Show the cart.
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

/// True if the leaf command carried `--json`.
pub fn cmd_wants_json(cmd: &MartCmd) -> bool {
    match cmd {
        MartCmd::Stores { json, .. }
        | MartCmd::Categories { json }
        | MartCmd::Search { json, .. }
        | MartCmd::Store { json, .. }
        | MartCmd::Items { json, .. }
        | MartCmd::Find { json, .. }
        | MartCmd::Item { json, .. }
        | MartCmd::Quote { json, .. }
        | MartCmd::Order { json, .. } => *json,
        MartCmd::Cart { cmd } => match cmd {
            MartCartCmd::Add { json, .. }
            | MartCartCmd::Remove { json, .. }
            | MartCartCmd::Show { json }
            | MartCartCmd::Clear { json } => *json,
        },
    }
}

/// Run a GrabMart command. `cache` (from `--no-cache` / `--refresh`) only
/// affects browse commands; cart/quote/order always go live.
pub async fn run(out: Output, cache: CacheMode, cmd: MartCmd) -> Result<()> {
    match cmd {
        MartCmd::Stores {
            category,
            keyword,
            offset,
            ..
        } => stores(out, cache, category.as_deref(), keyword.as_deref(), offset).await,
        MartCmd::Categories { .. } => categories(out, cache).await,
        MartCmd::Search {
            keyword, offset, ..
        } => search(out, cache, &keyword, offset).await,
        MartCmd::Store { store, .. } => store_cmd(out, cache, &store).await,
        MartCmd::Items {
            department,
            sub,
            offset,
            all,
            store,
            ..
        } => items(out, cache, &store, &department, sub.as_deref(), offset, all).await,
        MartCmd::Find {
            offset,
            all,
            store,
            keyword,
            ..
        } => find(out, cache, &store, &keyword, offset, all).await,
        MartCmd::Item { store, item, .. } => item_cmd(out, cache, &store, &item).await,
        MartCmd::Cart { cmd } => cart_run(out, cache, cmd).await,
        MartCmd::Quote {
            payment, dry_run, ..
        } => quote_cmd(out, payment.as_deref(), dry_run).await,
        MartCmd::Order {
            yes,
            dry_run,
            notes,
            payment,
            ..
        } => {
            order_cmd(
                out,
                yes.as_deref(),
                dry_run,
                notes.as_deref(),
                payment.as_deref(),
            )
            .await
        }
    }
}

fn check_keyword(k: &str) -> Result<String> {
    let k = k.trim();
    if k.chars().count() < 3 {
        return Err(Error::invalid(
            "GrabMart search needs at least 3 characters",
        ));
    }
    Ok(k.to_string())
}

fn print_stores(out: Output, list: &StoreList, with_products: bool) {
    if out.is_json() {
        out.json(&list.to_json());
        return;
    }
    if let Some(t) = &list.title {
        out.line(t);
    }
    if list.stores.is_empty() {
        out.line("no stores found");
    }
    for s in &list.stores {
        out.line(s.line());
        if with_products {
            for p in &s.products {
                out.line(format!("    {}", p.line()));
            }
        }
    }
    if let Some(n) = list.next_offset {
        out.line(format!("(more: --offset {n})"));
    }
}

async fn stores(
    out: Output,
    cache: CacheMode,
    category: Option<&str>,
    keyword: Option<&str>,
    offset: u32,
) -> Result<()> {
    let mut m = mart(out, cache).await?;
    let (lat, lng) = (m.loc.latitude, m.loc.longitude);
    let list = if let Some(k) = keyword {
        let k = check_keyword(k)?;
        let key = m
            .key(CacheKind::List, "search")
            .param("keyword", k.to_lowercase())
            .param("offset", offset);
        let v = m
            .public(
                &key,
                &api::search_url(&k, lat, lng, offset, PAGE),
                action::SEARCH_MERCHANT,
            )
            .await?;
        StoreList::parse_search(&v)
    } else if let Some(c) = category {
        let key = m
            .key(CacheKind::List, "category")
            .param("category", c)
            .param("offset", offset);
        let v = m
            .public(
                &key,
                &api::category_url(c, lat, lng, offset, PAGE),
                action::CATEGORY,
            )
            .await?;
        StoreList::parse_search(&v)
    } else {
        let key = m.key(CacheKind::List, "homefeed").param("offset", offset);
        let v = m
            .public(
                &key,
                &api::homefeed_url(lat, lng, offset, PAGE),
                action::FEED,
            )
            .await?;
        StoreList::parse_homefeed(&v)
    };
    print_stores(out, &list, false);
    Ok(())
}

async fn categories(out: Output, cache: CacheMode) -> Result<()> {
    let mut m = mart(out, cache).await?;
    let (lat, lng) = (m.loc.latitude, m.loc.longitude);
    // Same request as `stores` (first page), cached as categories (24 h).
    let key = m.key(CacheKind::Categories, "homefeed");
    let v = m
        .public(&key, &api::homefeed_url(lat, lng, 0, PAGE), action::FEED)
        .await?;
    let c = Categories::parse_homefeed(&v);
    if out.is_json() {
        out.json(&json!({
            "store_categories": c.store_categories,
            "departments": c.departments,
        }));
        return Ok(());
    }
    out.line("store categories (grab mart stores --category ID):");
    for x in &c.store_categories {
        out.line(format!("  {:<8} {}", x.id, x.name));
    }
    out.line("departments (across stores):");
    for x in &c.departments {
        out.line(format!("  {}  {}", x.id, x.name));
    }
    Ok(())
}

async fn search(out: Output, cache: CacheMode, keyword: &str, offset: u32) -> Result<()> {
    let k = check_keyword(keyword)?;
    let mut m = mart(out, cache).await?;
    let (lat, lng) = (m.loc.latitude, m.loc.longitude);
    let key = m
        .key(CacheKind::List, "search")
        .param("keyword", k.to_lowercase())
        .param("offset", offset);
    let v = m
        .public(
            &key,
            &api::search_url(&k, lat, lng, offset, PAGE),
            action::SEARCH_MERCHANT,
        )
        .await?;
    print_stores(out, &StoreList::parse_search(&v), true);
    Ok(())
}

async fn fetch_store(m: &mut Mart, store: &str, live: bool) -> Result<StorePage> {
    let url = api::merchant_url(store, m.loc.latitude, m.loc.longitude);
    let v = if live {
        m.fetch_public(&url, action::MERCHANT).await?
    } else {
        let key = m.key(CacheKind::Menu, "merchant").param("id", store);
        m.public(&key, &url, action::MERCHANT).await?
    };
    StorePage::parse(&v).ok_or_else(|| Error::not_found(format!("no GrabMart store {store}")))
}

async fn store_cmd(out: Output, cache: CacheMode, store: &str) -> Result<()> {
    let mut m = mart(out, cache).await?;
    let p = fetch_store(&mut m, store, false).await?;
    if out.is_json() {
        out.json(&p.to_json());
        return Ok(());
    }
    let open = match p.open {
        Some(true) => "OPEN",
        Some(false) => "CLOSED",
        None => "?",
    };
    out.line(format!(
        "{}  {}  {}  hours {}  eta {}  {}",
        p.id,
        p.name,
        open,
        p.hours.as_deref().unwrap_or("?"),
        p.eta_min.map(|e| format!("{e} min")).unwrap_or("?".into()),
        p.delivery_fee
            .map(|f| format!("delivery {}", f.peso()))
            .unwrap_or_default(),
    ));
    if !p.campaigns.is_empty() {
        out.line("offers:");
        for (_, name) in &p.campaigns {
            out.line(format!("  {name}"));
        }
    }
    out.line("departments (grab mart items STORE --department ID):");
    for d in &p.departments {
        let more = if d.has_more { ", more" } else { "" };
        out.line(format!(
            "  {}  {}  ({} shown{more})",
            d.id,
            d.name,
            d.items.len()
        ));
        for it in &d.items {
            out.line(format!("      {}", it.line()));
        }
    }
    Ok(())
}

/// Print a product page (or several, concatenated).
fn print_products(out: Output, page: &ProductPage) {
    if out.is_json() {
        out.json(&page.to_json());
        return;
    }
    if let Some(t) = page.total {
        out.line(format!("{t} matching items"));
    }
    if page.items.is_empty() {
        out.line("no items");
    }
    for it in &page.items {
        out.line(it.line());
    }
    if !page.sub_departments.is_empty() {
        out.line("sub-departments (--sub ID):");
        for d in &page.sub_departments {
            out.line(format!("  {}  {}", d.id, d.name));
        }
    }
    if let Some(n) = page.next_offset {
        out.line(format!("(more: --offset {n}, or --all)"));
    }
}

/// Fetch one page, or all pages up to [`MAX_PAGES`] when `all`.
async fn paginate<F>(m: &mut Mart, offset: u32, all: bool, mut fetch: F) -> Result<ProductPage>
where
    F: AsyncFnMut(&mut Mart, u32) -> Result<Value>,
{
    let mut acc = ProductPage::parse(&fetch(m, offset).await?);
    let mut pages = 1;
    while all && pages < MAX_PAGES {
        let Some(next) = acc.next_offset else { break };
        let more = ProductPage::parse(&fetch(m, next as u32).await?);
        acc.items.extend(more.items);
        acc.next_offset = more.next_offset;
        pages += 1;
    }
    Ok(acc)
}

#[allow(clippy::too_many_arguments)]
async fn items(
    out: Output,
    cache: CacheMode,
    store: &str,
    department: &str,
    sub: Option<&str>,
    offset: u32,
    all: bool,
) -> Result<()> {
    let mut m = mart(out, cache).await?;
    // Resolve the department's full id and type from the (cached) store page.
    let page = fetch_store(&mut m, store, false).await?;
    let (dept_id, dept_type) = match page.department(department) {
        Some(d) => (d.id.clone(), d.department_type),
        None => (department.to_string(), 0),
    };
    let store = store.to_string();
    let sub = sub.map(ToString::to_string);
    let res = paginate(&mut m, offset, all, async |m: &mut Mart, off: u32| {
        let (lat, lng) = (m.loc.latitude, m.loc.longitude);
        let key = m
            .key(CacheKind::Menu, "items")
            .param("id", &store)
            .param("department", &dept_id)
            .param("sub", sub.as_deref().unwrap_or_default())
            .param("offset", off);
        let url = api::items_url(
            &store,
            &dept_id,
            dept_type,
            sub.as_deref(),
            lat,
            lng,
            off,
            PAGE,
        );
        m.public(&key, &url, action::MERCHANT_ITEMS).await
    })
    .await?;
    print_products(out, &res);
    Ok(())
}

async fn find(
    out: Output,
    cache: CacheMode,
    store: &str,
    keyword: &str,
    offset: u32,
    all: bool,
) -> Result<()> {
    let k = check_keyword(keyword)?;
    let mut m = mart(out, cache).await?;
    let store = store.to_string();
    let res = paginate(&mut m, offset, all, async |m: &mut Mart, off: u32| {
        let (lat, lng) = (m.loc.latitude, m.loc.longitude);
        let key = m
            .key(CacheKind::List, "instore_search")
            .param("id", &store)
            .param("keyword", k.to_lowercase())
            .param("offset", off);
        let url = api::instore_search_url(&store, &k, lat, lng, off, PAGE);
        m.public(&key, &url, action::SEARCH_IN_STORE).await
    })
    .await?;
    print_products(out, &res);
    Ok(())
}

async fn fetch_item(m: &mut Mart, store: &str, item: &str) -> Result<ItemDetail> {
    let url = api::item_url(item, m.loc.latitude, m.loc.longitude, &m.loc.id);
    let key = m.key(CacheKind::Menu, "item").param("id", item);
    let v = m.public(&key, &url, action::ITEM_DETAIL).await?;
    let d = ItemDetail::parse(&v).ok_or_else(|| Error::not_found(format!("no item {item}")))?;
    if !d.product.merchant_id.is_empty() && d.product.merchant_id != store {
        return Err(Error::new(
            ErrorKind::NotFound,
            format!(
                "item {item} belongs to store {} ({}), not {store}",
                d.product.merchant_id,
                d.merchant_name.as_deref().unwrap_or("?")
            ),
        ));
    }
    Ok(d)
}

async fn item_cmd(out: Output, cache: CacheMode, store: &str, item: &str) -> Result<()> {
    let mut m = mart(out, cache).await?;
    let d = fetch_item(&mut m, store, item).await?;
    if out.is_json() {
        out.json(&d.to_json());
        return Ok(());
    }
    let p = &d.product;
    out.line(format!("{}  {}", p.id, p.name));
    out.line(format!(
        "  store         {}",
        d.merchant_name.as_deref().unwrap_or(&p.merchant_id)
    ));
    out.line(format!("  price         {}", p.price_text()));
    if let Some(l) = &p.discount_label {
        out.line(format!("  offer         {l}"));
    }
    if let Some(s) = &p.size {
        out.line(format!("  size          {s}"));
    }
    out.line(format!("  availability  {}", p.availability()));
    if !d.attributes.is_empty() {
        out.line("  attributes:");
        for (t, c) in &d.attributes {
            out.line(format!("    {t}: {c}"));
        }
    }
    Ok(())
}

async fn cart_run(out: Output, cache: CacheMode, cmd: MartCartCmd) -> Result<()> {
    match cmd {
        MartCartCmd::Add {
            qty,
            note,
            store,
            item,
            ..
        } => {
            let mut m = mart(out, cache).await?;
            // A cached detail (≤ menu TTL) is fine for id validation: the quote
            // re-prices everything live.
            let d = fetch_item(&mut m, &store, &item).await?;
            if !d.product.orderable() {
                return Err(Error::new(
                    ErrorKind::CartInvalid,
                    format!("{} is {}", d.product.name, d.product.availability()),
                ));
            }
            let mut cart = load_cart()?;
            cart.add(
                &store,
                d.merchant_name.as_deref().unwrap_or_default(),
                CartItem {
                    item_id: item.clone(),
                    quantity: qty,
                    name: d.product.name.clone(),
                    unit_price_minor: d.product.effective_price().minor(),
                    campaign_id: d.product.campaign_id.clone(),
                    comment: note.unwrap_or_default(),
                },
            )?;
            save_cart(&cart)?;
            if out.is_json() {
                out.json(&json!({"added": item, "quantity": qty, "store": store,
                    "name": d.product.name, "unit_price_minor": d.product.effective_price().minor()}));
            } else {
                out.line(format!(
                    "added {qty}x {} ({item}) at {} each",
                    d.product.name,
                    d.product.effective_price().peso()
                ));
            }
            Ok(())
        }
        MartCartCmd::Remove { item, .. } => {
            let mut cart = load_cart()?;
            if !cart.remove(&item) {
                return Err(Error::not_found(format!("no mart cart item {item}")));
            }
            if cart.is_empty() {
                clear_cart()?;
            } else {
                save_cart(&cart)?;
            }
            if out.is_json() {
                out.json(&json!({"removed": item}));
            } else {
                out.line(format!("removed {item}"));
            }
            Ok(())
        }
        MartCartCmd::Show { .. } => {
            let cart = load_cart()?;
            if out.is_json() {
                let mut v = serde_json::to_value(&cart)?;
                v["snapshot_subtotal_minor"] = json!(cart.snapshot_subtotal().minor());
                out.json(&v);
            } else if cart.is_empty() {
                out.line("mart cart is empty");
            } else {
                out.line(format!("{}  {}", cart.merchant_id, cart.merchant_name));
                for i in &cart.items {
                    out.line(format!(
                        "  {}x {}  {}  {}",
                        i.quantity,
                        i.item_id,
                        grabcli_core::Money(i.unit_price_minor).peso(),
                        i.name
                    ));
                }
                out.line(format!(
                    "  ~subtotal {} (prices when added; run `grab mart quote`)",
                    cart.snapshot_subtotal().peso()
                ));
            }
            Ok(())
        }
        MartCartCmd::Clear { .. } => {
            clear_cart()?;
            if out.is_json() {
                out.json(&json!({"cleared": true}));
            } else {
                out.line("mart cart cleared");
            }
            Ok(())
        }
    }
}

/// Build the live quote body: fetch the store page live (campaigns, section,
/// open state) and combine with the cart, address and payment.
async fn quote_body(m: &mut Mart, cart: &Cart, payment: &str) -> Result<(Value, StorePage)> {
    cart.validate()?;
    let page = fetch_store(m, &cart.merchant_id, true).await?;
    let store_campaigns: Vec<String> = page.campaigns.iter().map(|(id, _)| id.clone()).collect();
    let ids = quote::campaign_ids(&store_campaigns, cart);
    let section = page.section.clone().unwrap_or_default();
    Ok((
        quote::build_body(cart, &section, &m.loc, payment, &ids),
        page,
    ))
}

async fn do_quote(m: &mut Mart, payment: Option<&str>) -> Result<Quote> {
    let cart = load_cart()?;
    let payment = payment
        .map(ToString::to_string)
        .unwrap_or_else(|| m.payment_default());
    cart.validate()?;
    // Fail fast (no store-page call) when the GrabMart login can't be set up.
    m.ensure_mart_session().await?;
    let (body, _) = quote_body(m, &cart, &payment).await?;
    let resp = m.quote_call(body).await?;
    Quote::parse(&resp, &cart, &m.loc, &payment)
}

fn save_last_quote(q: &Quote) -> Result<()> {
    // Only the id and total: the signature is not persisted.
    let p = mart_dir()?.join("last-quote.json");
    let v = json!({"quote_id": q.id, "total_minor": q.total_after.minor()});
    paths::write_atomic(&p, &serde_json::to_vec_pretty(&v)?)
}

async fn quote_cmd(out: Output, payment: Option<&str>, dry_run: bool) -> Result<()> {
    let mut m = mart(out, CacheMode::Off).await?;
    if dry_run {
        let cart = load_cart()?;
        let payment = payment
            .map(ToString::to_string)
            .unwrap_or_else(|| m.payment_default());
        let (body, page) = quote_body(&mut m, &cart, &payment).await?;
        let shown = quote::redacted(&body);
        if out.is_json() {
            out.json(
                &json!({"dry_run": true, "would_post": api::CART, "body": shown,
                "store_open": page.open, "logged_in": m.logged_in()}),
            );
        } else {
            out.line(format!("dry run — would POST {}:", api::CART));
            out.line(serde_json::to_string_pretty(&shown).unwrap_or_default());
            if page.open == Some(false) {
                out.line(format!("note: {} is currently closed", page.name));
            }
            if !m.logged_in() {
                out.line("note: the saved web login has expired; a real quote needs a re-login (`grab auth login`)");
            }
        }
        return Ok(());
    }
    let q = do_quote(&mut m, payment).await?;
    save_last_quote(&q)?;
    if out.is_json() {
        out.json(&q.to_json());
    } else {
        for l in q.human() {
            out.line(l);
        }
    }
    Ok(())
}

fn prompt_confirm(q: &Quote) -> bool {
    use std::io::Write;
    print!(
        "Place this GrabMart order for {} ? type the quote id ({}) to confirm: ",
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

async fn order_cmd(
    out: Output,
    yes: Option<&str>,
    dry_run: bool,
    notes: Option<&str>,
    payment: Option<&str>,
) -> Result<()> {
    let mut m = mart(out, CacheMode::Off).await?;
    let q = do_quote(&mut m, payment).await?;
    save_last_quote(&q)?;
    let body = order::build_submit_body(&q, &m.loc, notes);
    let shown = quote::redacted(&body);

    if out.is_json() {
        let mut j = q.to_json();
        j["would_send"] = shown.clone();
        out.json(&j);
    } else {
        for l in q.human() {
            out.line(l);
        }
    }
    if dry_run {
        if !out.is_json() {
            out.line(format!("dry run — would POST {}:", api::ORDERS));
            out.line(serde_json::to_string_pretty(&shown).unwrap_or_default());
        }
        return Ok(());
    }

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

    // The allowlist is the real gate: with ordering off the URL isn't
    // registered and a send would fail closed anyway.
    if !m.ctx.config.allow_ordering {
        return Err(Error::new(
            ErrorKind::OrderingDisabled,
            "ordering disabled: set allow_ordering = true in ~/.config/grabcli/config.toml to enable",
        )
        .with_details(json!({"would_send": shown})));
    }

    // Refresh first so a stale session can't 401 mid-submit (never retried).
    m.ctx.refresh_session().await.ok();
    let req =
        grabcli_core::http::Request::post(api::ORDERS, body, grabcli_core::http::Hydra::Required)
            .no_refresh();
    let mut req = req;
    req.extra_headers = vec![
        ("origin".into(), api::ORIGIN.into()),
        ("referer".into(), format!("{}/", api::ORIGIN)),
        ("x-country-code".into(), "PH".into()),
    ];
    // No idempotency key: on ANY failure, do not retry.
    let resp = match m.ctx.call(&req).await {
        Ok(v) => v,
        Err(e) => {
            return Err(Error::new(
                e.kind,
                format!(
                    "{} — DID NOT retry; check the Grab app for an ongoing GrabMart order before trying again",
                    e.message
                ),
            ));
        }
    };
    let order_id = order::parse_submit(&resp)?;
    clear_cart()?;
    if out.is_json() {
        out.json(&json!({"ordered": true, "order_id": order_id, "quote_id": q.id}));
    } else {
        out.line(format!("GrabMart order placed: {order_id}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bpaf::Parser;

    fn parse(args: &[&str]) -> MartCmd {
        mart_cmd()
            .to_options()
            .run_inner(args)
            .unwrap_or_else(|e| panic!("{args:?}: {e:?}"))
    }

    #[test]
    fn parses_browse_commands() {
        match parse(&["stores", "--category", "23396", "--json"]) {
            MartCmd::Stores { category, json, .. } => {
                assert_eq!(category.as_deref(), Some("23396"));
                assert!(json);
            }
            o => panic!("{o:?}"),
        }
        match parse(&["find", "2-SR", "red bull", "--all"]) {
            MartCmd::Find {
                store,
                keyword,
                all,
                ..
            } => {
                assert_eq!(
                    (store.as_str(), keyword.as_str(), all),
                    ("2-SR", "red bull", true)
                );
            }
            o => panic!("{o:?}"),
        }
        match parse(&["items", "2-SR", "--department", "D1", "--sub", "S1"]) {
            MartCmd::Items {
                department, sub, ..
            } => {
                assert_eq!(department, "D1");
                assert_eq!(sub.as_deref(), Some("S1"));
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn parses_cart_and_order() {
        let c = parse(&["cart", "add", "2-SR", "PHITE1", "--qty", "2"]);
        assert!(matches!(
            c,
            MartCmd::Cart {
                cmd: MartCartCmd::Add { qty: 2, .. }
            }
        ));
        match parse(&["order", "--yes", "abc123abc123"]) {
            MartCmd::Order { yes, dry_run, .. } => {
                assert_eq!(yes.as_deref(), Some("abc123abc123"));
                assert!(!dry_run);
            }
            o => panic!("{o:?}"),
        }
        assert!(cmd_wants_json(&parse(&["quote", "--dry-run", "--json"])));
    }

    #[test]
    fn short_keywords_rejected() {
        assert!(check_keyword("rb").is_err());
        assert_eq!(
            check_keyword(" red bull ").ok().as_deref(),
            Some("red bull")
        );
    }
}
