# For assistants: ordering food with grabcli

This is the workflow to follow when a user asks you to order (or price) GrabFood
through `grab`. Read it fully before placing anything.

## The rule that matters most

**Never place an order without the user's explicit, per-order confirmation.**
Pricing, searching and reading history are safe to do on your own. Submitting an
order is not. Always show the user the full quote — items, every fee, any
voucher, the total, the delivery address, and the quote id — and only submit
after they explicitly say yes to that specific quote.

## Always use `--json`

Run every command with `--json`. Parse the result; on error, read
`error.kind` and react (see the table below). Human output is for people.

## Workflow

1. **Find** a restaurant (all of these are read-only and safe):
   - `grab food search "<keyword>" --json`: merchants, each with the matching
     `dishes[]`. Dish `id`s are menu item ids, so they work in `cart add`.
   - `grab food search "<dish>" --dishes --max-price 200 --sort price --json`:
     a flat list of matching dishes across restaurants (`merchant_id` on each).
   - `grab food nearby --open --sort rating --json`: everything around the
     address.
   - `grab food recommended --json`: Grab's "popular near you".
   - `grab food cuisines --json` lists the cuisine and collection ids. Then run
     `grab food category <id|name> --json`, for example `category japanese`.
   - Filters (client-side) work on all list commands: `--open`,
     `--min-rating 4.5`, `--max-fee 50`, `--max-eta 40`, `--price-level 2`,
     `--promo`, `--free-delivery`, `--cuisine NAME`, `--max-distance 3`,
     `--type food|mart`, `--sort rating|eta|fee|distance|price`, `--limit N`.
   - `--pages N` pulls more results. Calls are paced 3 s apart, at most 10
     pages. Use it sparingly. A 429 stops paging, and you get what was fetched,
     with `"stopped"` set in the JSON.
   - `--local` (search, nearby, category) answers from the offline catalog of
     everything seen before. Use it to avoid API calls when the data only needs
     to be roughly current. It can't tell what is open right now.
   Pick a merchant `id`.
2. **Read the menu**:
   `grab food menu <merchant-id> --json`. Note item ids, prices, and any
   `modifier_groups` where `required` is true — the user must choose an option
   for each (use the option's `id`). If an option has `"default": true`, the app
   preselects it; mention it.
3. **Build the cart** (one merchant at a time):
   `grab food cart add <merchant-id> <item-id> --qty N --mod <group-id>:<option-id> ... --json`
   Repeat `--mod` for each required group. Clear first if switching merchants:
   `grab food cart clear`.
4. **Quote**:
   `grab food quote --json`. This is read-only — no order is placed. It returns
   `quote_id`, the itemised total, fees, promos, `eta_min`, `payment_type_id`
   and the delivery `address`.
5. **Find the best voucher** (optional):
   `grab food vouchers --best --apply --json` re-quotes with each eligible code
   and stores the cheapest. Then re-quote to see the new total.
6. **Show the user** the full quote: items (with chosen modifiers), subtotal,
   each fee, any voucher/discount, the total, the delivery address, the payment
   method, the ETA, and the `quote_id`. Ask them to confirm.
7. **Only after the user explicitly okays it**, submit with the exact fresh id:
   `grab food order --yes <quote-id> --json`.
   - First you can preview with `grab food order --dry-run --json`.
   - If the price changed since the quote, the command refuses with
     `quote_changed` — re-quote, show the user the new total, and ask again.
   - If `ordering_disabled` comes back, ordering is off in config; tell the user
     to enable it themselves (`allow_ordering = true`). Do not enable it for
     them.
8. **Track**: `grab food orders --ongoing --json`, then
   `grab food track <order-id> --json` (add `--map out.png` for a map, or
   `--watch` to follow it; `--json` is a stable `"schema": 1` model).
9. **History**: `grab food history sync --vault PATH` mirrors completed orders
   into dated markdown notes with an `index.md` of stats. Safe to run anytime.

## Delivery address

- `grab saved-places list --json`: the account's saved places (`label` is
  `Home`, `Work`, a custom label or null). `current: true` is the delivery
  address in use; `near_current: true` is the closest place within 150 m of it.
