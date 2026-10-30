# GrabFood discovery endpoints

Recon of food.grab.com (PH web, 2026-10). These calls are read-only. They are
all in the CLI allowlist (`crates/grabfood/src/api.rs`) and are implemented; see
[CLI implementation](#cli-implementation) below. Sample responses are in `docs/fixtures/discovery_*.json`.
The samples are trimmed: arrays are cut and request/decision IDs are redacted. The user lat/lng
is rounded to `1.23,103.8`.

## Auth and headers

Base: `https://portal.grab.com/foodweb/guest/v2`. This is the "guest" API. The
web app also has `/foodweb/v2/...` (logged-in) variants with the same shapes.

| Header | Required? |
|---|---|
| Cookies | yes. With no cookies the call returns **401**. A guest `passenger_authn_token` is enough, and a logged-in session works too. |
| `x-country-code: PH` | yes. Without it the call returns **403**. |
| `x-gfc-country: PH`, `accept-language: en`, `x-grab-web-app-version` | sent by the web; not needed in testing |
| `x-hydra-jwt` | sent by the web; **not needed** for these calls |
| `content-type: application/json` | for POST `/search` |

Limits and anti-bot:

- **Rate limit**: about 10-12 calls in quick succession return `429 {"reason":"rate_exceeded","message":"FW_RATE_EXCEEDED"}`.
  Space calls about 3-4 s apart when you page, and cache the results (the response cache fits this).
- **ALTCHA (latent)**: the web bundle can attach `X-ALTCHA-Payload` to the guest endpoints `search`,
  `category`, `category/shortcuts`, `recommended/merchants` and `merchants/{id}`. The trigger is
  `428 {"reason":"altcha_required"}` with an `x-altcha-challenge` header, or `400 altcha_rejected`.
  - The challenge comes from `GET {foodweb}/v1/altcha/challenge`. It is a SHA-256 proof-of-work: find
    `number <= maxnumber (~1e5)` with `sha256(salt + number) == challenge`.
  - It is feature-flagged (`altchaEnable`) and was **not enforced** in testing.
  - Treat a 428 as "back off and report" until it is implemented.

## 1. Search / nearby list: `POST /search`

```json
{"latlng":"1.23,103.8","keyword":"sisig","offset":0,"pageSize":32,"countryCode":"PH"}
```

These are the only fields the web ever sends. The CLI already uses this call (`grab food search`).

- **Nearby / "all restaurants"**: send `keyword: ""` (empty). This gave `totalCount` 260 for the test location.
- **Pagination**:
  - `offset` += page size. `hasMore` tells whether to continue.
  - Pages at 0/32/64 for the empty keyword had no overlap in testing.
  - On keyword searches, `totalCount` drifts between pages (349 vs 331) and a page can contain
    **duplicate merchant ids**, so dedupe by `id`.
- **Ignored**:
  - `pageSize`: 10 and 100 both return about 30. The count varies per call (17-52).
  - `sortBy`, `sortType`, `filters`, `filter`, and `categoryShortcutID` inside the body have no effect.
  - **There is no server-side sort or filter.**

Response, `searchResult`:

- `totalCount`, `hasMore`, `hasDishResult`
- `searchMerchants[]`:
  - `id`, `address.name`, `chainName`, `branchName`
  - `businessType`: `"FOOD"` or `"MART"`. Mart stores such as Watsons and 7-Eleven appear in food search too.
  - `estimatedDeliveryTime` (min), `estimatedDeliveryTimeRange`
  - `estimatedDeliveryFee{price, priceDisplay, hasDiscountedPrice, priceDisplayHtml}`. `price` is null for some merchants.
  - `merchantBrief`:
    - `cuisine[]`, `rating`, `vote_count`, `distanceInKm`, `priceTag` (1-3)
    - `openHours{open, displayedHours, ...}`
    - `promo{hasPromo, description}`
    - `isIntegrated`, `deliverBy`, `description`
  - `merchantStatusInfo.status` (already used by the CLI)
  - Labels: `sideLabels.data[]{type, displayedText}` (promo tags), `customLabel[]` (e.g. GRABUNLIMITED),
    `littleIconLabel[]` (e.g. "Top Service")
  - `reviews`
  - `branchMerchants[]` (same shape) and `branchPagination{moreMerchants[ids], retrievalMeta}` for chains
  - **`dishes[]`** (see below)
- `retrievalMeta` (base64) decodes to the detected intent: `isBrand/isCuisine/isDish/isMartItem/isMartMerchant/...`,
  with a 6000 m radius.

Fixtures: `discovery_search.json` (empty keyword) and `discovery_search_dishes.json` ("sisig").

### Dish-level search

There is no separate dish endpoint. When the keyword matches dishes,
`hasDishResult: true` and each merchant carries the matched items in `dishes[]`:

- `ID`, `name`, `available`, `priceInMinorUnit`, `discountedPriceInMin`
- `priceV2.amountInMinor`, `takeawayPriceInMin`
- `description`, `imgHref`, `merchantID`, `itemAttributes`

The `ID`s are the menu item IDs, so they can go straight into `grab food cart add`.

## 2. Cuisine / collection listing: `GET /category`

`?latlng=1.23,103.8&categoryShortcutID=89&offset=0&pageSize=32`

- Response:
  - `categoryName`, `categoryDescription`, `categoryImageURL`, `shortcutID`
  - `searchResult` (same shape as `/search`)
- Pagination works the same way: offset 32 of Japanese gave 32 merchants with `hasMore`.
- A cuisine with no PH merchants returns `searchResult.instruction` ("We can't find...") and no
  `searchMerchants`. One example is 755 "Mart", which is a GrabMart MY shortcut.
- Mart shopping-mission IDs also work here. For example, 23396 "Grocery Stores" returned 19 stores (see mart.md).

Cuisine IDs linked by the PH site:

| ID | Cuisine |
|---|---|
| 45 | Chinese |
| 62 | Fast Food |
| 63 | Filipino |
| 89 | Japanese |
| 96 | Korean |
| 110 | Mexican |
| 151 | Seafood |
| 251 | Promotions |

Web route: `/ph/en/cuisines/japanese-delivery/89`. There is also an untested list endpoint,
`GET {foodweb}/categories?latlng&countryCode&offset&pageSize`. Fixture: `discovery_category.json`.

## 3. Home collections: `GET /category/shortcuts`

`?latlng=1.23,103.8&countryCode=PH&cityID=4&poiID=`

- `cityID` 4 is Manila.
- Response `shortcuts[]`:
  - `id`, `name`, `description`, `imageURL`, `imageV2`
  - `action{actionType:"DEEPLINK", value:"grab://...campaignLandingV2ID=..."}`
- These are the home carousels, for example:
  - 25865 "Deal Drop Zone"
  - 25323 "Starts at ₱99"
  - 23865 "Grab 5-Star Eats"
  - 19182 "Only On Grab"
  - 22926 "GrabUnlimited Exclusive"
  - 5047 "Nearby"
- Feed a shortcut `id` into `/category?categoryShortcutID=` to list its merchants.

Fixture: `discovery_shortcuts.json`.

## 4. Recommended: `GET /recommended/merchants`

`?latitude=1.23&longitude=103.8&mode=&offset=0&countryCode=PH`

- Note that this endpoint takes separate `latitude`/`longitude`, not `latlng`.
- Response: `recommendedMerchantGroups[]{feature:"N", recommendedMerchants[]}`. Each merchant has:
  - `id`, `address.name`, `distanceInKm`
  - `merchantData{estimated_delivery_time, rating, vote_count, priceTag, promo, service_hours, isIntegrated, photo_href}`
- This is the "Popular near you" list: 8 merchants per call, and `offset=8` returns the next 8.

Fixture: `discovery_recommended.json`.

## Other endpoints seen in the JS (untested)

- `GET /guest/v2/merchants/{id}?latlng=`: the guest menu, same as the proxy menu.
  - For mart stores it returns `merchant:null, featureFlags.isMartMerchant:true`.
  - The food web then redirects to `mart.grab.com/merchant/{id}`.
- `GET {foodweb}/v2/chain?latlng&chainName&countryCode&skipDistanceFilter`: chain branches. `chainName=Jollibee` returned 404 "chain not found". The exact chain key is unknown.
- `{foodwebproxy}/v2/order/category` and `/v2/order/categories`: logged-in variants.
- `/v1/autocomplete` and `/v1/geo/search-poi`: autocomplete and POI search.

## Not available: do these client-side

- **Sort** by rating, distance, ETA, fee or price. Use `merchantBrief.rating`, `distanceInKm`,
  `estimatedDeliveryTime`, `estimatedDeliveryFee.price` and `priceTag`.
- **Filters**:
  - open now: `merchantBrief.openHours.open` / `merchantStatusInfo.status`
  - min rating
  - promo: `promo.hasPromo` / `sideLabels`
  - free or discounted delivery: `estimatedDeliveryFee.hasDiscountedPrice` / `priceDisplayHtml`
  - price level, max ETA, `businessType` FOOD vs MART
- A real "load all": page until `!hasMore`, dedupe by `id`, and respect the rate limit.

## CLI implementation

| Command | Endpoint | Cache kind (TTL) |
|---|---|---|
| `grab food search <kw>` | `POST /search` | list (10 min) |
| `grab food nearby` | `POST /search` with `keyword:""` | list |
| `grab food category <id\|name>` | `GET /category?categoryShortcutID=` | list |
| `grab food recommended` | `GET /recommended/merchants` (8 per page) | list |
| `grab food cuisines` | static ids above + `GET /category/shortcuts` | categories (24 h) |
| `grab food menu <id>` | proxy menu; `GET /guest/v2/merchants/{id}` if the session is expired | menu (30 min) |

- Code lives in `crates/grabfood/src`:
  - `discover.rs`: parsing, filters and sort.
  - `browse.rs`: commands, paging and fallback.
  - `index.rs`: catalog rows.
- **Filters and sort** run on the client, over every fetched page (`ListOpts`).
  `--max-fee` uses the fee after any discount. "Free" is parsed from
  `priceDisplayHtml`. Merchants with a missing value never pass a filter on that
  field, and they sort last.
- **Paging**:
  - `--pages N` (at most 10) fetches pages with `offset += returned count` until
    `hasMore` is false.
  - The pacer keeps live calls at least 3 s apart. Pages that are already
    cached cost no time.
  - Results are deduped by merchant `id`.
  - An error after the first page stops paging. The partial result is printed,
    and the JSON gets `"stopped": "<reason>"`.
- **Dishes**:
  - `search` shows each merchant's `dishes[]`. Grab also returns loosely
    related items, so items whose name or description contains the keyword
    rank first.
  - `--dishes` flattens the list and keeps only the dishes that match.
  - `--max-price` uses the effective (discounted) price.
- **Fallback**: if the first live page fails (429, network or API error), the
  command uses the newest stale cached response for the same key. Failing
  that, it uses the local catalog near the address. JSON reports `source` as
  `stale` or `local`, plus `fallback_reason`.
- **Catalog**:
  - Every live list page and menu is upserted into the SQLite catalog,
    including restaurants, chain branches, the locations they were seen from,
    menu categories and items.
  - `--local` queries the catalog by name, cuisine, chain or dish, with no
    network access. It uses FTS5 word/prefix matches first and trigram
    substring matches second. Results are limited to 6 km around the address (the radius Grab uses), or to `--max-distance`.
  - Catalog data has no live "open now" state, so treat `--open` with
    `--local` as best-effort.
- `category <name>` resolves against the static cuisine ids first and the
  cached collection names second (case-insensitive, substring).

Not implemented:

- ALTCHA. A 428 is surfaced as an `api` error.
- `GET {foodweb}/categories`.
- The chain endpoint.
- `/v1/autocomplete`.
