# grabcli — notes for agents

Command-line client for Grab (GrabFood first). Binary: `grab`. Rust workspace.

## Stack
- Workspace: `crates/grabcli-core` (shared), `crates/grabfood` + `crates/grabmart` (service libraries), `crates/grabcli-sqlite` (the SQLite cache backend), `crates/grabcli` (the `grab` binary).
- Library crates (core, grabfood, grabmart) own no storage: no paths, config-file reads, env, printing or rusqlite. Embedders call `Context::new(config, Arc<dyn SessionStore>, Arc<dyn Cache>)` (both traits async). Files (`session.json`, carts, login state, hydra key) and the SQLite cache are wired up in the bin crate (`crates/grabcli/src/local.rs`).
- tokio, reqwest (rustls, gzip; cookies built manually), bpaf (derive) for args, serde, rsa (hydra), sha2 (quote id, image hashes), tokio-tungstenite (CDP), rusqlite (bundled SQLite + FTS5) for the cache, in `grabcli-sqlite` only.
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
- core: config, session, store (`SessionStore`), hydra (docs/hydra.md), http allowlist, money, ctx, sso (silent refresh), login (step functions + `LoginDriver` with an injectable `SendBudget`; the CLI keeps the pending login and send log as files; docs/auth.md), places (saved places: read-only GET + local `location` cookie, docs/places.md).
- core `cache/`: the `Cache` backend trait, `CacheHandle` (TTL/mode logic: `ctx.cached_call(_meta)`, `get_or_fetch(_meta)`, `CacheKey::new(ns, kind, endpoint).param(..).location(..)`), catalog row/query types, and `MemoryCache`/`NoCache`.
- grabcli-sqlite: `SqliteCache` implements `Cache`, plus stats/clear/prune for `grab cache`. `db.rs` has the schema and migrations: append to `MIGRATIONS`, never edit a shipped one. `catalog.rs` has the restaurants/locations/menu tables with FTS5 and trigram search. `images.rs` is the content-addressed LRU image store.
- grabfood: typed API in `lib.rs` (menu, cart_add, quote, vouchers, prepare/submit/place_order, orders, track, history); api (endpoints+allowlist), discover (list parsing, filters, sort; bpaf parser behind the `cli` feature), browse (typed listings, paging, fallback), index (catalog rows), menu, cart (a value), quote, order (docs/checkout.md, docs/discovery.md).
- grabmart: api (public/protected bases, allowlist; `protected/orders` only with `allow_ordering`), recaptcha (v3 token minted in the CDP browser via core `cdp::evaluate_in_origin`; never print/cache tokens), client (`Mart::new(ctx)`, injectable `Pacer` ≥2s, 429 → one retry), model, cart (a value), quote, order (reuses `grabfood::order::gate`). Cache namespace `"mart"`. docs/mart.md.
- grabcli (bin): `local.rs` (config, `FileSessionStore`, send budget, pending login, hydra key, cache), `output.rs`, `paths.rs`, `food/` and `mart/` (bpaf enums, printing, cart files at `<state>/{food,mart}/cart.json`, the tracking map, file-backed mart pacing), auth, places, cache.
