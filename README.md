# North

North is a small database engine written in Rust. It is designed around ordered
query pipelines, immutable table schemas, mandatory primary keys, 8 KiB pages,
stable row identifiers, and B+ tree indexes.

North is currently an early implementation. Configuration, storage-format
primitives, and variable-length slotted heap pages are implemented.

## Fixed format decisions

- Database page size: 8,192 bytes
- Integer encoding: little-endian
- Row identifier: 8-byte `(PageId, SlotId, Generation)` value
- `PageId`: unsigned 32-bit integer
- `SlotId`: unsigned 16-bit integer
- `Generation`: unsigned 16-bit integer
- Table schemas are immutable
- Every table must have exactly one primary-key column
- B+ tree is the default and only MVP index

These values are part of the database format and are intentionally absent from
the YAML configuration.

## Heap page layout

Each heap page contains a 32-byte header, an 8-byte entry for every allocated
slot, contiguous free space, and row bytes packed from the end of the page.

```text
+--------------------------+ offset 0
| 32-byte page header      |
+--------------------------+
| 8-byte slot entries      | grows downward
+--------------------------+
| contiguous free space    |
+--------------------------+
| variable-length rows     | grows upward
+--------------------------+ offset 8192
```

Deleting a row immediately increments its slot generation, invalidating the old
RID. A deleted slot is reused before the directory grows. Slots that exhaust the
16-bit generation space are permanently retired rather than wrapped. Compaction
moves row bytes but preserves slot IDs and live RIDs.

## Configuration

Copy `north.example.yaml` to `north.yaml` and adjust the database path and cache
budget. Unknown YAML fields are rejected so misspelled settings cannot silently
fall back to defaults.

```sh
north --config north.yaml
```

Rust and Cargo are required to build North.
