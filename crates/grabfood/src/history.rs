//! `grab food history sync --vault <dir>`: export completed GrabFood orders to a
//! Markdown/Obsidian vault, idempotently.
//!
//! The on-disk format is documented separately (and shared with other writers):
//! one note per order under `<vault>/<YYYY>/<YYYY-MM-DD-HHMM>-<slug>.md`, plus a
//! regenerated `index.md`. Notes are keyed on the order id (the booking code for
//! API orders) so re-runs update in place and never duplicate. A note the user
//! edited keeps its text outside the `<!-- grab:begin -->`/`<!-- grab:end -->`
//! markers.
//!
//! Only completed PHP orders are written. No payment details, tokens, addresses,
//! phone numbers or driver names ever reach a note.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use grabcli_core::error::{Error, ErrorKind, Result};
use grabcli_core::money::Money;
use grabcli_core::paths::write_atomic;
use serde_json::Value;
use sha1::{Digest, Sha1};

use crate::datetime::Pht;

/// One cart line for a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub quantity: i64,
    pub name: String,
    /// (quantity, name) per chosen modifier.
    pub modifiers: Vec<(i64, String)>,
    /// Line total (qty * unit incl. modifiers), if known.
    pub line_total: Option<Money>,
}

impl Line {
    /// `<qty>x <name> (<mod>, <mod>)` for the frontmatter `items` list.
    fn item_string(&self) -> String {
        let base = format!("{}x {}", self.quantity, self.name);
        if self.modifiers.is_empty() {
            base
        } else {
            let mods: Vec<String> = self
                .modifiers
                .iter()
                .map(|(q, n)| {
                    if *q > 1 {
                        format!("{q} {n}")
                    } else {
                        n.clone()
                    }
                })
                .collect();
            format!("{base} ({})", mods.join(", "))
        }
    }

    /// `<name> (<mod>, <mod>)` for the body table's Item cell.
    fn display_name(&self) -> String {
        if self.modifiers.is_empty() {
            self.name.clone()
        } else {
            let mods: Vec<String> = self
                .modifiers
                .iter()
                .map(|(q, n)| {
                    if *q > 1 {
                        format!("{q} {n}")
                    } else {
                        n.clone()
                    }
                })
                .collect();
            format!("{} ({})", self.name, mods.join(", "))
        }
    }
}

/// A completed order ready to render.
#[derive(Debug, Clone)]
pub struct OrderRecord {
    pub key: String,
    pub when: Pht,
    pub merchant: String,
    pub branch: String,
    pub lines: Vec<Line>,
    pub subtotal: Option<Money>,
    pub fees: Vec<(String, Money)>,
    pub discount: Money,
    pub total: Money,
    pub group_order: bool,
}

impl OrderRecord {
    pub fn merchant_full(&self) -> String {
        if self.branch.is_empty() {
            self.merchant.clone()
        } else {
            format!("{} - {}", self.merchant, self.branch)
        }
    }
}

fn money(v: &Value, ptr: &str) -> Option<Money> {
    v.pointer(ptr).and_then(Value::as_i64).map(Money)
}

