# GrabMart endpoints

Recon of mart.grab.com and its API (PH, 2026-10). Only read calls were made: nothing was added to
a cart and nothing was quoted or ordered. The cart and order shapes below come from the web bundle.
Sample responses are in `docs/fixtures/mart_*.json`. They are trimmed: request/decision IDs are
redacted, and the user lat/lng is rounded to `1.23,103.8`.

## Where it lives

- **Separate API**: `https://api.grab.com/martwebapi/v1/{public|guest|protected}`. It is *not* portal.grab.com/foodweb.
  - The protected base is also proxied same-origin as `https://mart.grab.com/martwebapi/v1/protected/...`.
  - The bundle picks `public`, `guest` or `protected` per call. Logged-in users use `protected`.
  - `useGuestBrowsing` is `[]` for PH, so anonymous PH browsing uses `public`.
- **No PH mart web UI**:
  - `mart.grab.com/ph/en` is a marketing landing page.
  - `/ph/en/merchant/{id}` redirects to the landing page or to SG.
  - The food web shows an empty page for mart merchants: the food guest `merchants/{id}` returns
    `merchant:null, featureFlags.isMartMerchant:true`.
  - The **PH data is fully served by the API** with PH coordinates.
- Mart stores also show up in **food** `/search` (with `businessType:"MART"`) and in food `/category` with
  the shopping-mission IDs below, so they can be discovered without the mart API.
- IDs:
  - Merchant IDs look like `2-C221R641CXWGEE`, the same namespace as food.
  - Item IDs look like `PHITE2026...`.
  - Department IDs look like `PHITEDP...`. Sub-departments are `{merchantID}-{PHITEDP...}`.

## Auth and headers

| Header | Required? |
|---|---|
| `X-Recaptcha-Token` | **yes for `public`**. Without it the call returns `403 {"message":"Recaptcha failed"}`. |
| `X-Country-Code: PH` | yes |
| `Accept-Language: en` | sent |
| `X-Grab-Web-App-Version`, `X-Device-ID` | sent by the web |
| `X-Hydra-JWT` | sent on `protected` calls (cart, orders) when `enableHydraJWTHeader` is set |
| Cookies | needed for `protected`. They are scoped to mart.grab.com/api.grab.com (Grab SSO). |

About the reCAPTCHA token:

- It is a Google **reCAPTCHA v3** token, minted with `grecaptcha.execute(siteKey, {action})`.
  - The site key is public and is in the mart page config: `6LfnMFIfAAAAAGCE6O5Kscpsg0ATxuLrmbxtnXCc`.
- The action equals the endpoint name:
  - `getFeed`, `getShopFeed`, `getMerchant`, `searchMerchant`, `searchInStore`, `getMerchantItems`
  - `getItemCategory`, `getRecommendation`, `getItemDetail`, `getPlaces`, `detectLocation`
  - `getShopMissionLandingPage`, `getShopMissionCategoryTreeItems`
- Tokens are single-use and short-lived (~2 min).
- The site key is bound to mart.grab.com, so a plain HTTP client cannot mint them.

### How grabcli gets past reCAPTCHA (2026-10-17)

Three options were evaluated:

| Option | Result |
|---|---|
| (a) `protected` base with the logged-in session + `X-Hydra-JWT` | The bundle never attaches a reCAPTCHA token to `protected` calls (only `public`/`guest`), so it *should* skip reCAPTCHA. **Could not be verified**: the saved web login had expired (`grab auth refresh` → 401), and `protected/homefeed` returned `401` with an empty body both on `api.grab.com` and on the `mart.grab.com` proxy. `guest` returns `403 Recaptcha failed` like `public`. |
| (b) Mint tokens in the CDP browser | **Works, no login needed.** Evaluating `grecaptcha.execute(siteKey, {action})` in a `mart.grab.com` tab (the PH landing page redirects to `/sg/en`, which is fine: same origin) returns a token that the Rust client then sends as `X-Recaptcha-Token` from outside the browser. `public` search, store, department, in-store search and item detail all returned 200 this way. |
| (c) Serve mart stores through the food API | Not viable. Food guest `merchants/{id}` returns `merchant:null` (`isMartMerchant:true`) for mart stores, the logged-in food `/order/merchants/{id}` was untestable (expired login), and food guest `/search` now answers 401 without cookies. |

So `grab mart` uses **(b)** for every public call: `crates/grabmart/src/recaptcha.rs`
evaluates the mint expression through `grabcli_core::cdp::evaluate_in_origin`,
which reuses an existing `mart.grab.com` tab or opens one in the background (and
leaves it open for later calls). One token is minted per uncached request;
tokens are never printed, logged or cached. The quote uses the `protected` base
(no reCAPTCHA) and so needs a logged-in GrabMart session (see "Quote" below). Once a login is available,
(a) should be re-tested: if `protected` browse calls work without a token,
browsing could drop the browser dependency for logged-in users.