- `grab saved-places use <index|poi_id|label|name> --json` switches the
  delivery address to that place. It only changes grabcli's local session, not
  the Grab account. The unit and driver note carry over only when the place is
  the current one; otherwise pass `--unit` / `--note`. Confirm the place with
  the user first, and re-quote afterwards.

## GrabMart

Groceries, pharmacies and convenience stores (S&R, Watsons, 7-Eleven, …) use
`grab mart …`. Same rules: never order without explicit per-order confirmation.

1. **Find a store** near the saved address:
   - `grab mart search "<keyword>" --json` → stores *and* matching products
     across stores (`stores[].products[]`).
   - `grab mart stores --keyword "S&R" --json`, `grab mart stores --category 23396 --json`
     (ids from `grab mart categories --json`), or plain `grab mart stores --json`.
   Keywords need ≥3 characters.
2. **Find products in the store**:
   - `grab mart find <store-id> "red bull" --json` (in-store search; `--all` pages).
   - `grab mart store <store-id> --json` → departments; then
     `grab mart items <store-id> --department <id> [--sub <id>] --json`.
   - `grab mart item <store-id> <item-id> --json` → attributes (brand, size, …).
   Each product has `price_minor`, `discounted_price_minor`,
   `effective_price_minor`, `size` (pack size, parsed from the name or the Size
   attribute), `available`, `in_stock` and `availability`. Grab exposes no stock
   counts — only availability flags. Tell the user the pack size when items
   look alike (e.g. "4 x 250ml" vs "4 cans").
3. **Cart** (one store at a time, separate from the food cart):
   `grab mart cart add <store-id> <item-id> --qty N --json`,
   `grab mart cart show --json`, `grab mart cart remove <item-id>`,
   `grab mart cart clear`.
4. **Quote**: `grab mart quote --json` → `quote_id`, `subtotal_minor`,
   delivery before/after, `fees[]` (service, small-order, …), `discounts[]`,
   `total_minor`, `address`. Needs a logged-in session; on `auth_expired`,
   follow "Logging in" below. `grab mart quote --dry-run --json` shows the (redacted)
   request without calling Grab.
5. **Order** only after the user okays that exact quote:
   `grab mart order --yes <quote-id> --json` (preview: `--dry-run`). Same
   `quote_changed` / `ordering_disabled` / `confirmation_required` handling as
   food. Never retry a failed submit; ask the user to check the Grab app.

Mart specifics:
- Browsing mints a reCAPTCHA token in the CDP browser for every uncached call;
  `cdp_unavailable` means the browser at `cdp` isn't reachable. No login is
  needed to browse.
- The mart API is strict about bursts (429 after ~10 quick calls). `grab`
  spaces calls ≥2s and retries a 429 once; if you still get `rate_limited`,
  wait a minute. Prefer one `find`/`search` over many `item` calls; browse
  results are cached (10–30 min).

## Caching

Browse data is cached in a local SQLite database, keyed by the delivery
location. Lists are kept 10 min, menus 30 min, details 6 h and cuisine or
collection lists 24 h. Re-running `search`, `nearby` or `menu` while exploring is
cheap, so use the cached results instead of re-fetching them yourself. JSON
output has `cached_at` (unix seconds) and `age_secs`, which are null when the
data is live. Lists also have `source`: `api`, `cache`, `mixed`, `stale` or
`local`. If they used a fallback after a failed live call, they also carry
`fallback_reason`. Tell the user when results are stale or local.

Quotes, vouchers, orders, tracking and history are never cached, and
`quote`/`order` always re-check a live menu. That means the prices you show the
user are fresh.

- `grab --refresh food search … --json`: force fresh data. Use this when the
  user asks what's open *now*, or when a cached merchant looks closed or stale.
- `grab --no-cache …` or `GRAB_NO_CACHE=1`: bypass the cache entirely.
- `grab food menu <id> --images --json`: download item images. Each item then
  has an `image_path`. `grab cache image <url>` fetches one image.
- `grab cache stats --json`, `grab cache clear [--kind menu] [--expired] [--all]`,
  `grab cache prune`, `grab cache path`.

