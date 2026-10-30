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

This crate is thin: it parses arguments, owns the `grab auth`,
`grab saved-places` and `grab cache` commands, and mounts each service's commands (`grab food` from
[`grabfood`](../grabfood), `grab mart` from [`grabmart`](../grabmart)).
Shared session, auth and HTTP code lives in [`grabcli-core`](../grabcli-core).

## License

[Apache-2.0](../../LICENSE.txt)
