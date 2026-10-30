# grabmart

GrabMart API client for [grabcli](../../README.md).

This is an internal building block of the `grab` CLI, not a general-purpose
library. Its API is not stable.

## Modules

- `api`: public and protected endpoint URLs and the URL allowlist; the
  order-submission URL is only registered when ordering is enabled.
- `client`: `Mart::new(ctx)`; requests paced by an injectable `Pacer` (at
  least 2 s apart, one retry on `429`).
- `recaptcha`: the reCAPTCHA v3 token, minted in the browser over CDP and never
  printed or cached.
- `model`: stores, departments and products.
- `cart`, `quote`: the mart cart (a value, separate from food's) and price
  quotes.
- `order`: gated order submission, sharing GrabFood's confirmation gate
  ([docs/mart.md](../../docs/mart.md)).

## How it fits

Built on [`grabcli-core`](../grabcli-core) for session, auth (including
GrabMart's own GrabID web login), HTTP and the read-only browse cache, and reuses [`grabfood`](../grabfood)'s order gate. The
[`grabcli`](../grabcli) binary implements `grab mart` on top of it.

See the root README's [disclaimer](../../README.md#disclaimer).

## License

[Apache-2.0](../../LICENSE.txt)