Other notes:

- Coordinates are **`latitude` + `longitude`** on most calls, and `latlng` on `/merchants/...` and `/category`.
  Using the wrong one gives `400 {"message":"invalid coordinates"}`.
- Rate limit: there was a 429 after a burst of about 10 calls, and once after only
  three calls within ~5 s (in-browser). `grab mart` spaces calls ≥2 s apart
  (across processes, via `<state>/mart/last-call`), retries a 429 on a read once
  after 10 s, then reports `rate_limited`. With that pacing, a dozen consecutive
  live calls went through without a 429.

All paths below are relative to `https://api.grab.com/martwebapi/v1/public` and are `GET` unless noted.

## Home feed: `/homefeed`

`?latitude=1.23&longitude=103.8&offset=0&size=20`

Response: `{hasMore, nextOffset, feeds[]}`. Each feed has a `type`:

- `CATEGORY`: `categoryWidget.categories[]{id, name, imageURL}`. These are **shopping missions** (store categories):
  - 8300 Stores Near You, 23396 Grocery Stores, 23395 Pharmacies, 23399 Convenience Stores
  - 23402 Fresh & Frozen, 23400 Beauty, 23398 Pet Supplies, 24098 Mom & Baby
  - 24099 Home & Living, 24096 Electronics, 23401 Flowers
  - 19675 Weekend Price Drop
  - `MART_ENTRY`
- `ITEMCATEGORY`: `itemCategoryWidget.categories[]` are cross-store product departments, for example
  `PHITEDP20200716054649016835` "Pantry Supplies". They go to `/item/categories`.
- `RECOMMENDATION`: `recommendationWidget.recommendations[]` (merchant plus items).
- `MERCHANT`: `merchantWidget.merchants[]` is "More stores". Page it with `offset=nextOffset`.
- `ADVERTISE`: banners.

Fixture: `mart_homefeed.json`.

## Stores in a category: `/category`

`?categoryShortcutID=23396&latlng=1.23,103.8&offset=0&pageSize=20`

- Response: `{categoryName, categoryDescription ("tag:SHOPPING_MISSION"), searchResult{totalCount, searchMerchants[]}, sortAndFiltersV2}`.
- Grocery Stores returned 19.
- The same call also works on food guest `/category`.

Fixture: `mart_category.json`.

## Store search and cross-store product search: `/search`

`?keyword=shampoo&latitude=1.23&longitude=103.8&offset=0&size=20`

- Response: `searchResult{numItems, searchMerchants[], nextOffset, hasMore, hasDishResult}`, `sortAndFiltersV2`.
- Each merchant has:
  - `id`, `address{name, combined_address, city}`, `latlng`, `chainID`, `chainName`, `businessType:"MART"`
  - `estimatedDeliveryTime`, `estimatedDeliveryFee{price, priceDisplay}`
  - `merchantBrief` (same shape as food, plus `hasPromotion`), `merchantStatusInfo.status`
  - `highlights[]` (promo tag title/subtitle), `branchMerchants[]`, `matchedByItem`
  - **`dishes[]` = matched products**: `ID`, `name`, `priceInMinorUnit`, `discountedPriceInMin`, `priceV2`,
    `barcode`, `soAvailable`, `merchantID`
- This is the **"find a product across stores"** call. "shampoo" returned Watsons, Southstar, 7-Eleven, Alfamart and others, each with matching items.
- The bundle also passes `filters`, but `sortAndFiltersV2` came back as just `{"possibility":true}`.
  So there are **no sort or filter options in practice**.
- The minimum keyword length for PH is 3 (`search_english_min_keyword_length_limits`).

Fixture: `mart_search.json`.

## Store page: `/merchants/{id}`

`?latlng=1.23,103.8`

`merchant` has:

- Basics: `ID`, `name`, `cuisine` (e.g. "Pharmacy"), `ETA`, `status`, `distanceInKm`, `latlng`, `address`, `currency`
- Ratings and hours: `rating`, `voteCount`, `openingHours`, `sectionOpenHours`
- Delivery: `estimatedDeliveryFee`, `deliveryOptions` (`DELIVERY`, `TAKEAWAY`), `deliverBy`
- `schedulerOrderConfig`: timeslots
- `outOfStockInstructions` (`CONTACT`, `SPECIFIC_ITEM`, `REMOVE`): a mart-only checkout option
- `merchantShareLink`
- `menu`:
  - `menuMeta.orderValueLimit`, `campaigns[]`, `menuItemType`, `stackableOfferOption`
  - `departments[]`:
    - `ID`, `name`, `available`, `sortOrder`, `hasMore`, `supportSearch`, `items[]` (first ~10 only)
    - `departmentType`: `1` = synthetic "Discounted items" (`mockTodayOfferDepartID`), `2` = "For You" (`dynamicCategoryID`), others are real departments
