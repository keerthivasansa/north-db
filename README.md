# North

North is a small database engine written in Rust. It is designed for fast,
predictable data access through ordered query pipelines, immutable schemas,
mandatory primary keys, and B+ tree indexes.

North is early-stage software. The storage foundation is implemented; the table
catalog, B+ tree, query language, and crash recovery are in progress.

## Principles

- **Ordered pipelines:** query stages run in the order written.
- **Immutable schemas:** a table's structure never changes after creation.
- **Required identity:** every table has exactly one explicit primary key.
- **Fast storage primitives:** 8 KiB pages and stable row identifiers underpin
  heap storage and B+ tree indexes.
- **Deliberate scope:** joins and advanced text indexes are post-MVP work.

## Status

- Strict YAML configuration loading
- Fixed-size 8 KiB storage pages
- Stable `(PageId, SlotId, Generation)` row identifiers
- Variable-length slotted heap pages
- Insert, lookup, delete, slot reuse, and compaction
- Bounded LRU page cache with pinned guards and dirty writeback

See [ARCHITECTURE.md](ARCHITECTURE.md) for the on-disk layout, invariants, and
implementation roadmap.

## Configuration

Copy `north.example.yaml` to `north.yaml` and adjust the database path and cache
budget. Unknown YAML fields are rejected so misspelled settings cannot silently
fall back to defaults.

```sh
north --config north.yaml
```

Rust and Cargo are required to build North.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```
