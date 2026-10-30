# grabcli

A command-line client for Grab, starting with GrabFood (Philippines). It talks
to the same web API the food.grab.com site uses, so you can search restaurants,
read menus, build a cart, get a real price quote (with fees, vouchers and your
delivery address), and — if you turn it on yourself — place an order, all from a
terminal or a script.

The binary is `grab`. Commands are grouped by service: `grab food …` for
GrabFood, `grab mart …` for GrabMart (groceries, pharmacies, convenience
stores), `grab auth …` for session management. A future Grab service would slot
in as `grab <service> …` reusing the same auth and HTTP core.

Status: v0. Search, discovery (nearby, recommended, cuisines, categories, offline catalog search), menu, cart, quote, vouchers, order history and order
tracking are implemented and tested against the live API. Order placement is
implemented but **disabled by default** (see Safety).

## Disclaimer

grabcli is an unofficial, independent project. It is not affiliated with,
endorsed by, or supported by Grab Holdings or any of its subsidiaries. "Grab"
and "GrabFood" are trademarks of their respective owners.

It works by calling Grab's private web API, the same one food.grab.com uses.
That API is undocumented and can change or break at any time without notice,
and using it may be against Grab's terms of service. Use it at your own risk:
you are responsible for your account and for any orders placed with it.

Ordering is off by default. Nothing can be submitted until you set
`allow_ordering = true` in your config yourself (see Safety below). `grab mart`
ordering is gated the same way: `allow_ordering` plus `--yes <quote-id>`.

## Why

The GrabFood site is the only way to order, and it is a browser app. This makes
the read-only parts (what's the menu, what would this cost, what did I order
before) scriptable, and makes placing a known order a single reviewable command
instead of a point-and-click session.

## Safety: ordering is off until you turn it on

Placing an order is gated three ways:

1. The order-submission URL is only added to the HTTP client's allowlist when
   `allow_ordering = true` in your config. With it off (the default), any attempt
   to submit fails before a request leaves your machine.
2. `grab food order` and `grab mart order` re-quote the cart and refuses unless you either pass
   `--yes <quote-id>` matching the fresh quote, or confirm interactively.
3. There is no server-side idempotency key for the submit endpoint, so grabcli
   never retries it. On any ambiguous failure it tells you to check
   `grab food orders --ongoing` rather than risk a double order.

Turn ordering on yourself, deliberately, by editing your config.

## Install

With Nix:

```
nix run github:macalinao/grabcli -- food search pizza
# or install into your profile
nix profile install github:macalinao/grabcli
```

With cargo:

```
cargo install --path crates/grabcli   # installs the `grab` binary
```

## Auth setup

grabcli never creates Grab credentials on its own. You either import them from
a browser that is already logged in, or let `grab auth login` drive the real
Grab login page in an isolated context of that browser while you approve the
login.

```
# Log in through the browser exposing CDP (default http://127.0.0.1:9222):
grab auth login --phone +63XXXXXXXXXX --dry-run   # shows the plan, sends nothing
grab auth login --phone +63XXXXXXXXXX             # sends ONE approval request / code
                                                  # (or set `phone` in config.toml and
                                                  #  run plain `grab auth login`)
grab auth login --poll            # after approving in the Grab app
grab auth login --otp 123456      # or: finish with the code Grab sent

# Or import from a browser that's already logged in at food.grab.com:
grab auth import --cdp http://127.0.0.1:9222
# Or from a session file you already have:
grab auth import --from /path/to/session.json
# Re-sync only the saved delivery address from the browser (keeps the login):
grab auth import --address-only
# Or deliver to one of the account's saved places (Home, Work, …); local only:
grab saved-places list
grab saved-places use home --unit "Unit 1A"

# Renew the 2-hour session silently via the 60-day SSO cookie (never sends a challenge):
grab auth refresh

# Check what you've got (expiries only, never values; also any pending login):
grab auth status
```

Two credentials matter:

- **Cookies.** `gfc_session` (food.grab.com) lasts 2 hours. The GrabID SSO
  cookie `grabid-openid-authn-ck` lasts up to 60 days when the login ticked
  "Remember me". `grab auth refresh` turns the SSO cookie into a fresh
  `gfc_session` with the same silent OAuth round trip a browser makes. No
  browser and no challenge are involved. The client does this by itself when
  the session is about to expire or on a `401` (once, then it retries the
  request once), unless you set `auto_refresh = false`. Order submission is
  never retried. When the SSO is dead, run `grab auth login`.
  `grab auth login` sends at most one challenge per run, with a 60 s
  cooldown and a cap of 3 per hour. It keeps the login tab alive for 10
  minutes so `--otp`/`--poll` can finish it later. It warns about
  food.grab.com order-tracking tabs: those log a browser out (see
  [docs/login.md](docs/login.md) §5). See [docs/auth.md](docs/auth.md) for
  the details.