/// Parse one order from the history API. Returns `None` to skip (not completed,
/// or not a PHP order).
pub fn parse_order(o: &Value) -> Option<OrderRecord> {
    if o.get("orderState").and_then(Value::as_str) != Some("COMPLETED") {
        return None;
    }
    let sd = o.get("snapshotDetail")?;
    let cw = sd.get("cartWithQuote")?;
    let currency = cw
        .pointer("/foodQuote/currency/code")
        .or_else(|| cw.pointer("/currency/code"))
        .and_then(Value::as_str)
        .unwrap_or("PHP");
    if currency != "PHP" {
        return None;
    }
    let mc = cw.pointer("/merchantCartWithQuoteList/0")?;
    let full = mc
        .pointer("/merchantInfoObj/name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (merchant, branch) = split_merchant(full);

    let mut lines = Vec::new();
    for it in mc
        .get("itemsV2")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = it
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let quantity = it.get("quantity").and_then(Value::as_i64).unwrap_or(1);
        let mut modifiers = Vec::new();
        for g in it
            .get("modifierGroups")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for m in g
                .get("modifiers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let mn = m
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let mq = m.get("quantity").and_then(Value::as_i64).unwrap_or(1);
                if !mn.is_empty() {
                    modifiers.push((mq, mn));
                }
            }
        }
        let line_total =
            money(it, "/totalPriceInMinorUnit").or_else(|| money(it, "/priceInMinorUnit"));
        lines.push(Line {
            quantity,
            name,
            modifiers,
            line_total,
        });
    }

    let subtotal = money(cw, "/foodQuoteInMin/priceInMinorUnit");
    let mut fees = Vec::new();
    for f in cw
        .pointer("/otherFees/otherFeesForDisplay")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let key = f.get("key").and_then(Value::as_str).unwrap_or_default();
        // Skip the aggregate "Applicable fees"/serviceFees line.
        if key == "serviceFees" {
            continue;
        }
        let name = f
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Fee")
            .to_string();
        let amt = f.get("amountInMin").and_then(Value::as_i64).unwrap_or(0);
        fees.push((name, Money(amt)));
    }
    let discount = money(cw, "/totalQuoteInMin/promoAmountInMinorUnit").unwrap_or(Money::ZERO);
    let total = money(cw, "/totalQuoteInMin/reducedPriceInMinorUnit")
        .or_else(|| money(cw, "/totalQuoteInMin/priceInMinorUnit"))?;

    let created = o.get("createdAt").and_then(Value::as_str)?;
    let when = Pht::from_rfc3339_utc(created)?;

    let booking = o
        .get("bookingCode")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let key = if booking.is_empty() {
        fallback_key(&when, full, total)
    } else {
        booking.to_string()
    };

    let group_order = o
        .get("groupOrderGroupID")
        .and_then(Value::as_str)
        .is_some_and(|g| !g.is_empty());

    Some(OrderRecord {
        key,
        when,
        merchant,
        branch,
        lines,
        subtotal,
        fees,
        discount,
        total,
        group_order,
    })
}

fn split_merchant(full: &str) -> (String, String) {
    match full.split_once(" - ") {
        Some((brand, branch)) => (brand.trim().to_string(), branch.trim().to_string()),
        None => (full.trim().to_string(), String::new()),
    }
}

/// Fallback key for an order with no booking code:
/// `receipt-` + first 12 hex of `sha1("<YYYY-MM-DD HH:MM>|<full>|<total 2dp>")`.
pub fn fallback_key(when: &Pht, full: &str, total: Money) -> String {
    let basis = format!(
        "{} {}|{}|{:.2}",
        when.ymd(),
        when.hhmm(),
        full,
        total.as_major_f64()
    );
    let digest = Sha1::digest(basis.as_bytes());
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("receipt-{hex}")
}

