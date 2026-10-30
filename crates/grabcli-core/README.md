# grabcli-core

Shared session, auth and HTTP plumbing for [grabcli](../../README.md).

This is an internal building block of the `grab` CLI, not a general-purpose
library. Its API is not stable.

## Modules

- `config`: `config.toml` settings (parsed from text), including the
  `allow_ordering` gate.
- `session`, `store`: Grab cookies and hydra token, and the async
  `SessionStore` trait the context loads and saves them through.
- `cdp`: Chrome DevTools Protocol client for importing a logged-in browser's
  session and minting hydra.
- `sso`, `login`: silent session refresh through the GrabID SSO cookie, and
  the browser-driven `grab auth login` ([docs/auth.md](../../docs/auth.md)).
- `cache`: the async `Cache` backend trait (responses, catalog, images) and
  `CacheHandle` (TTLs, modes), for read-only browse data only. The SQLite
  backend is [`grabcli-sqlite`](../grabcli-sqlite).
- `hydra`, `jwt`: the hydra device JWT ([docs/hydra.md](../../docs/hydra.md)).
- `http`: the HTTP client; every request is checked against a URL allowlist.
- `ctx`: `Context::new(config, store, cache)`: owns config, session and
  client, saves session changes through the store, and re-mints hydra on
  rejection.
- `error`: one error type with stable kinds and exit codes.
- `location`, `money`: delivery address, centavo amounts.
- `places`: the account's saved places (one read-only GET) and the `location`
  cookie for one ([docs/places.md](../../docs/places.md)).

## How it fits

Service crates such as [`grabfood`](../grabfood) and
[`grabmart`](../grabmart) register their allowlisted URLs
and make every call through `Context`. The library owns no storage: no paths,
config files, environment or printing. The [`grabcli`](../grabcli) binary
wires files, the SQLite cache and the services together.

See the root README's [disclaimer](../../README.md#disclaimer).

## License

[Apache-2.0](../../LICENSE.txt)
