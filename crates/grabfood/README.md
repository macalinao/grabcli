# grabfood

GrabFood typed API for [grabcli](../../README.md): browse, menu, cart, quote,
vouchers, order, tracking and history, each returning plain structs.

This is an internal building block of the `grab` CLI, not a general-purpose
library. Its API is not stable.

## Modules

- `api`: endpoint URLs and the URL allowlist; the order-submission URL is only
  registered when ordering is enabled.
- `discover`, `browse`, `index`: search, nearby, recommended and category
  listings, their filters and paging, and catalog rows for the local cache
  ([docs/discovery.md](../../docs/discovery.md)).
- `menu`, `cart`, `quote`: menu parsing, the cart (a value the caller keeps),
  and price quotes.
- `order`: gated order submission ([docs/checkout.md](../../docs/checkout.md)).
- `tracking`: live order state as versioned JSON.
- `history`, `datetime`: export of completed orders to Markdown notes.

## How it fits

Built on [`grabcli-core`](../grabcli-core) for session, auth and HTTP. The
[`grabcli`](../grabcli) binary implements `grab food` on top of it. The `cli`
feature adds the bpaf parser for the list filters.

See the root README's [disclaimer](../../README.md#disclaimer).

## License

[Apache-2.0](../../LICENSE.txt)
