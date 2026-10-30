# grabcli

The `grab` binary: an unofficial command-line client for Grab, starting with
GrabFood and GrabMart.

## Install

```
nix build .#grab
# or
cargo install --path crates/grabcli
```

Commands, auth setup and the ordering safety model are documented in the
[root README](../../README.md). Read its [disclaimer](../../README.md#disclaimer)
first.

## How it fits

This crate parses arguments, prints, and owns the files: `config.toml`,
`session.json`, carts, login state and the SQLite cache
([`grabcli-sqlite`](../grabcli-sqlite)). Its `grab food` and `grab mart`
commands call the typed [`grabfood`](../grabfood) and
[`grabmart`](../grabmart) libraries. Shared session, auth and HTTP code lives
in [`grabcli-core`](../grabcli-core).

## License

[Apache-2.0](../../LICENSE.txt)