/// NFKD-ish ASCII slug of the brand (part before ` - `).
pub fn slug(brand: &str) -> String {
    let lowered = brand.to_lowercase();
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in lowered.chars() {
        if ch == '\'' || ch == '\u{2019}' {
            continue; // drop apostrophes
        }
        let keep = ascii_fold(ch);
        if keep.is_ascii_alphanumeric() {
            out.push(keep);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "merchant".to_string()
    } else {
        trimmed
    }
}

// Fold a handful of common accented Latin letters to ASCII; everything else
// that isn't alphanumeric becomes a separator.
fn ascii_fold(c: char) -> char {
    match c {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' => 'o',
        'ú' | 'ù' | 'û' | 'ü' => 'u',
        'ñ' => 'n',
        'ç' => 'c',
        c if c.is_ascii() => c,
        _ => ' ',
    }
}

// ---- Markdown rendering -----------------------------------------------------

const BEGIN: &str = "<!-- grab:begin -->";
const END: &str = "<!-- grab:end -->";

fn yaml_q(s: &str) -> String {
    // JSON string escaping gives us valid YAML double-quoted scalars.
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

fn peso(m: Money) -> String {
    m.peso()
}

fn esc_cell(s: &str) -> String {
    s.replace('|', "\\|")
}

fn frontmatter(rec: &OrderRecord, user: &str, cwd: &str) -> String {
    let title = format!("{} — {}", rec.merchant, rec.when.pretty());
    let items: Vec<String> = rec.lines.iter().map(|l| yaml_q(&l.item_string())).collect();
    format!(
        "---\n\
         title: {}\n\
         date: {}\n\
         time: {}\n\
         user: {user}\n\
         cwd: {cwd}\n\
         tags: [food, grabfood]\n\
         order_id: {}\n\
         merchant: {}\n\
         branch: {}\n\
         items: [{}]\n\
         total_php: {}\n\
         group_order: {}\n\
         source: grab-api\n\
         ---\n",
        yaml_q(&title),
        rec.when.ymd(),
        yaml_q(&rec.when.hhmm()),
        yaml_q(&rec.key),
        yaml_q(&rec.merchant),
        yaml_q(&rec.branch),
        items.join(", "),
        rec.total.trimmed_major(),
        rec.group_order,
    )
}

fn block(rec: &OrderRecord) -> String {
    let mut s = String::new();
    s.push_str(BEGIN);
    s.push('\n');
    s.push_str(&format!(
        "# {} — {} {}\n\n",
        rec.merchant,
        rec.when.pretty(),
        rec.when.hhmm()
    ));
    if !rec.branch.is_empty() {
        s.push_str(&format!("**Branch:** {}  \n\n", rec.branch));
    }
    if rec.group_order {
        s.push_str("> [!note] Group order: items and total are for the whole group.\n\n");
    }

    // Items table.
    s.push_str("| Qty | Item | Price |\n|---:|---|---:|\n");
    for l in &rec.lines {
        let price = l.line_total.map(peso).unwrap_or_default();
        s.push_str(&format!(
            "| {} | {} | {} |\n",
            l.quantity,
            esc_cell(&l.display_name()),
            price
        ));
    }
    s.push('\n');

    // Charges table, or a plain total if we have no breakdown.
    let have_breakdown = rec.subtotal.is_some() || !rec.fees.is_empty();
    if have_breakdown {
        s.push_str("| | Amount |\n|---|---:|\n");
        if let Some(sub) = rec.subtotal {
            s.push_str(&format!("| Subtotal | {} |\n", peso(sub)));
        }
        for (name, amt) in &rec.fees {
            s.push_str(&format!("| {} | {} |\n", esc_cell(name), peso(*amt)));
        }
        if rec.discount.minor() != 0 {
            // Discounts shown as a negative amount with a true minus sign.
            let d = Money(-rec.discount.minor().abs());
            s.push_str(&format!(
                "| Discounts | −{} |\n",
                peso(d).trim_start_matches('-')
            ));
        }
        s.push_str(&format!("| **Total** | **{}** |\n", peso(rec.total)));
    } else {
        s.push_str(&format!("**Total paid:** {}\n", peso(rec.total)));
    }

    s.push_str("\n## Related\n\n- [[index|GrabFood Orders]]\n");
    s.push_str(END);
    s.push('\n');
    s
}

/// Render a complete new note.
pub fn render_note(rec: &OrderRecord, user: &str, cwd: &str) -> String {
    format!("{}\n{}", frontmatter(rec, user, cwd), block(rec))
}

/// Replace the marker block (and regenerate frontmatter) in an existing note,
/// preserving any user text outside the markers.
pub fn update_note(existing: &str, rec: &OrderRecord, user: &str, cwd: &str) -> String {
    let body = strip_frontmatter(existing);
    let new_block = block(rec);
    let new_body = match (body.find(BEGIN), body.find(END)) {
        (Some(b), Some(e)) if e > b => {
            let after = &body[e + END.len()..];
            let before = &body[..b];
            format!("{before}{new_block}{after}")
        }
        _ => {
            // No markers: insert the block first, keep the old body after it.
            format!("{new_block}\n{}", body.trim_start_matches('\n'))
        }
    };
    // Normalise trailing whitespace so a re-run of an unchanged order is a
    // byte-for-byte match with `render_note` (idempotence).
    format!("{}\n{}\n", frontmatter(rec, user, cwd), new_body.trim())
}

fn strip_frontmatter(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---\n") {
            return rest[end + 5..].to_string();
        }
        if let Some(end) = rest.find("\n---") {
            return rest[end + 4..].to_string();
        }
    }
    s.to_string()
}

// ---- Existing-note index ----------------------------------------------------