- Items:
  - `ID`, `name`, `available`, `priceInMinorUnit`, `discountedPriceInMin`, `discountPercentage`
  - `priceV2` / `discountedPriceV2`, `campaigns[{discountToken}]`
  - `barcode`, `itemClassID`, `searchMeta.departmentID`, `images`, `description`
- There are **no `modifierGroups`** on the items sampled. The menu is a flat catalogue.
- There is **no `section`** field in the public response. The cart body needs one: look for it in the
  `protected` merchant response, or reuse food's `/order/merchants/{id}`, which was not tested because the CLI session had expired.

Fixture: `mart_merchant.json`. It is trimmed from 325 KB.

## Department listing: `/merchants/{id}/items`

`?merchantID={id}&latlng=1.23,103.8&parentID=<department ID>&departmentType=<n>&subDepartmentID=&poiID=&offset=0&size=20`

- Use `parentID`, not `departmentID`. With `departmentID` the list comes back empty.
- Response: `{hasMore, nextOffset, items[], subDepartments[{id, name, sortOrder}], sortAndFiltersV2}`.
- To drill down, pass a `subDepartments[].id` as `subDepartmentID`.

Fixture: `mart_items.json`.

## In-store product search: `/merchants/{id}/search`

`?merchantID={id}&latlng=1.23,103.8&keyword=sunscreen&offset=0&size=20`

Response: `{hasMore, nextOffset, totalCount, items[], subDepartments, sortAndFiltersV2}`. "sunscreen" at Watsons gave a `totalCount` of 276.

Fixture: `mart_instore.json`.

## Product detail, price and stock: `/item/{itemID}`

`?latitude=1.23&longitude=103.8&poiID=` (reCAPTCHA action `getItemDetail`). There is no `merchantID` param.

- Response: `{item, merchant}`. `item` has the listing fields plus `discountedPriceV2`,
  `parentItemClassID` (`{merchantID}-{department}`) and `itemAttributeGroupsV2[].attributes[]`
  (`Brand`, `Size` such as "About 4 x 250ml", `Flavors`, `Packaging Format`, ...).
- `merchant` is a short store summary (name, hours, campaigns).
- Price and availability are also present in every listing: `priceInMinorUnit`, `discountedPriceInMin`,
  `available` / `soAvailable`.
- **No stock counts are exposed**, only availability booleans.
- In department pages (`/merchants/{id}`), `departments[].items[]` lacks the discounted price;
  `departments[].elementCards[].item` has it.

Fixture: `mart_item.json` (S&R Example Branch, Red Bull 4 x 250ml) and `mart_instore_sr.json` (S&R, "Red Bull").

## Untested or failing endpoints

- `/item/categories?latitude&longitude&categoryID&subCategoryID&poiID&offset` is the cross-store department page (stores plus subcategories). It returned `400 "Something went wrong"` with the params tried.
- `/recommendations?latitude&longitude&offset&size` also returned 400.
- These were not tried:
  - `/landing-pages/{slug}`, `/category-tree/{slug}/items` (shopping-mission landing)
  - `POST /shopfeed`
  - `/poi/search`, `/poi/reverse-geo`, `/configs`, `/rewards/promotions/{code}`
- Protected endpoints, not called:
  - `/wallet/methods`, `/orders/history`, `/history-orders`
  - `/orders/{id}`, `/orders/{id}/track`, `/cancelreasons`
  - `POST /shopping-cart/link`

## Cart / quote / order compared with GrabFood

These come from the bundle. Only the quote was attempted (401: expired login); orders were never called.

| | GrabFood (CLI today) | GrabMart |
|---|---|---|
| Quote | `POST food.grab.com/proxy/foodweb/v2/order/cart` | `POST mart.grab.com/martwebapi/v1/protected/cart` (+ `X-Hydra-JWT`) |
| Quote body | `{merchants:[{ID, items, section}], paymentTypeID, promoCode:[{promoCode}], campaignIDs, countryCode, destination{latitude, longitude, address, poiID, ...}}` | Same `merchants`/`paymentTypeID`/`destination`/`campaignIDs`. `promoCode[]` carries `{offerID, promoCode, promoCodeID, promoCodeUUID, redemptionID, redemptionUUID}`. Optional `cartFlags`, `eInvoice*`. |
| Quote response | `quoteSignature`, `totalQuote`, `deliveryQuote`, `breakdowns` | `quoteSignature`, `totalQuote{reducedPriceV2, taxV2}`, `deliveryQuote.reducedPriceInMinorUnit`, `merchants[0].items`, `promoCodes`, `campaigns` |
| Order | `POST .../order/orders` `{cartSignature, destination{coordinates,...}, paymentTypeID, supportComplexMenu, deliveryNotes, ...}` | `POST .../protected/orders` with the same body **plus `businessType:"MART"`** and `mpaDetail` (out-of-stock preference). Returns `orderID`. |
| Idempotency | none | none |

