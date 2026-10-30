# grabfood

GrabFood API client and `grab food` subcommands for [grabcli](../../README.md).

This is an internal building block of the `grab` CLI, not a general-purpose
library. Its API is not stable.

## Modules

- `api`: endpoint URLs and the URL allowlist; the order-submission URL is only
  registered when ordering is enabled.
- `discover`, `browse`, `index`: search, nearby, recommended and category
  listings, their filters and paging, and catalog rows for the local cache
  ([docs/discovery.md](../../docs/discovery.md)).
- `menu`, `cart`, `quote`: menu parsing, the local cart, and price quotes.
- `order`: gated order submission ([docs/checkout.md](../../docs/checkout.md)).
- `tracking`, `map`: live order state as versioned JSON, and a static PNG map
  rendered from OpenStreetMap tiles.
- `history`, `datetime`: export of completed orders to Markdown notes.

## How it fits

Built on [`grabcli-core`](../grabcli-core) for session, auth and HTTP. The
[`grabcli`](../grabcli) binary mounts `FoodCmd` under `grab food` and calls
`run`.

See the root README's [disclaimer](../../README.md#disclaimer).

## License

[Apache-2.0](../../LICENSE.txt)