- **The hydra device JWT**, a ~10-minute token the menu/quote/order endpoints
  require. grabcli mints it automatically. See [docs/hydra.md](docs/hydra.md) for
  how, and the native-vs-browser trade-off.

Config lives at `~/.config/grabcli/config.toml` (or `$XDG_CONFIG_HOME`). Session
state is `~/.config/grabcli/session.json` (mode 0600; override with
`GRABCLI_SESSION`). The local cart and saved quotes live under
`~/.local/state/grabcli/food/`.

Example `config.toml`:

```toml
# Real orders stay impossible until you set this yourself.
allow_ordering = false

# Default payment method id (shown on every quote).
payment_type_id = "123456789"

# A logged-in browser's CDP endpoint, used to import cookies and (optionally)
# mint hydra.
cdp = "http://127.0.0.1:9222"

# auto | native | cdp. "auto" mints natively if a signing key is present, else
# falls back to the browser.
hydra_mode = "auto"

# Phone `grab auth login` uses when --phone isn't given (international form).
# Only ever printed masked. With it set, plain `grab auth login` sends a
# challenge when the silent refresh fails.
phone = "+63XXXXXXXXXX"

# Response cache (optional; these are the defaults, TTLs in seconds).
[cache]
enabled = true
[cache.ttl]
list = 600          # restaurant/store lists and search results
menu = 1800         # menus
details = 21600     # static restaurant/store details
categories = 86400  # cuisine/category lists
# image_max_mb = 500        # LRU cap for the image store
# index_max_age_days = 30   # `grab cache prune` drops catalog rows older than this
```

## Caching

Read-only browse data is cached in one SQLite database,
`~/.cache/grabcli/cache.sqlite` (or under `$XDG_CACHE_HOME/grabcli` or
`$GRABCLI_CACHE_DIR`). It is created with mode 0600 and uses WAL, so several `grab`
processes can share it. There are three parts:

- **Responses**: the raw upstream JSON. Entries are keyed by request and by
  delivery location, rounded to about 11 m. Search, nearby, category and
  recommended lists keep 10 min. Menus keep 30 min. Details keep 6 h. Cuisine
  and collection lists keep 24 h.
- **Catalog**: every restaurant, menu category and menu item that a live
  response returns is upserted into normal tables. These have full-text
  (FTS5, accent-insensitive, prefix) and trigram (substring) indexes. Each
  restaurant also records the locations it was seen from. The `--local` flag
  searches this catalog with no network access.
- **Images**: Grab CDN images are fetched only on request (`grab food menu
  --images`, `grab cache image URL`). They are stored once per content hash
  under `images/`. The store is LRU-evicted past `image_max_mb`, which
  defaults to 500.

Human output marks cached data (`cached 4m ago`), and `--json` carries
`cached_at`/`age_secs`. If a live list call fails (for example a 429 or a
network error), list commands fall back to a stale cached response, then to
the local catalog, and say so.

What is **never** cached: carts, quotes, vouchers, orders, order
history/tracking, and anything auth-related. `quote`, `vouchers` and `order`
always fetch a live menu before pricing.

```
grab --refresh food search ramen    # skip cached data, store the fresh response
grab --no-cache food menu <id>      # bypass the cache entirely (or GRAB_NO_CACHE=1)
grab cache stats [--json]           # entries per kind, catalog row counts, images, db size
grab cache clear [--kind KIND] [--expired]   # responses only; KIND: list|menu|details|categories
grab cache clear --all              # also wipe the catalog and the image store
grab cache prune                    # expired responses, old catalog rows, image LRU, VACUUM
grab cache path [--json]
grab cache image <URL>              # download a grab.com CDN image, print its local path
```

`--no-cache` and `--refresh` are global flags, so put them before the service
(`grab --refresh food …`). `--no-cache` also skips catalog indexing. A corrupt
database is recreated automatically, and deleting the directory is
always safe. Older versions kept JSON files in per-kind directories; the first
run of this version removes them.

## Commands

Every command accepts `--json` for machine-readable output; errors then print
`{"error":{"kind","message"}}` on stderr with a matching exit code.