/// A minimally-parsed existing note, for idempotence and fallback upgrades.
#[derive(Debug, Clone)]
pub struct ExistingNote {
    pub path: PathBuf,
    pub order_id: String,
    pub merchant: String,
    pub branch: String,
    pub total_php: f64,
    pub date: String,
    pub time: String,
    pub source: String,
    pub items: Vec<String>,
    pub group_order: bool,
}

impl ExistingNote {
    pub fn merchant_full(&self) -> String {
        if self.branch.is_empty() {
            self.merchant.clone()
        } else {
            format!("{} - {}", self.merchant, self.branch)
        }
    }
}

fn fm_value(fm: &str, key: &str) -> Option<String> {
    for line in fm.lines() {
        if let Some(rest) = line.strip_prefix(key)
            && let Some(v) = rest.strip_prefix(": ")
        {
            let v = v.trim();
            // Unquote if double-quoted.
            if v.starts_with('"')
                && let Ok(s) = serde_json::from_str::<String>(v)
            {
                return Some(s);
            }
            return Some(v.to_string());
        }
    }
    None
}

/// Scan `<vault>/<YYYY>/*.md`, returning the parsed notes.
pub fn scan_vault(vault: &Path) -> Result<Vec<ExistingNote>> {
    let mut out = Vec::new();
    let Ok(years) = std::fs::read_dir(vault) else {
        return Ok(out);
    };
    for year in years.flatten() {
        if !year.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(year.path()) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            let fm = frontmatter_text(&text);
            let order_id = fm_value(&fm, "order_id").unwrap_or_default();
            if order_id.is_empty() {
                continue;
            }
            out.push(ExistingNote {
                path: p,
                order_id,
                merchant: fm_value(&fm, "merchant").unwrap_or_default(),
                branch: fm_value(&fm, "branch").unwrap_or_default(),
                total_php: fm_value(&fm, "total_php")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0),
                date: fm_value(&fm, "date").unwrap_or_default(),
                time: fm_value(&fm, "time").unwrap_or_default(),
                source: fm_value(&fm, "source").unwrap_or_default(),
                items: fm_items(&fm),
                group_order: fm_value(&fm, "group_order").as_deref() == Some("true"),
            });
        }
    }
    Ok(out)
}

// Parse the `items: [ ... ]` frontmatter line into its strings.
fn fm_items(fm: &str) -> Vec<String> {
    for line in fm.lines() {
        if let Some(rest) = line.strip_prefix("items: ")
            && let Ok(Value::Array(a)) = serde_json::from_str::<Value>(rest.trim())
        {
            return a
                .into_iter()
                .filter_map(|v| v.as_str().map(ToString::to_string))
                .collect();
        }
    }
    Vec::new()
}

fn frontmatter_text(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---")
    {
        return rest[..end].to_string();
    }
    String::new()
}

fn days_between(a: &str, b: &str) -> Option<i64> {
    let parse = |d: &str| {
        let mut it = d.split('-');
        let y: i64 = it.next()?.parse().ok()?;
        let m: u32 = it.next()?.parse().ok()?;
        let dd: u32 = it.next()?.parse().ok()?;
        Some(crate::datetime::Pht {
            year: y,
            month: m,
            day: dd,
            hour: 0,
            minute: 0,
        })
    };
    let pa = parse(a)?;
    let pb = parse(b)?;
    // Reuse the civil-day math via a round-trip through file epoch is overkill;
    // compute ordinal directly.
    Some((ordinal(&pa) - ordinal(&pb)).abs())
}

fn ordinal(p: &crate::datetime::Pht) -> i64 {
    // Days from a fixed epoch via the same algorithm the datetime module uses
    // internally; re-derive here to avoid widening its public API.
    let y = if p.month <= 2 { p.year - 1 } else { p.year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153
        * (if p.month > 2 {
            p.month - 3
        } else {
            p.month + 9
        }) as i64
        + 2)
        / 5
        + p.day as i64
        - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Decide the on-disk action for one order against the existing notes.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Write a new note at this path.
    Create(PathBuf),
    /// Update the note already at this path (same key or adopted fallback).
    Update(PathBuf),
}