The quote body, exactly as the web builds it (checkout chunk `4683-*.js`):

```json
{
  "merchants": [{"ID": "<store id>", "items": [{"ID": "<item id>", "quantity": 1}], "section": "<merchant.section>"}],
  "paymentTypeID": "<id>",
  "destination": {"latitude": 0, "longitude": 0, "address": "<saved address>", "poiID": "<saved poiID>"},
  "promoCode": [],
  "campaignIDs": ["<merchant.menu.campaigns[].ID>", "<item campaignID>"]
}
```

Items only carry `modifierGroups`/`comment` when set. `section` comes from the merchant response;
the public response has none, so grabcli sends `""` (to be checked against a logged-in quote).
The order body is `{deliveryNotes, destination{coordinates{latitude, longitude, poiID}, address,
addressDetail, poiID}, paymentTypeID, cartSignature, businessType:"MART", supportComplexMenu:true,
mpaDetail}`. As for food, `addressDetail` and the saved driver note come from the `location`
cookie, and `--notes` is appended to the note.

How grabcli implements it (`crates/grabmart`):

- `protected/cart` is allowlisted; `protected/orders` only when `allow_ordering = true`
  (no other protected endpoint is allowlisted).
- **The protected API needs GrabMart's own web login, not food's `gfc_session`.** mart.grab.com
  has its own GrabID OAuth client: `GET mart.grab.com/grab-id/login` → 302 to
  `partner-api.grab.com/grabid/v1/oauth2/authorize` (scope `openid profile.read martweb-protected`,
  `redirect_uri=https://mart.grab.com/grab-id/token`, PKCE) → the callback sets mart's session
  cookies on mart.grab.com. The web app then sends `protected` calls with `credentials: include`
  (those cookies plus the `.grab.com` ones), `X-Grab-Web-App-Version`, and `X-Hydra-JWT` when
  enabled. With only food's login, `protected/cart` answers 401 (seen live 2026-10-17).
- grabcli does that round trip silently with the SSO cookie (`sso::Service::Mart`, the same code as
  food's refresh): before a quote when there is no mart session, and once more after a 401. It
  never sends an OTP or push.
- **One-time consent:** for an account that never used the mart web client, `authorize` redirects to
  `weblogin.grab.com/consent`. That is a grant only the user should make, so grabcli stops with
  `auth_missing` and asks you to open `https://mart.grab.com/grab-id/login` in the logged-in
  browser and approve it once. After that the silent login works. (Observed 2026-10-17; the quote
  itself could not be tested past this point.)
- `grab mart quote` re-fetches the store page live for campaigns/section, posts the body with
  `X-Hydra-JWT`, and parses `breakdowns[]` into subtotal, fees (service, small-order, ...) and
  discounts, plus `deliveryQuote` and `totalQuote.reducedPriceV2`. Never cached.
- `grab mart quote --dry-run` prints the redacted body without calling Grab.
- `grab mart order` uses food's gate (`--yes <quote-id>` matching a fresh re-quote, or an interactive
  confirm), refreshes the session first, and never retries the submit.
- Whether food `/order/cart` accepts mart merchants is still **unknown**.

## CLI mapping

| Command | Endpoint (action) | Cache |
|---|---|---|
| `grab mart stores` | `/homefeed` MERCHANT widget (`getFeed`) | list |
| `grab mart stores --category ID` | `/category` (`getCategory`) | list |
| `grab mart stores --keyword K`, `grab mart search K` | `/search` (`searchMerchant`) | list |
| `grab mart categories` | `/homefeed` CATEGORY + ITEMCATEGORY (`getFeed`) | categories |
| `grab mart store ID` | `/merchants/{id}` (`getMerchant`) | menu |
| `grab mart items ID --department D [--sub S]` | `/merchants/{id}/items` (`getMerchantItems`) | menu |
| `grab mart find ID K` | `/merchants/{id}/search` (`searchInStore`) | list |
| `grab mart item ID ITEM`, `cart add` | `/item/{item}` (`getItemDetail`) | menu |
| `grab mart quote` | `POST protected/cart` (+ live `/merchants/{id}`) | never |
| `grab mart order` | `POST protected/orders` (gated) | never |
