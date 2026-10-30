# grabcli-sqlite

The SQLite cache backend for [grabcli](../../README.md): `SqliteCache`
implements [`grabcli-core`](../grabcli-core)'s `Cache` trait with one database
(WAL, mode 0600) holding cached responses, a restaurant/menu catalog with FTS5
and trigram search, and a content-addressed image store with LRU eviction.

This is an internal building block of the `grab` CLI, not a general-purpose
library. Its API is not stable. It is the only crate that links `rusqlite`.

See the root README's [disclaimer](../../README.md#disclaimer).

## License

[Apache-2.0](../../LICENSE.txt)