/// Choose create-vs-update for `rec`, considering already-assigned paths this
/// run (`taken`). Mutates nothing.
pub fn plan_action(
    rec: &OrderRecord,
    existing: &[ExistingNote],
    taken: &mut std::collections::BTreeSet<PathBuf>,
    vault: &Path,
) -> Action {
    // 1. A note already keyed on this order id → update it.
    if let Some(n) = existing.iter().find(|n| n.order_id == rec.key) {
        return Action::Update(n.path.clone());
    }
    // 2. Adopt a fallback (receipt-…) note for the same order.
    if let Some(n) = existing.iter().find(|n| {
        n.order_id.starts_with("receipt-")
            && n.merchant_full() == rec.merchant_full()
            && (n.total_php - rec.total.as_major_f64()).abs() < 0.005
            && days_between(&n.date, &rec.when.ymd()).is_some_and(|d| d <= 1)
    }) {
        return Action::Update(n.path.clone());
    }
    // 3. New note: compute a free path.
    let dir = vault.join(format!("{:04}", rec.when.year));
    let stamp = rec.when.file_stamp();
    let slug = slug(&rec.merchant);
    let mut candidate = dir.join(format!("{stamp}-{slug}.md"));
    let mut n = 2;
    while candidate.exists() || taken.contains(&candidate) {
        candidate = dir.join(format!("{stamp}-{slug}-{n}.md"));
        n += 1;
    }
    taken.insert(candidate.clone());
    Action::Create(candidate)
}

/// Apply orders to the vault. Returns (created, updated, unchanged) counts.
/// Writes notes atomically; only rewrites when content changed.
pub fn apply(vault: &Path, records: &[OrderRecord], user: &str) -> Result<(usize, usize, usize)> {
    let cwd = vault.to_string_lossy().into_owned();
    let existing = scan_vault(vault)?;
    let mut taken = std::collections::BTreeSet::new();
    let (mut created, mut updated, mut unchanged) = (0, 0, 0);

    // Assign in ascending (datetime, key) order for deterministic collisions.
    let mut sorted: Vec<&OrderRecord> = records.iter().collect();
    sorted.sort_by(|a, b| (a.when.file_stamp(), &a.key).cmp(&(b.when.file_stamp(), &b.key)));

    for rec in sorted {
        match plan_action(rec, &existing, &mut taken, vault) {
            Action::Create(path) => {
                let content = render_note(rec, user, &cwd);
                write_atomic(&path, content.as_bytes())?;
                created += 1;
            }
            Action::Update(path) => {
                let old = std::fs::read_to_string(&path).unwrap_or_default();
                let content = update_note(&old, rec, user, &cwd);
                if content == old {
                    unchanged += 1;
                } else {
                    write_atomic(&path, content.as_bytes())?;
                    updated += 1;
                }
            }
        }
    }

    // Regenerate index.md from the full note set (including what we just wrote).
    let all = scan_vault(vault)?;
    let index = generate_index(&all);
    let index_path = vault.join("index.md");
    let old_index = std::fs::read_to_string(&index_path).unwrap_or_default();
    if index != old_index {
        write_atomic(&index_path, index.as_bytes())?;
    }

    Ok((created, updated, unchanged))
}

fn money_from_php(php: f64) -> Money {
    Money((php * 100.0).round() as i64)
}

