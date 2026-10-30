# grabcli — notes for agents

Command-line client for Grab (GrabFood first). Binary: `grab`. Rust workspace.

## Stack
- Workspace: `crates/grabcli-core` (shared), `crates/grabfood` + `crates/grabmart` (services), `crates/grabcli` (the `grab` binary).
- tokio, reqwest (rustls, gzip; cookies built manually), bpaf (derive) for args, serde, rsa (hydra), sha2 (quote id, image hashes), tokio-tungstenite (CDP), rusqlite (bundled SQLite + FTS5) for the cache.
- Money is integer minor units (centavos); display `₱`.

## Commands
- `cargo build`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`.
- `nix build .#grab`, `nix flake check`, `nix fmt`.

## Safety (do not weaken)
- Order submission URL is only allowlisted when `allow_ordering = true` in config.
- `grab food order` / `grab mart order` require `--yes <quote-id>` matching a fresh re-quote, or an interactive confirm.
- Never retry the submit endpoint (no idempotency key); check `grab food orders --ongoing`.
- Never print/log/commit cookies, tokens or card details. `session*.json` is gitignored.
- The cache (core `cache/`: SQLite responses, catalog, images) is for read-only browse data only. No `CacheKind`, catalog table or image may come from carts, quotes, vouchers, orders, tracking/history or auth. The cart→quote→order path must fetch live (`MenuSource::Live` / `CacheMode::Off`). Image fetches send no cookies and are limited to https `*.grab.com`.

## Layout
- core: config, session, hydra (docs/hydra.md), http allowlist, money, output, ctx, sso (silent refresh), login (`grab auth login` in an isolated browser context; docs/auth.md), places (saved places: read-only GET + local `location` cookie, docs/places.md).
- core `cache/`: `mod.rs` holds the response cache API: `ctx.cached_call(_meta)` / `Cache::get_or_fetch(_meta)` and `CacheKey::new(ns, kind, endpoint).param(..).location(..)`. Keep that API source-compatible; mart uses it too. `db.rs` has the schema and migrations: append to `MIGRATIONS`, never edit a shipped one. `catalog.rs` has the restaurants/locations/menu tables with FTS5 and trigram search. `images.rs` is the content-addressed LRU image store.
- grabfood: api (endpoints+allowlist), discover (list parsing, filters, sort), browse (search/nearby/recommended/category commands, paging, fallback), index (catalog rows), menu, cart, quote, order (docs/checkout.md, docs/discovery.md).
- grabmart: api (public/protected bases, allowlist; `protected/orders` only with `allow_ordering`), recaptcha (v3 token minted in the CDP browser via core `cdp::evaluate_in_origin`; never print/cache tokens), client (pacing ≥2s, 429 → one retry), model, cart (`<state>/mart/cart.json`), quote, order (reuses `grabfood::order::gate`). Cache namespace `"mart"`. docs/mart.md.