The global flags go before `food` (`grab --refresh food menu <id>`).

## Logging in

Sessions renew themselves: `gfc_session` lasts 2 h and is refreshed silently
from the 60-day SSO cookie (`grab auth refresh` does it by hand, and it never
sends anything). Only when that fails (`auth_expired`/`auth_missing`) is a real
login needed. **A login sends an SMS/WhatsApp code or a Grab-app approval to
the user's phone, so never start one on your own.**

1. **Try silent first:** `grab auth refresh --json`. If `kind` is `refreshed`,
   you're done. Don't use bare `grab auth login` for this: when the user has
   `phone` set in config.toml it sends a challenge if the silent refresh fails.
2. **Ask the user in chat first.** Say that you're about to send one login
   approval (app) or code (sms/whatsapp) to their phone, and ask which
   channel. Wait for a yes.
3. **Close stale order-tracking tabs.** Run the dry run,
   `grab auth login --channel app --dry-run --json` (add `--phone +63…` if the
   user has no `phone` in config.toml; `phone_source` says which). It sends nothing and
   lists `order_tracking_tabs`. If any are listed, ask the user to close those
   food.grab.com `/order/…` tabs. They log the browser out every ~16 s.
   grabcli never closes tabs, and you shouldn't either unless the user asks.
   Also check `next_send_allowed_in_secs` is 0.
4. **Send once:** `grab auth login --channel app --json` (plus `--phone +63…`
   if not configured).
   This sends exactly one challenge. Never re-run it to "retry". If it fails,
   read `error.kind`. `rate_limited` means wait `retry_after_secs`, and
   `captcha_required` means the user must log in in a normal browser and run
   `grab auth import`.
5. **Finish:**
   - `kind: approval_pending`: tell the user to approve in the Grab app,
     then run `grab auth login --poll --json`. If it returns
     `approval_pending` again, they haven't approved yet. Run `--poll` again.
   - `kind: otp_sent`: ask the user for the code, then run
     `grab auth login --otp <code> --json`. On `otp_invalid`, ask again
     (the page allows only a few tries). Never guess a code. Don't repeat the
     code back in chat or logs.
   - Changed your mind: `grab auth login --cancel`.
   - The login stays resumable for 10 minutes.
6. **Verify:** `grab auth status --json`. `gfc_session_logged_in` should be
   true with ~120 min left.

Limits enforced by the CLI: a 60 s cooldown and 3 sends per hour. Phone
numbers are masked in all output. Cookies, tokens and codes are never printed.

## Exactly-once

The submit endpoint has no idempotency key, so grabcli never retries it. If
`grab food order` fails with `network`, `rate_limited` or `api` after you tried
to submit, **do not run it again**. Check `grab food orders --ongoing --json`
first — the order may have gone through.

## Error kinds to handle

| kind | what to do |
|---|---|
| `auth_missing` / `auth_expired` | `grab auth refresh --json` (silent); if that fails, follow "Logging in" (ask the user first) |
| `otp_invalid` | ask the user for the code again, then `grab auth login --otp <code>` |
| `login_expired` | the pending login is gone; ask the user before sending a new one |
| `captcha_required` | the user must log in in a normal browser, then `grab auth import` |
| `hydra_expired` | retry once; if it persists, the signing setup or browser is unavailable |
| `cdp_unavailable` | the logged-in browser isn't reachable; tell the user |
| `cart_invalid` | a required modifier is missing or an item is gone; fix the cart |
| `quote_changed` | re-quote, show the new total, re-confirm with the user |
| `ordering_disabled` | ordering is off; the user must enable it themselves |
| `confirmation_required` | you didn't pass a matching `--yes <quote-id>` |
| `rate_limited` | wait and retry gently; never tight-loop |

## Don't

- Don't place or retry an order on your own judgement.
- Don't send a login challenge (`--phone` without `--dry-run`) without asking the user, and never
  repeat one to "retry".
- Don't tight-poll order history or tracking (respect ≥15s, back off on 429).
- Don't echo tokens, cookies, card details, or the user's address into logs you
  don't need to; the quote already shows the address for confirmation.