/// Fully regenerate `index.md` from the parsed notes.
pub fn generate_index(notes: &[ExistingNote]) -> String {
    let mut s = String::new();
    let paid: Vec<&ExistingNote> = notes.iter().filter(|n| n.total_php > 0.0).collect();
    let dates: Vec<&str> = {
        let mut d: Vec<&str> = notes
            .iter()
            .map(|n| n.date.as_str())
            .filter(|d| !d.is_empty())
            .collect();
        d.sort_unstable();
        d
    };
    let latest = dates.last().copied().unwrap_or("");
    let earliest = dates.first().copied().unwrap_or("");

    s.push_str(&format!(
        "---\ntitle: \"GrabFood Orders\"\ndate: {}\ntags: [food, grabfood]\n---\n\n",
        if latest.is_empty() {
            "1970-01-01"
        } else {
            latest
        }
    ));
    s.push_str("# GrabFood Orders\n\n");
    s.push_str("Generated by grabcli; this file is fully rewritten each run, so keep manual notes elsewhere.\n\n");

    // Totals.
    let total_spend: f64 = paid.iter().map(|n| n.total_php).sum();
    let mean = if paid.is_empty() {
        0.0
    } else {
        total_spend / paid.len() as f64
    };
    let median = {
        let mut v: Vec<f64> = paid.iter().map(|n| n.total_php).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
        if v.is_empty() {
            0.0
        } else if v.len() % 2 == 1 {
            v[v.len() / 2]
        } else {
            (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0
        }
    };
    let min = paid
        .iter()
        .map(|n| n.total_php)
        .fold(f64::INFINITY, f64::min);
    let max = paid.iter().map(|n| n.total_php).fold(0.0_f64, f64::max);
    let groups = notes.iter().filter(|n| n.group_order).count();
    let with_id = notes
        .iter()
        .filter(|n| !n.order_id.starts_with("receipt-"))
        .count();
    let receipt_only = notes.len() - with_id;

    s.push_str("## Totals\n\n");
    s.push_str(&format!(
        "- **Orders:** {} ({earliest} to {latest})\n",
        notes.len()
    ));
    s.push_str(&format!(
        "- **Spend:** {} across {} paid orders\n",
        money_from_php(total_spend).peso(),
        paid.len()
    ));
    s.push_str(&format!(
        "- **Mean / median:** {} / {}\n",
        money_from_php(mean).peso(),
        money_from_php(median).peso()
    ));
    if !paid.is_empty() {
        s.push_str(&format!(
            "- **Range:** {} – {}\n",
            money_from_php(min).peso(),
            money_from_php(max).peso()
        ));
    }
    s.push_str(&format!("- **Group orders:** {groups}\n"));
    s.push_str(&format!(
        "- **With Grab order id:** {with_id}; receipt-only (fallback key): {receipt_only}\n\n"
    ));

    // By year.
    let mut by_year: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    for n in notes {
        if n.date.len() >= 4 {
            let e = by_year.entry(n.date[..4].to_string()).or_default();
            e.0 += 1;
            e.1 += n.total_php;
        }
    }
    s.push_str("## By year\n\n| Year | Orders | Spend |\n|---|---:|---:|\n");
    for (y, (c, sp)) in &by_year {
        s.push_str(&format!("| {y} | {c} | {} |\n", money_from_php(*sp).peso()));
    }
    s.push('\n');

    // Monthly spend.
    let mut by_month: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    for n in notes {
        if n.date.len() >= 7 {
            let e = by_month.entry(n.date[..7].to_string()).or_default();
            e.0 += 1;
            e.1 += n.total_php;
        }
    }
    s.push_str("## Monthly spend\n\n| Month | Orders | Spend |\n|---|---:|---:|\n");
    for (m, (c, sp)) in &by_month {
        s.push_str(&format!("| {m} | {c} | {} |\n", money_from_php(*sp).peso()));
    }
    s.push('\n');

    // Top 20 places (by order count, grouped by brand).
    let mut places: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    for n in notes {
        let e = places.entry(n.merchant.clone()).or_default();
        e.0 += 1;
        e.1 += n.total_php;
    }
    let mut places: Vec<(String, (usize, f64))> = places.into_iter().collect();
    places.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(&b.0)));
    s.push_str("## Top 20 places\n\n| Place | Orders | Spend |\n|---|---:|---:|\n");
    for (name, (c, sp)) in places.iter().take(20) {
        s.push_str(&format!(
            "| {name} | {c} | {} |\n",
            money_from_php(*sp).peso()
        ));
    }
    s.push('\n');

    // Top 30 items (by quantity, modifiers stripped).
    let mut items: BTreeMap<String, i64> = BTreeMap::new();
    for n in notes {
        for it in &n.items {
            let (qty, name) = split_item(it);
            *items.entry(name).or_default() += qty;
        }
    }
    let mut items: Vec<(String, i64)> = items.into_iter().collect();
    items.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    s.push_str("## Top 30 items\n\nCounted by quantity, item name without modifiers.\n\n| Item | Qty |\n|---|---:|\n");
    for (name, qty) in items.iter().take(30) {
        s.push_str(&format!("| {name} | {qty} |\n"));
    }
    s.push('\n');

    // Time of day.
    let windows = [
        ("Morning (05–11)", 5, 11),
        ("Lunch (11–15)", 11, 15),
        ("Afternoon (15–18)", 15, 18),
        ("Dinner (18–22)", 18, 22),
    ];
    let mut tod: Vec<(usize, f64)> = vec![(0, 0.0); 5];
    for n in notes {
        let h: u32 = n.time.get(..2).and_then(|s| s.parse().ok()).unwrap_or(0);
        let idx = if (5..11).contains(&h) {
            0
        } else if (11..15).contains(&h) {
            1
        } else if (15..18).contains(&h) {
            2
        } else if (18..22).contains(&h) {
            3
        } else {
            4
        };
        tod[idx].0 += 1;
        tod[idx].1 += n.total_php;
    }
    s.push_str("## Time of day (PHT)\n\n| Window | Orders | Spend |\n|---|---:|---:|\n");
    for (i, (label, _, _)) in windows.iter().enumerate() {
        s.push_str(&format!(
            "| {label} | {} | {} |\n",
            tod[i].0,
            money_from_php(tod[i].1).peso()
        ));
    }
    s.push_str(&format!(
        "| Late night (22–05) | {} | {} |\n\n",
        tod[4].0,
        money_from_php(tod[4].1).peso()
    ));

    // Day of week.
    let mut dow: BTreeMap<&str, (usize, f64)> = BTreeMap::new();
    for n in notes {
        if let Some(p) = parse_date(&n.date) {
            let e = dow.entry(p.weekday()).or_default();
            e.0 += 1;
            e.1 += n.total_php;
        }
    }
    s.push_str("## Day of week\n\n| Day | Orders | Spend |\n|---|---:|---:|\n");
    for day in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"] {
        let (c, sp) = dow.get(day).copied().unwrap_or((0, 0.0));
        s.push_str(&format!(
            "| {day} | {c} | {} |\n",
            money_from_php(sp).peso()
        ));
    }
    s.push('\n');

    // Last 20 orders (wikilinks by note basename).
    let mut recent: Vec<&ExistingNote> = notes.iter().collect();
    recent.sort_by(|a, b| {
        (b.date.as_str(), b.time.as_str()).cmp(&(a.date.as_str(), a.time.as_str()))
    });
    s.push_str("## Last 20 orders\n\n");
    for n in recent.iter().take(20) {
        if let Some(stem) = n.path.file_stem().and_then(|x| x.to_str()) {
            s.push_str(&format!(
                "- [[{stem}]] — {} {}\n",
                n.date,
                money_from_php(n.total_php).peso()
            ));
        }
    }
    s.push('\n');

    s.push_str("## Caveats\n\n");
    s.push_str("- Spend stats exclude ₱0 orders; order counts include them.\n");
    s.push_str("- Receipt-only orders (no Grab order id) are keyed on a content hash.\n\n");
    s.push_str("## Related\n\n- [[index|GrabFood Orders]]\n");
    s
}

