# 0. Principles and conventions

## The AMR cell model

Adaptive mesh refinement covers a domain with coarse cells and recursively
subdivides a cell only where finer resolution is needed. 5FS applies this one
idea to four different domains:

| Level | Domain | Coarse cell | Refines into | Spec |
|---|---|---|---|---|
| Device map | the block device | one contiguous mapping per AG | sub-range mappings (incremental relocation, holes) | [1-layout.md](1-layout.md) |
| Allocator | an AG's block space | 1 MiB allocation cells | sub-cells down to inode slots | [2-allocation.md](2-allocation.md) |
| Extents | a file's offset space | one large contiguous extent | finer extents where writes are fine-grained; holes stay unmapped | [4-extents.md](4-extents.md) |
| Directories | a directory's hash space | one entry block | deeper hash buckets where entries cluster (extendible hashing) | [5-directories.md](5-directories.md) |

Every instance follows the same cell state model. A cell is in one of four
states, encoded in 2 bits wherever a state array is stored:

| Value | State | Meaning |
|---|---|---|
| 0 | FREE | uniform: nothing here (free space / file hole / empty bucket) |
| 1 | RSVD | uniform: reserved by the filesystem itself |
| 2 | FULL | uniform: fully used / mapped as one piece |
| 3 | REFINED | subdivided: consult the child table |

Refinement is always paired with **coarsening**: when all children of a
refined cell return to a uniform state, the parent collapses back and the
child table is reclaimed. Anti-fragmentation is a property of the structure,
not a maintenance tool run after the fact.

The four instances share the format concept, not an implementation. Their
concurrency and journaling requirements differ; implementations should not
force them through one generic structure.

## Virtual addressing

Nothing in file metadata ever stores a physical device address. The two
address types are:

* **Block address**: `<32-bit AG id><32-bit block offset within the AG>`
* **Inode number**: `<32-bit AG id><32-bit inode slot within the AG>`,
  where a slot is `inodesize` bytes (default 256)

Both resolve through the AG map ([1-layout.md](1-layout.md)). Consequences:

* moving an AG (wholly or partially) updates only the AG map — every block
  pointer and inode number in the filesystem remains valid;
* inode numbers are stable across resize and relocation;
* AG ids are never reused, and AG *identity* is never split or merged — only
  an AG's physical mapping refines.

The only physical addresses on disk are in the superblock (its own location,
the AG map location, and the kernel region for the bootloader) — the minimum
needed to bootstrap.

## Metadata integrity

* Every metadata structure begins with a magic value and carries a CRC32C
  checksum and, where it can be rewritten in place, a generation number.
* Unused space inside partially-used metadata blocks is zeroed.
* Together these make the filesystem scannable: a recovery tool can identify
  superblock, AG headers, inodes, and tables by magic + checksum without
  trusting any pointer.

## Numbers and encoding

* All on-disk integers are **big-endian** (network order).
* All timestamps are 64-bit signed seconds + 32-bit nanoseconds.
* Sizes named in this spec (1 MiB cells, 4 KiB blocks, 256-byte inodes) are
  defaults; the format encodes them in the superblock rather than hardcoding.
* File and directory names are UTF-16 (`char16_t`), matching the 5OS VFS.
  *(Trade-off noted: UTF-8 is the interchange norm; UTF-16 avoids conversion
  at the OS boundary. Revisit before format freeze.)*

## Feature flags

The superblock carries three flag words, ext-style:

* `compat` — older drivers may read and write
* `ro_compat` — older drivers may mount read-only
* `incompat` — older drivers must refuse to mount

Any format change after freeze allocates a flag. Version 2 itself is
`incompat` with the 2015 prototype format ([legacy-2015.md](legacy-2015.md)).

## Naming

"5FS" and "GoFS" are the same name (五 = *go*); source code uses the `gofs_`
prefix. On-disk magics use the "5FS" spelling.
