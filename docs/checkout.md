# The order-submission request

This documents the GrabFood "Place Order" request, reconstructed by **reading
the web app's JavaScript only** — grabcli's order path was written and
unit-tested against this, never by calling the endpoint. The submit URL is off
the HTTP allowlist unless `allow_ordering = true`.

## Endpoint

```
POST https://food.grab.com/proxy/foodweb/v2/order/orders
```

(When the app's `enableLocalProxy` flag is off the base is
`https://portal.grab.com/foodweb` instead; grabcli uses the proxy base, matching
the live site.) Defined in the web app's `common-utils` bundle as the
`createOrder` export, with a 30s timeout.

## Headers

Same as the other authenticated calls:

- `x-hydra-jwt` — the device JWT (see [hydra.md](hydra.md)).
- `x-gfc-session`, `x-gfc-country: PH`, `x-country-code: PH`.
- `x-grab-web-app-version` — the Next.js build id.
- `content-type: application/json;charset=UTF-8`.
- `X-GFC-GFB-Login: "false"` (the non-enterprise case).
- Cookies via the `Cookie:` header. (The web app also sends
  `Authorization: Bearer <accessToken>`; grabcli authenticates with the cookie
  jar + `x-gfc-session`, as the read-only calls do.)

## Body

Built by the web `createOrder` thunk (`De()` in the redux bundle):

```json
{
  "destination": {
    "address": "<saved address>",
    "keywords": "<saved address>",
    "coordinates": { "latitude": 0, "longitude": 0 },
    "addressDetail": "<saved addressDetail>",
    "poiID": "<saved poiID>"
  },
  "cartSignature": "<quoteSignature from the last /order/cart>",
  "supportComplexMenu": true,
  "deliveryNotes": "<saved noteToDriver>[; --notes]",
  "paymentTypeID": "<payment method id>",
  "isIntegrated": true,
  "countryCode": "PH"
}
```

Key points:

- **`cartSignature` is the `quoteSignature`** returned by `POST /order/cart`.
  The cart items, modifiers, promos, campaigns, comment and scheduled time are
  **not** in the order body — the server reconstructs them from the signature.
  So you must quote immediately before ordering.
- **There is no idempotency key / request id** in the body or headers. This is
  why grabcli never retries a submit: a retried POST could create a second
  order. On any ambiguous failure, check `GET /order/orders?...onlyOngoingOrders`
  instead.
- `isIntegrated` comes from the merchant (`merchant.isIntegrated`); grabcli sends
  the menu's value (true for integrated merchants).
- The web build has no tip or cutlery fields.
- `addressDetail` comes from the `location` cookie's `addressDetail` (unit,
  floor, building). `deliveryNotes` is the cookie's `noteToDriver`, with
  `--notes` appended after `; ` (it never replaces the saved note). Both are
  redacted in `--dry-run` / JSON `would_send` output.

## Saved address and payment

- The delivery address (text, `poiID`, lat/lng) comes from the browser's
  `location` cookie, which grabcli reads. No address API call is needed to order.
- The payment method is a `paymentTypeID`. The web app lists wallet methods via
  `GET /order/wallet/methods`; grabcli uses the id saved in config/session (shown
  on every quote), overridable with `--payment`.

## Response

```json
{ "orderID": "...", "orderType": "INTEGRATED", "orderMeta": { ... } }
```

(optionally an `orderSDKResp` for the Pay SDK, flattened to
`referenceID`/`orderSDKPayload`). The app then navigates to `/order/<orderID>`
and tracks it. grabcli reads `orderID`; absence of it is treated as "order not
confirmed".

## Errors

Proxy errors are `{target, reason, message}` or `{code, actionType, title,
message}`. Order-relevant codes:

| code | meaning |
|---|---|
| 1100 | item not available |
| 1102 | item price updated |
| 1103 | merchant closed |
| 1104/1105/1106 | promo/campaign invalid or exhausted |
| 1110/1111/1112 | payment invalid / cash not accepted |
| 1120–1123 | section/item/modifier failed |
| 1130/429001 | an existing ongoing food order |
| 1150 | signature expired (re-quote) |
| 1170 | delivery radius invalid |

Plain HTTP: 401 → re-login, 404 → not found, 429 → busy.

## Tracking

- `GET /order/orders/{id}/track` → `{bookingCode, ownerID, state, taskID,
  taskType}`. The web app polls this every 15s until a final state.
- `GET /order/orders/{id}` → `{order:{...}}` for full detail.
- Final states: `COMPLETED`, `FAILED`, and any `CANCELLED*`.
- Order history: `GET /order/history?input.limit&input.offset&input.onlyOngoingOrders`.
  The response carries both an internal `orderID` and a human `bookingCode`
  (e.g. `A-…`); the booking code is the stable public identifier.