fn split_item(it: &str) -> (i64, String) {
    // "<qty>x <name> (mods)" -> (qty, name without mods)
    let (qty, rest) = match it.split_once("x ") {
        Some((q, r)) => (q.trim().parse().unwrap_or(1), r),
        None => (1, it),
    };
    let name = rest.split(" (").next().unwrap_or(rest).trim().to_string();
    (qty, name)
}

fn parse_date(d: &str) -> Option<crate::datetime::Pht> {
    let mut it = d.split('-');
    Some(crate::datetime::Pht {
        year: it.next()?.parse().ok()?,
        month: it.next()?.parse().ok()?,
        day: it.next()?.parse().ok()?,
        hour: 0,
        minute: 0,
    })
}

/// Validate that `vault` is a directory we can write to.
pub fn ensure_vault(vault: &Path) -> Result<()> {
    if vault.as_os_str().is_empty() {
        return Err(Error::new(ErrorKind::InvalidInput, "--vault is required"));
    }
    std::fs::create_dir_all(vault)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> OrderRecord {
        OrderRecord {
            key: "A-EXAMPLE123".into(),
            when: Pht::from_rfc3339_utc("2026-03-05T11:42:00Z").expect("pht"),
            merchant: "Sample Burger Bar".into(),
            branch: "Downtown".into(),
            lines: vec![Line {
                quantity: 2,
                name: "Double Burger".into(),
                modifiers: vec![(1, "No Onions".into()), (1, "Large Fries".into())],
                line_total: Some(Money(70000)),
            }],
            subtotal: Some(Money(79000)),
            fees: vec![("Delivery fee".into(), Money(4900))],
            discount: Money(2650),
            total: Money(81250),
            group_order: false,
        }
    }

    #[test]
    fn slug_rules() {
        assert_eq!(slug("Joe's Noodles & Co"), "joes-noodles-co");
        assert_eq!(slug("McDonald's"), "mcdonalds");
        assert_eq!(slug("Café Olé"), "cafe-ole");
        assert_eq!(slug("!!!"), "merchant");
    }

    #[test]
    fn fallback_key_is_stable() {
        let when = Pht::from_rfc3339_utc("2026-01-06T03:37:00Z").expect("pht"); // 11:37 PHT
        let k = fallback_key(&when, "Example Grill - Sample City", Money(73000));
        assert!(k.starts_with("receipt-"));
        assert_eq!(k.len(), "receipt-".len() + 12);
        // Deterministic.
        assert_eq!(
            k,
            fallback_key(&when, "Example Grill - Sample City", Money(73000))
        );
    }

    #[test]
    fn note_has_expected_shape() {
        let n = render_note(&rec(), "someone", "/vault/food/grab");
        assert!(n.contains("title: \"Sample Burger Bar — Mar 5, 2026\""));
        assert!(n.contains("order_id: \"A-EXAMPLE123\""));
        assert!(n.contains("total_php: 812.5"));
        assert!(n.contains("source: grab-api"));
        assert!(n.contains("items: [\"2x Double Burger (No Onions, Large Fries)\"]"));
        assert!(n.contains("**Branch:** Downtown"));
        assert!(n.contains("| Subtotal | ₱790.00 |"));
        assert!(n.contains("| Discounts | −₱26.50 |"));
        assert!(n.contains("| **Total** | **₱812.50** |"));
        assert!(n.contains(BEGIN) && n.contains(END));
    }

    #[test]
    fn update_preserves_user_text() {
        let original = render_note(&rec(), "someone", "/v");
        // Simulate the user adding text before and after the block.
        let edited =
            original.replace(BEGIN, &format!("My note above.\n\n{BEGIN}")) + "\nMy note below.\n";
        let updated = update_note(&edited, &rec(), "someone", "/v");
        assert!(updated.contains("My note above."));
        assert!(updated.contains("My note below."));
        assert!(updated.contains("order_id: \"A-EXAMPLE123\""));
    }

    #[test]
    fn idempotent_second_run_no_change() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let v = dir.path();
        let (c1, u1, _) = apply(v, &[rec()], "someone")?;
        assert_eq!((c1, u1), (1, 0));
        let (c2, u2, un2) = apply(v, &[rec()], "someone")?;
        assert_eq!((c2, u2, un2), (0, 0, 1));
        Ok(())
    }

    #[test]
    fn adopts_fallback_receipt_note() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let v = dir.path();
        // Seed a fallback (email-receipt) note for the same order.
        let when = rec().when;
        let fb_key = fallback_key(&when, "Sample Burger Bar - Downtown", Money(81250));
        let mut seed = rec();
        seed.key = fb_key;
        // Write it as an email-receipt note.
        let year_dir = v.join("2026");
        std::fs::create_dir_all(&year_dir)?;
        let seed_path = year_dir.join(format!("{}-sample-burger-bar.md", when.file_stamp()));
        let mut note = render_note(&seed, "someone", &v.to_string_lossy());
        note = note.replace("source: grab-api", "source: email-receipt");
        std::fs::write(&seed_path, &note)?;

        // Now sync the API order (real booking code) — should adopt, not create.
        let (created, updated, _) = apply(v, &[rec()], "someone")?;
        assert_eq!((created, updated), (0, 1));
        let after = std::fs::read_to_string(&seed_path)?;
        assert!(after.contains("order_id: \"A-EXAMPLE123\""));
        assert!(after.contains("source: grab-api"));
        // Only one note total.
        let count = std::fs::read_dir(&year_dir)?.count();
        assert_eq!(count, 1);
        Ok(())
    }
}
