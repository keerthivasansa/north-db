# North Architecture

This document is the technical contract for North's storage engine. It records
the on-disk format and invariants that future components must preserve.

## Scope and status

North is a single-process database engine. Its query language will execute an
ordered pipeline exactly as written; implementation may optimize an individual
stage, but must not change stage semantics.

Implemented today:

- YAML configuration parsing and validation
- 8 KiB raw page abstraction
- Fixed-width page IDs and row identifiers
- Variable-length slotted heap pages
- Database-file header and exact page I/O

Planned next:

1. Buffer pool
2. Table catalog and immutable row layouts
3. B+ tree primary and secondary indexes
4. North Query Language parser and executor
5. Write-ahead log and recovery

## Global storage invariants

| Property | Value |
| --- | --- |
| Database page size | 8,192 bytes |
| Integer byte order | Little-endian |
| Page identifier | Unsigned 32-bit integer |
| Slot identifier | Unsigned 16-bit integer |
| Slot generation | Unsigned 16-bit integer |
| Row identifier | 8 bytes: `(PageId, SlotId, Generation)` |

Page size, byte order, RID layout, and page-header layout are database-format
decisions. They are not user configuration settings.

## Raw page abstraction

Each physical page is exactly 8,192 bytes. `PageId(n)` maps to byte offset:

```text
n × 8192
```

The page layer performs checked reads and writes and explicitly encodes integer
values in little-endian order. It does not expose unchecked access or use
`unsafe` Rust.

## Database file and disk manager

Page zero is reserved for database-wide metadata. It is not a heap or index
page. `DiskManager` is the only current component that reads or writes the
database file; it performs full-page I/O and rejects reads or writes to page
zero and unallocated page IDs.

### Database header

The header occupies all of page zero. Bytes not assigned below are currently
zero and reserved for future compatibility.

| Offset | Size | Field | Meaning |
| ---: | ---: | --- | --- |
| 0 | 8 | magic | ASCII `NORTHDB\\0` |
| 8 | 2 | format version | Currently `1` |
| 10 | 2 | reserved | Zero |
| 12 | 4 | page size | `8192` |
| 16 | 4 | next page ID | First never-allocated data page ID |
| 20 | 4 | catalog root page ID | Reserved; `u32::MAX` means none |
| 24 | 4 | flags | Reserved, currently zero |
| 28 | 4 | checksum | Reserved, currently zero |

A new database has only its header page and `next_page_id = 1`. Allocating a
page writes a zeroed 8 KiB page at the high-water mark and then advances the
stored `next_page_id`. `DiskManager::sync` explicitly requests durable file
storage. Crash-safe allocation ordering and header checksums will arrive with
the write-ahead log.

Opening a file validates its page alignment, header magic/version/page size, and
that the allocation high-water mark does not point beyond the physical file.
Existing files are never overwritten by database creation.

## RID encoding

An RID identifies a row without depending on a logical row number:

```text
byte 0..4  PageId      u32, little-endian
byte 4..6  SlotId      u16, little-endian
byte 6..8  Generation  u16, little-endian
```

The generation makes a reused slot distinguishable from its prior occupant.
An RID is valid only when its page ID, slot ID, generation, and live state all
match the corresponding slot entry.

## Slotted heap page

A heap page stores variable-length row bytes. The header and slot directory grow
from the beginning of the page; row data is allocated from the end of the page.

```text
+-------------------------------+ offset 0
| 32-byte heap-page header      |
+-------------------------------+
| slot directory                | grows toward higher offsets
+-------------------------------+
| contiguous free space         |
+-------------------------------+
| variable-length row data      | grows toward lower offsets
+-------------------------------+ offset 8192
```

### Heap-page header

| Offset | Size | Field | Meaning |
| ---: | ---: | --- | --- |
| 0 | 4 | magic | ASCII `NHPG` |
| 4 | 1 | format version | Currently `1` |
| 5 | 1 | page kind | Heap page (`1`) |
| 6 | 2 | flags | Reserved, currently zero |
| 8 | 2 | slot count | Allocated slot entries |
| 10 | 2 | free start | First byte after the slot directory |
| 12 | 2 | free end | First byte of packed row data |
| 14 | 2 | live count | Number of live rows |
| 16 | 4 | next page ID | Heap-page link; `u32::MAX` means none |
| 20 | 8 | page LSN | Reserved for write-ahead logging |
| 28 | 4 | checksum | Reserved for checksums |

### Slot entry

Each allocated slot consumes 8 bytes immediately after the header.

| Offset within slot | Size | Field | Meaning |
| ---: | ---: | --- | --- |
| 0 | 2 | record offset | Start of row bytes in the page |
| 2 | 2 | record length | Row length in bytes |
| 4 | 2 | generation | Current slot generation |
| 6 | 2 | flags | Live, reusable, or retired |

Slot flags are:

```text
0  reusable deleted slot
1  live row
2  retired slot; it must never be reused
```

### Row lifecycle

1. Insert uses the first reusable deleted slot, otherwise it appends a new slot
   entry.
2. A row is written into contiguous free space from the page end downward.
3. Delete clears the row reference, increments the generation, and makes the
   slot reusable.
4. A stale RID cannot resolve after deletion because its generation no longer
   matches.
5. If a generation would overflow, the slot is retired instead of wrapping.
6. Compaction repacks live row bytes but leaves each live slot ID and generation
   unchanged, so all live RIDs remain valid.

### Validation on open

Opening a heap page validates:

- Magic, format version, and page kind
- Slot-directory and free-space boundaries
- Live-slot count
- Slot flags and nonzero live generations
- Row ranges, including page bounds and overlap

Invalid pages fail closed with a corruption error.

## Configuration

North uses strict YAML configuration. Unknown fields are rejected, relative
database paths are resolved relative to the YAML file, and the following invalid
combination is rejected:

```yaml
storage:
  create_if_missing: true
  read_only: true
```

The current configuration controls database path, cache budget, creation mode,
read-only mode, durability switches, and log level. It does not change the
on-disk layout.

## Module map

| Module | Responsibility |
| --- | --- |
| `config` | Strict YAML configuration and validation |
| `storage::page` | Fixed-size raw pages and checked byte access |
| `storage::rid` | Stable RID encoding |
| `heap::slotted_page` | Heap-page format, row lifecycle, and validation |

## Performance direction

North should turn its constrained model into predictable speed:

- Precompute immutable row layouts.
- Fuse pipeline stages unless an operation such as sort requires materialization.
- Decode only requested columns.
- Reuse buffers and avoid per-row allocation in scans.
- Use B+ tree lookups for primary-key predicates without changing pipeline
  semantics.
- Keep the initial concurrency model simple: multiple readers and one writer.

Benchmark primary-key lookups, sequential inserts, range scans, filtered scans,
updates, deletes, and database reopen behavior as the disk manager and indexes
arrive.
