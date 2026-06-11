# 1. Device layout, superblock, allocation groups, the AG map

## Overview

A 5FS volume is a set of **allocation groups** (AGs) placed on a block device
by the **AG map**. AGs have variable sizes, need not be adjacent, need not be
in id order on the device, and the space between them does not belong to the
filesystem. The only fixed point is AG 0, which starts at device offset 0 and
holds the superblock, the AG map, the journal, and (optionally) the boot
kernel region.

```
device:  [ AG 0 ][ AG 3 ]....hole....[ AG 1 ][ AG 4 ].....[ AG 2 ]
            ^
            superblock, AG map, journal, kernel region
```

## Limits

| Quantity | Limit | Source |
|---|---|---|
| block size | 4 KiB default; min 512, power of two | superblock |
| inode size | 256 bytes default; `blocksize` must be a power-of-two multiple | superblock |
| AG size | min(2^32 × inodesize, 2^32 × blocksize) — 1 TiB at defaults | 32-bit local addressing |
| recommended AG size | 4–64 GiB | keeps relocation units cheap |
| AG count | 2^32, ids never reused | 32-bit AG id |
| file size | 2^64 bytes | 64-bit `in_size` |

## Superblock

Located at device offset 0 (start of AG 0), one block, with a backup copy at
the end of AG 0's first physical segment. Fields (all big-endian):

| Field | Size | Description |
|---|---|---|
| `sb_magic` | 4 | `"5FSB"` |
| `sb_csum` | 4 | CRC32C of the superblock (field zeroed during computation) |
| `sb_version` | 4 | 2 |
| `sb_flags_compat` | 8 | see [0-principles.md](0-principles.md) |
| `sb_flags_ro_compat` | 8 | |
| `sb_flags_incompat` | 8 | |
| `sb_blocksize` | 4 | bytes |
| `sb_inodesize` | 2 | bytes |
| `sb_uuid` | 16 | volume UUID (also keys directory hashing) |
| `sb_disk_name` | 32 | UTF-16, 16 code units |
| `sb_root_ino` | 8 | inode number of `/` |
| `sb_next_ag` | 4 | next AG id to assign (monotonic, never reused) |
| `sb_agmap_offset` | 8 | **physical** device offset of the AG map area |
| `sb_agmap_length` | 8 | length in bytes of each AG map copy |
| `sb_journal_start` | 8 | block address (AG-relative) of journal |
| `sb_journal_length` | 8 | journal length in blocks |
| `sb_kernel_offset` | 8 | **physical** device offset of kernel region, 0 if none |
| `sb_kernel_end` | 8 | physical end of kernel region |
| `sb_free_blocks` etc. | 8 each | volume-wide counters (advisory; authoritative counts are per-AG) |
| `sb_gen` | 8 | generation, bumped on every superblock write |

Mount procedure: read block 0, verify magic + checksum; on failure try the
backup; load the AG map; replay the journal; mount.

## The AG map

The AG map is the keystone structure: it is how virtual addresses
(`<AG id><offset>`) become physical device offsets. It is **not** a file —
it lives in space reserved within AG 0 at a physical offset recorded in the
superblock, stored as **two copies** updated alternately (A/B), each
checksummed and generation-stamped. The copy with the highest valid
generation wins.

Conceptually the map is an array indexed by AG id. Following the AMR model,
an entry is either coarse (one mapping) or refined (several):

```
agmap_entry:
  flags        (2)   — PRESENT, IMMOVABLE, RETIRED, REFINED
  segment_count(2)   — 1 if coarse
  ag_length    (4)   — AG length in blocks
  segments[]:
    ag_block     (4) — first block (within the AG's address space) this segment maps
    length       (4) — in blocks
    dev_offset   (8) — physical device offset in bytes
```

* **Coarse entry** — one segment mapping the whole AG: the common case,
  one array lookup per translation.
* **Refined entry** — the AG's address space is split across several physical
  segments. This is what makes relocation *incremental*: a 64 GiB AG moves
  1 GiB at a time, each step journaled as one small map update, while
  mounted. When the AG is physically contiguous again the entry coarsens
  back to one segment. Refined mappings also allow punching a hole inside
  an AG's physical footprint (see [7-resize.md](7-resize.md)).
* **IMMOVABLE** — set on AG 0 (the superblock and kernel region are at
  physical offsets the bootloader depends on).
* **RETIRED** — the AG id was used and its space released; the id is never
  reassigned.

The whole map is held in memory while mounted (a few MiB even for thousands
of AGs); translation cost is a lookup plus, for refined entries, a short
segment search.

## Allocation group format

Each AG begins with a header block:

| Field | Size | Description |
|---|---|---|
| `ag_magic` | 4 | `"5FSH"` |
| `ag_csum` | 4 | CRC32C |
| `ag_num` | 4 | this AG's id |
| `ag_gen` | 8 | generation |
| `ag_length` | 4 | length in blocks |
| `ag_free_blocks` / `ag_rsvd_blocks` / `ag_full_blocks` | 4 each | authoritative counters |
| `ag_alloc_root` | 4 | block offset (within this AG) of the allocator root table |
| `ag_ino_hint` / `ag_data_hint` | 4 each | allocation cursors (hints only) |

After the header: the allocator root table ([2-allocation.md](2-allocation.md)),
then general space. AG 0 additionally reserves, in order: the AG map copies,
the journal, and the kernel region.

AG headers exist so a recovery tool can rediscover the volume by scanning for
`"5FSH"` blocks even if the AG map is lost; during normal operation the AG
map is authoritative.

## Boot kernel region

5FS guarantees that the boot kernel is stored **physically contiguous**.
`sb_kernel_offset` / `sb_kernel_end` give its physical location so a
bootloader can load it with raw block reads — no filesystem parsing. The
region lives in AG 0, which is IMMOVABLE, so the guarantee survives any
resize or relocation. Installing or replacing the kernel rewrites the region
and updates the superblock; the file is also reachable normally (e.g. as
`/system/kernel.bin`) via an inode whose extents point at the reserved
blocks (RSVD in the allocator).