```
grab food search <keyword> [--dishes] [--max-price PESOS] [--local] [FILTERS]
grab food nearby [--local] [FILTERS]
grab food recommended [FILTERS]
grab food cuisines
grab food category <id|name> [--local] [FILTERS]
grab food menu <merchant-id> [--images]
grab food cart add <merchant-id> <item-id> [--qty N] [--mod GROUP:OPTION ...] [--note TEXT]
grab food cart rm <item-id>
grab food cart show
grab food cart clear
grab food quote [--voucher CODE] [--payment ID]
grab food vouchers [--best] [--apply]
grab food order [--yes <quote-id>] [--dry-run] [--notes TEXT]
grab food orders [--ongoing] [--limit N]
grab food track <order-id> [--watch] [--map out.png]
grab food history sync --vault PATH [--limit N]
grab mart stores [--category ID] [--keyword TEXT] [--offset N]
grab mart categories
grab mart search <keyword>
grab mart store <store-id>
grab mart items <store-id> --department ID [--sub ID] [--offset N] [--all]
grab mart find <store-id> <keyword> [--offset N] [--all]
grab mart item <store-id> <item-id>
grab mart cart add <store-id> <item-id> [--qty N] [--note TEXT]
grab mart cart remove <item-id> | show | clear
grab mart quote [--payment ID] [--dry-run]
grab mart order [--yes <quote-id>] [--dry-run] [--notes TEXT] [--payment ID]
grab auth login [[--phone N] [--channel app|sms|whatsapp] [--dry-run | --poll | --otp-prompt]] [--otp CODE|-] [--poll [--timeout S]] [--cancel] [--cdp URL]
grab auth refresh
grab auth import [--cdp URL] [--from PATH | --address-only]
grab auth hydra
grab auth status
grab saved-places list [--country CODE]
grab saved-places use <n|poi-id|label|name> [--unit TEXT] [--note TEXT]
grab cache stats | clear [--kind KIND] [--expired] [--all] | prune | path | image <URL>
```

`FILTERS` are applied on the client, because Grab has no server-side sort or
filter: `--open`, `--min-rating X`, `--max-fee PESOS`, `--max-eta MIN`,
`--price-level N` (1-3), `--promo`, `--free-delivery`, `--cuisine NAME`,
`--max-distance KM`, `--type food|mart`,
`--sort relevance|rating|eta|fee|distance|price`, `--limit N` and `--pages N`.
`--pages` pulls more upstream pages, up to 10 and at least 3 s apart. On a 429
it stops and shows what it already has.

### Example: discovery

```
$ grab food nearby --open --min-rating 4.5 --sort eta --limit 2
2-EXAMPLEMERCH01  Pan de Manila - Example Branch  ★4.7 (335)  26 min  0.6 km  free delivery  ₱₱  OPEN  promo: 5% off
2-EXAMPLEMERCH02  Poke Joint - Example Branch  ★4.7 (586)  28 min  0.6 km  free delivery  ₱₱  OPEN  promo: 20% off
(2 merchants shown (31 fetched of 260); cached 3m ago; more available (--pages N))

$ grab food search sisig --dishes --max-price 200 --sort price --limit 2
2-EXAMPLEMERCH03-CZBBTYMDR8CYLT  Chicken Sisig Burger  ₱50.00  — Jayfia's Food Kiosk - Example Branch (2-EXAMPLEMERCH03)  55 min  9.3 km  ★4.3
PHITE202605160214032515399  Pork Sisig Served Hot  ₱83.75  — Baliwag Lechon Manok - Example Branch (2-EXAMPLEMERCH04)  45 min  4.1 km  ★4.6
(2 dishes shown (45 fetched of 597); cached 3m ago; more available (--pages N))

$ grab food category japanese --limit 1
PHGFSTI0000EXa1  Marugame Udon - Example Branch  ★4.6 (6104)  37 min  1.3 km  free delivery  ₱  OPEN  promo: 5% off
(1 merchants shown (23 fetched of 114); cached 3m ago; more available (--pages N))

$ grab food search adobo --local --dishes --limit 1     # offline, from the catalog
PHITE20210301080656260762  Amadobo Full  ₱335.00  — Recovery Food - Example Branch (PHGFSTI0000EXa2)  34 min  0.8 km  ★4.3
(1 dishes shown; local catalog, data up to 3m old)
```

`grab food cuisines` lists the cuisine ids (45 Chinese, 62 Fast Food, 63 Filipino,
89 Japanese, 96 Korean, 110 Mexican, 151 Seafood, 251 Promotions) and the home
collections near you. `category` accepts any of those ids, or a name or name
fragment.

