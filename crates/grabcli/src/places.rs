//! `grab saved-places …`: the account's saved places (Home, Work, other saved
//! addresses), and switching grabcli's delivery address to one of them.
//!
//! Listing is a read-only GET (docs/places.md). `use` only rewrites the
//! session's `location` cookie locally; nothing on Grab's side changes.

use bpaf::Bpaf;
use grabcli_core::error::Result;
use grabcli_core::location::Location;
use grabcli_core::places::{self, Match, SavedPlace};
use grabcli_core::{Context, Hydra, Request};
use grabcli_core::cache::CacheMode;
use serde_json::{Value, json};

use crate::local;
use crate::output::Output;

/// `grab saved-places <cmd>`.
#[derive(Debug, Clone, Bpaf)]
pub enum PlacesCmd {
    /// List saved places; `*` marks the current delivery address, `~` the
    /// closest place within 150 m of it.
    #[bpaf(command("list"))]
    List {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Only places in this country (e.g. PH).
        #[bpaf(long("country"), argument("CODE"))]
        country: Option<String>,
    },

    /// Deliver to a saved place: sets grabcli's saved delivery address
    /// (local session only; nothing changes on Grab).
    #[bpaf(command("use"))]
    Use {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Unit / floor / building line (addressDetail). Kept when the place
        /// is the current address (`*` or `~` in the list); otherwise empty
        /// unless given.
        #[bpaf(long("unit"), argument("TEXT"))]
        unit: Option<String>,
        /// Note to the driver. Same rule as --unit.
        #[bpaf(long("note"), argument("TEXT"))]
        note: Option<String>,
        /// List number, poiID, label (home, work, …) or name.
        #[bpaf(positional("PLACE"))]
        place: String,
    },
}

pub fn wants_json(cmd: &PlacesCmd) -> bool {
    match cmd {
        PlacesCmd::List { json, .. } | PlacesCmd::Use { json, .. } => *json,
    }
}

pub async fn run(out: Output, cmd: PlacesCmd) -> Result<()> {
    let mut ctx = local::context(|_| places::allowlist(), CacheMode::Off).await?;
    let all = fetch(&mut ctx).await?;
    let current = Location::from_session(ctx.session()).ok();
    match cmd {
        PlacesCmd::List { country, .. } => {
            list(out, &all, current.as_ref(), country.as_deref());
            Ok(())
        }
        PlacesCmd::Use {
            place, unit, note, ..
        } => {
            let p = places::find(&all, &place)?.clone();
            // Keep the unit/note only when staying at the same place (or the
            // closest one within 150 m: usually the same building).
            let idx = all.iter().position(|q| q.poi_id == p.poi_id);
            let same = current
                .as_ref()
                .filter(|c| idx.is_some() && places::current_place(&all, c).map(|(i, _)| i) == idx);
            let unit = unit.or_else(|| same.and_then(|c| c.address_detail.clone()));
            let note = note.or_else(|| same.and_then(|c| c.note_to_driver.clone()));
            let cookie = p.to_location(unit.as_deref(), note.as_deref());
            ctx.set_location(&cookie).await?;
            let loc = Location::from_session(ctx.session())?;
            if out.is_json() {
                out.json(&json!({
                    "place": place_json(&p, Some(Match::Current)),
                    "address": loc.address,
                    "address_detail": loc.address_detail,
                    "note_to_driver": loc.note_to_driver,
                }));
            } else {
                out.line(format!("Delivering to {}", title(&p)));
                out.line(format!("  {}", p.address));
                if let Some(u) = &loc.address_detail {
                    out.line(format!("  Unit: {u}"));
                }
                if let Some(n) = &loc.note_to_driver {
                    out.line(format!("  Note: {n}"));
                }
            }
            Ok(())
        }
    }
}

async fn fetch(ctx: &mut Context) -> Result<Vec<SavedPlace>> {
    let v = ctx
        .call(&Request::get(places::FAVORITES_URL, Hydra::Required))
        .await?;
    places::parse_places(&v)
}

fn title(p: &SavedPlace) -> String {
    match p.display_label() {
        Some(l) if !l.eq_ignore_ascii_case(&p.name) && !p.name.is_empty() => {
            format!("{l} · {}", p.name)
        }
        Some(l) => l,
        None => p.name.clone(),
    }
}

fn place_json(p: &SavedPlace, m: Option<Match>) -> Value {
    json!({
        "poi_id": p.poi_id,
        "label": p.display_label(),
        "label_raw": p.label,
        "name": p.name,
        "address": p.address,
        "latitude": p.location.latitude,
        "longitude": p.location.longitude,
        "city": p.city,
        "city_id": p.city_id,
        "country": p.country,
        "country_code": p.country_code,
        "current": m == Some(Match::Current),
        "near_current": m == Some(Match::Near),
    })
}

fn list(out: Output, all: &[SavedPlace], current: Option<&Location>, country: Option<&str>) {
    // Numbers stay those of the full list so `use <n>` works with --country.
    let shown: Vec<(usize, &SavedPlace)> = all
        .iter()
        .enumerate()
        .filter(|(_, p)| country.is_none_or(|c| p.country_code.eq_ignore_ascii_case(c)))
        .collect();
    let cur = current.and_then(|c| places::current_place(all, c));
    let match_of = |i: usize| cur.filter(|(j, _)| *j == i).map(|(_, m)| m);
    if out.is_json() {
        let items: Vec<Value> = shown
            .iter()
            .map(|(i, p)| {
                let mut v = place_json(p, match_of(*i));
                v["index"] = json!(i + 1);
                v
            })
            .collect();
        out.json(&json!({
            "places": items,
            "current": current.map(|c| json!({
                "poi_id": c.id,
                "address": c.address,
                "address_detail": c.address_detail,
                "note_to_driver": c.note_to_driver,
            })),
        }));
        return;
    }
    if shown.is_empty() {
        out.line("No saved places.");
        return;
    }
    for (i, p) in &shown {
        let m = match_of(*i);
        out.line(format!(
            "{} {:>2}  {}  [{}]",
            match m {
                Some(Match::Current) => "*",
                Some(Match::Near) => "~",
                None => " ",
            },
            i + 1,
            title(p),
            p.country_code
        ));
        out.line(format!("      {}", p.address));
        if m.is_some()
            && let Some(c) = current
        {
            if let Some(u) = &c.address_detail {
                out.line(format!("      Unit: {u}"));
            }
            if let Some(n) = &c.note_to_driver {
                out.line(format!("      Note: {n}"));
            }
        }
    }
}
