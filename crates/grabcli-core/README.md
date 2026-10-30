# grabcli-core

Shared session, auth and HTTP plumbing for [grabcli](../../README.md).

This is an internal building block of the `grab` CLI, not a general-purpose
library. Its API is not stable.

## Modules

- `config`: `config.toml` settings, including the `allow_ordering` gate.
- `session`: saved Grab cookies and hydra token.
- `cdp`: Chrome DevTools Protocol client for importing a logged-in browser's
  session and minting hydra.
- `sso`, `login`: silent session refresh through the GrabID SSO cookie, and
  the browser-driven `grab auth login` ([docs/auth.md](../../docs/auth.md)).
- `cache`: the SQLite response cache, searchable catalog and image store, for
  read-only browse data only.
- `hydra`, `jwt`: the hydra device JWT ([docs/hydra.md](../../docs/hydra.md)).
- `http`: the HTTP client; every request is checked against a URL allowlist.
- `ctx`: the per-command `Context` that owns config, session and client, and
  re-mints hydra on rejection.
- `error`: one error type with stable kinds and exit codes.
- `location`, `money`, `output`, `paths`: delivery address, centavo amounts,
  human/JSON output, file locations.
- `places`: the account's saved places (one read-only GET) and the `location`
  cookie for one ([docs/places.md](../../docs/places.md)).

## How it fits

Service crates such as [`grabfood`](../grabfood) and
[`grabmart`](../grabmart) register their allowlisted URLs
and make every call through `Context`. The [`grabcli`](../grabcli) binary wires
the services together.

See the root README's [disclaimer](../../README.md#disclaimer).

## License

[Apache-2.0](../../LICENSE.txt)
