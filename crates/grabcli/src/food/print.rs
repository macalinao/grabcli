//! Human and `--json` output for `grab food`.

use grabcli_core::cache::{CacheHandle, human_age};
use grabcli_core::jwt;
use grabcli_core::money::Money;
use grabfood::browse::{CUISINES, Cuisines, Listing, footer, meta_json};
use grabfood::discover::{Dish, Merchant};
use grabfood::{Menu, Quote};
use serde_json::{Value, json};

use crate::output::Output;

pub fn merchants(out: Output, l: &Listing, ms: &[Merchant], show_dishes: bool) {
    if out.is_json() {
        let mut j = meta_json(l, ms.len());
        j["merchants"] = ms.iter().map(Merchant::to_json).collect();
        out.json(&j);
        return;
    }
    if ms.is_empty() {
        out.line("no merchants found");
    }
    for m in ms {
        out.line(m.human_line());
        if show_dishes {
            for d in m.dishes.iter().take(3) {
                out.line(format!("    {}", d.human_line()));
            }
            if m.dishes.len() > 3 {
                out.line(format!("    … {} more dishes", m.dishes.len() - 3));
            }
        }
    }
    out.line(footer(l, ms.len(), "merchants"));
}

pub fn dishes(out: Output, l: &Listing, dishes: &[(&Dish, &Merchant)]) {
    if out.is_json() {
        let mut j = meta_json(l, dishes.len());
        j["dishes"] = dishes
            .iter()
            .map(|(d, m)| {
                let mut v = d.to_json();
                v["merchant"] = json!({
                    "id": m.id, "name": m.name, "rating": m.rating, "eta_min": m.eta_min,
                    "distance_km": m.distance_km, "open": m.open,
                    "delivery_fee_minor": m.fee.map(Money::minor),
                    "free_delivery": m.free_delivery,
                });
                v
            })
            .collect();
        out.json(&j);
        return;
    }
    if dishes.is_empty() {
        out.line("no dishes found");
    }
    for (d, m) in dishes {
        let mut extra = Vec::new();
        if let Some(e) = m.eta_min {
            extra.push(format!("{e} min"));
        }
        if let Some(k) = m.distance_km {
            extra.push(format!("{k:.1} km"));
        }
        if let Some(r) = m.rating {
            extra.push(format!("★{r:.1}"));
        }
        out.line(format!(
            "{}  — {} ({})  {}",
            d.human_line(),
            m.name,
            m.id,
            extra.join("  ")
        ));
    }
    out.line(footer(l, dishes.len(), "dishes"));
}

pub fn cuisines(out: Output, c: &Cuisines) {
    if out.is_json() {
        out.json(&json!({
            "cuisines": CUISINES.iter().map(|(id, name)| json!({"id": id, "name": name})).collect::<Vec<_>>(),
            "collections": c.collections.iter().map(|s| json!({
                "id": s.id, "name": s.name, "description": s.description,
            })).collect::<Vec<_>>(),
            "cached_at": c.cached_at,
            "age_secs": c.cached_at.map(|t| (jwt::now() - t).max(0)),
            "collections_error": c.error.as_ref().map(|e| e.to_json()["error"].clone()),
        }));
        return;
    }
    out.line("cuisines:");
    for (id, name) in CUISINES {
        out.line(format!("  {id:>6}  {name}"));
    }
    out.line("collections near you:");
    for s in &c.collections {
        if s.description.is_empty() {
            out.line(format!("  {:>6}  {}", s.id, s.name));
        } else {
            out.line(format!("  {:>6}  {} — {}", s.id, s.name, s.description));
        }
    }
    if let Some(e) = &c.error {
        out.line(format!("  (collections unavailable: {})", e.message));
    }
    if let Some(t) = c.cached_at {
        out.line(format!("(cached {} ago)", human_age(jwt::now() - t)));
    }
    out.line("use `grab food category <id|name>` to list one");
}

/// `image_errors` is `Some` when `--images` was given.
pub fn menu_human(out: Output, menu: &Menu, age_secs: Option<i64>, image_errors: Option<usize>) {
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
    if let Some(age) = age_secs {
        out.line(format!("(cached {} ago)", human_age(age)));
    }
    if let Some(image_errors) = image_errors {
        let n = menu
            .items
            .iter()
            .take(grabfood::MAX_MENU_IMAGES)
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
}

pub async fn menu_json(menu: &Menu, cache: &CacheHandle) -> Value {
    let mut items = Vec::new();
    for it in &menu.items {
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
        let image_path = match it.image_url.as_deref() {
            Some(u) => cache
                .cached_image(u)
                .await
                .and_then(|i| i.path)
                .map(|p| p.display().to_string()),
            None => None,
        };
        items.push(json!({
            "id": it.id, "name": it.name, "category": it.category,
            "price_minor": it.price.minor(), "available": it.available,
            "description": it.description, "image_url": it.image_url,
            "image_path": image_path,
            "modifier_groups": groups,
        }));
    }
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

pub fn quote_human(out: Output, q: &Quote) {
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