Global flags (before the service): `--json`, `--no-cache`, `--refresh` (see
[Caching](#caching)).

### Example: price a meal

```
$ grab food search noodles
2-EXAMPLE123  Example Noodles - Downtown  [OPENED]  40 min  0.8 km  ★4.6

$ grab food menu 2-EXAMPLE123
Example Noodles - Downtown  OPEN  hours 11:00-22:00  eta 40 min  section SEC-EXAMPLE
  ITEM-1  ₱360.00  [Bowls] Karaage Bowl
      group GRP-SPICE "Spice level" (required, 1..=1)
        MOD-MILD Mild [default]
        MOD-HOT  Hot

$ grab food cart add 2-EXAMPLE123 ITEM-1 --qty 1 --mod GRP-SPICE:MOD-MILD
added 1x Karaage Bowl (ITEM-1)

$ grab food quote
Example Noodles - Downtown  (quote 0087763a88dd)
  subtotal           ₱360.00
  delivery            ₱82.00
  total              ₱442.00  (before promos ₱442.00)
  eta           40 min
  payment       123456789
  deliver to    <your saved address>
```

The quote always shows the delivery address (from your saved `location`) and the
payment method, and prints a short `quote-id` that stays the same while the
price is unchanged.

### Finding the cheapest voucher

```
grab food vouchers --best --apply
```

re-quotes with each eligible code (gently, one at a time) and stores the cheapest
in the cart.

### Placing an order (when enabled)

```
grab food order --dry-run                 # print the exact request, send nothing
grab food order --yes 0087763a88dd        # submit, only if it matches a fresh quote
```

With `allow_ordering = false` the last command prints what it *would* send
(tokens and address redacted) and exits non-zero (`ordering_disabled`).

### Tracking a live order

```
grab food track <order-id>              # one snapshot (human summary)
grab food track <order-id> --json       # stable, versioned JSON (see below)
grab food track <order-id> --watch      # re-poll every 30s, print on change
grab food track <order-id> --map out.png
```

`track` merges the live `/track` feed (state, driver position, ETA, remaining
route) with the order `/detail` snapshot (merchant, drop-off, items, total). The
`--json` output is a stable, versioned model — `"schema": 1` — whose fields are
additive across versions, so scripts can depend on them:

```jsonc
{
  "schema": 1,
  "order_id": "…", "booking_code": "A-…", "short_number": "…",
  "state": "FOOD_COLLECTED",      // raw Grab order state
  "human_state": "on the way to you",
  "is_final": false,              // COMPLETED / FAILED / CANCELLED*
  "cancelable": false,
  "stages": [{"task_type":"…","state":"…"}],
  "eta": {"min_minutes": 10, "max_minutes": 12},
  "driver": {"name":"…","vehicle_model":"…","vehicle_plate":"…",
             "service_type":"…","rating":5.0,"photo_url":"…",
             "location":{"latitude":…,"longitude":…},"bearing":…},
  "merchant": {"name":"…","address":"…","coordinates":{…}},
  "dropoff":  {"name":"…","address":"…","coordinates":{…}},
  "route": [{"latitude":…,"longitude":…}],  // remaining path to destination
  "route_distance_m": 1234.5,
  "total_php": 442.0,
  "items": [{"quantity":1,"name":"…"}],
  "web_url": "https://food.grab.com/ph/en/order/…"
}
```

`--map out.png` renders a static PNG: an OpenStreetMap basemap framing all known
points, the remaining route as a line, and pins for the restaurant (orange),
drop-off (blue) and driver (green). Tiles come straight from the OSM tile servers
(not the Grab client, so they never touch the Grab allowlist) with a descriptive
`User-Agent` and an on-disk cache, and carry the required
`(C) OpenStreetMap contributors` attribution. Only a handful of tiles are ever
fetched per render; if tiles are unavailable it falls back to a plain background.

In `--watch`, a `429` backs off (60s) instead of aborting the session.

### Syncing order history to notes

```
grab food history sync --vault /path/to/notes [--limit N]
```

Paginates completed orders (gently; `history` returns `429` if hammered) and
writes one markdown note per order under `<vault>/<YYYY>/`, plus a regenerated
`index.md` with spending stats. Notes are keyed by the API `bookingCode` (the
`A-…` code, which matches the receipt's booking code); it's idempotent, only
rewrites a note when its content changed, and preserves any text you add outside
the `<!-- grab:begin -->…<!-- grab:end -->` markers. Only `COMPLETED`, PHP-priced
orders are recorded.

## Error kinds

`--json` errors and exit codes let scripts react:

| kind | exit | meaning |
|---|---|---|
| `auth_missing` | 4 | no session; run `grab auth login` (or `grab auth import`). Also: GrabMart needs a one-time consent in the browser (the message says where) |
| `auth_expired` | 4 | 401, or no `gfc_session`, and silent refresh failed: the SSO is dead. Run `grab auth login` |
| `otp_invalid` | 4 | `grab auth login --otp`: wrong code; the login is still pending, try again |
| `captcha_required` | 4 | Grab wants a human step (captcha/selfie); log in in a normal window, then `grab auth import` |
| `login_expired` | 4 | no pending login, or it expired or its tab is gone; start again with `--phone` |
| `hydra_expired` | 4 | hydra token missing/rejected and couldn't be minted |
| `cdp_unavailable` | 4 | the browser CDP endpoint was unreachable |
| `quote_changed` | 3 | the fresh quote differs from the one you confirmed |
| `ordering_disabled` | 5 | `allow_ordering` is false |
| `confirmation_required` | 6 | order needs `--yes` or an interactive confirm |
| `cart_invalid` | 2 | empty cart or missing required modifiers |
| `address_missing` | 2 | no saved delivery address (`location` cookie) in the session; run `grab auth import --address-only` |
| `rate_limited` | 7 | Grab returned 429, or the login send budget (60 s / 3 per hour) is used; `details.retry_after_secs` |

## GrabMart

`grab mart …` browses GrabMart stores near your saved delivery address, finds
products (price, pack size, availability), keeps a local cart (separate from the
food cart) and prices it. Details and endpoints: [docs/mart.md](docs/mart.md).

- **Browsing needs the CDP browser, not a login.** The public mart API requires
  a Google reCAPTCHA v3 token per call. `grab` mints one in the browser at
  `cdp` (default `http://127.0.0.1:9222`) by evaluating `grecaptcha.execute` in
  a `mart.grab.com` tab (opened in the background if needed). Tokens are never
  printed or cached.
- **Quotes need a logged-in session** (`grab auth login`) and a saved delivery
  address, like food.
- Calls are paced ≥2s apart; the mart API answers 429 after ~10 quick calls.
  Browse results are cached under namespace `mart`.
- `grab mart order` has the same gates as food: `allow_ordering = true`,
  `--yes <quote-id>` matching a fresh re-quote (or interactive confirm), never
  retried.

```
$ grab mart search "S&R"
2-C6AZTLAEAKCFVT  S&R - Example Branch  [CLOSED_SCHEDULE_AVALIABLE]  1.0 km  Grocery Supermarket
    PHITE20260805062002014849   ₱159.00  S&R Spicy Tuna Roll 12pcs  [12pcs]  (available)
    …
$ grab mart find 2-C6AZTLAEAKCFVT "red bull"
294 matching items
PHITE20260805063620080508   ₱339.00  Red Bull Energy Drink 4 x 250ml  [4 x 250ml]  (available)
PHITE20260805062941037384   ₱339.00  Red Bull Energy Drink 4 cans  [4 cans]  (available)
…
$ grab mart cart add 2-C6AZTLAEAKCFVT PHITE20260805063620080508
added 1x Red Bull Energy Drink 4 x 250ml (PHITE20260805063620080508) at ₱339.00 each
$ grab mart quote            # subtotal, delivery, fees, discounts, total
```

## For assistants

See [SKILL.md](SKILL.md) for the exact workflow an assistant should follow —
including that it must never place an order without the user's explicit per-order
confirmation.

## Layout

```
crates/grabcli-core   shared: config, session, auth/hydra, HTTP allowlist, SQLite cache (responses, catalog, images), money, output
crates/grabfood       GrabFood API + `grab food` subcommands (discover/browse/index: listings, filters, catalog)
crates/grabmart       GrabMart API (reCAPTCHA via CDP) + `grab mart` subcommands
crates/grabcli        the `grab` binary; composes services
docs/hydra.md         the hydra device JWT: what it is and how we mint it
docs/checkout.md      the order-submission request, from reading the web app
docs/discovery.md     discovery endpoints (search/category/shortcuts/recommended) and how the CLI uses them
docs/mart.md          GrabMart endpoints, reCAPTCHA, cart/quote/order shapes
```

Built with Nix (`nix build`, `nix flake check`) and standard cargo.

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md). Report vulnerabilities privately as
described in [SECURITY.md](SECURITY.md).

## License

[Apache-2.0](LICENSE.txt)
