# 2. Free space management: the refinement tree allocator

Each AG manages its own block space with a refinement tree — the AMR
instance over allocation. It replaces the 2015 flat 2-bit bitmap; the 2015
"partially filled block of inodes" was exactly one refinement level, and this
generalizes it.

## Cell hierarchy

With default sizes (4 KiB blocks, 256-byte inodes), fanout is 16 at every
level:

| Level | Cell size | Refines into |
|---|---|---|
| 0 | 1 MiB (256 blocks) | 16 × 64 KiB |
| 1 | 64 KiB (16 blocks) | 16 × 4 KiB |
| 2 | 4 KiB (one block) | 16 × 256-byte slots |
| 3 | 256-byte slot | leaf |

* **Data** allocations have a granularity floor of one block (level 2):
  sub-block cells would force read-modify-write under the device sector
  size. Refinement below block level exists **only for inode slots and
  small fs-internal metadata** (the 2015 rule, kept).
* Cell sizes derive from `sb_blocksize` / `sb_inodesize`; the constants
  above are the defaults, not the format.

## Cell states

Each cell is FREE / RSVD / FULL / REFINED (2 bits, see
[0-principles.md](0-principles.md)). FULL at a coarse level means the whole
cell is allocated as a unit — e.g. a 1 MiB file extent is *one* level-0 FULL
cell, not 256 marked blocks.

## On-disk structure

The **root table** sits right after the AG header (`ag_alloc_root`): a flat
2-bit state array over the AG's level-0 cells, followed by a 6-byte
child-table reference per cell (`block u32` within the AG + `record u16`),
meaningful only where the state is REFINED. A 64 GiB AG has 65,536 level-0
cells → 16 KiB of states + 384 KiB of references (100 blocks).

**Child tables** are materialized only for REFINED cells and live in
allocator table blocks (state RSVD, magic `"5FSA"`, CRC32C, generation).
A table block holds a header plus fixed 128-byte records:

```
table record:
  states   (4)  — 16 children × 2 bits
  refs[16] (6 each) — child-table reference, valid only where state = REFINED
                      <4-byte block offset within AG><2-byte record index>
  (padding to 128)
```

The root table's REFINED entries need references too; a small reference
array for level-0 cells follows the root state array, materialized in the
same reserved area.

Metadata cost is proportional to *refinement actually present*: a
mostly-empty AG or one holding large files carries kilobytes of allocator
metadata. The worst case (everything refined to block level) is bounded by
the cost of the old flat bitmap; it never exceeds it.

## Operations

* **Allocate large (≥ 1 MiB)**: scan the root table for FREE level-0 cells.
  Result is naturally 1 MiB-aligned and contiguous — this is what the
  contiguous-kernel guarantee and large-file extents use.
* **Allocate small**: prefer descending into an existing REFINED cell with
  free children (keeps refinement clustered), else refine a FREE cell near
  `ag_data_hint`. Refining writes one child table and flips the parent
  state.
* **Allocate inode slot**: same, one level deeper, near `ag_ino_hint`.
* **Free**: flip the cell to FREE; if all siblings are now FREE, collapse —
  free the child table, set the parent FREE, and repeat upward. Coarsening
  is the buddy merge: defragmentation is structural, not a background tool.
* Every refine/coarsen is a single journal transaction (parent state +
  child table + counters).

## Allocation policy

The structure is simple; quality lives in policy. Rules of thumb the
implementation should follow (not format-binding):

* **Cluster refinement.** One tail in each of a thousand level-0 cells
  destroys the coarse free space large allocations need. Fill refined
  neighborhoods before opening new ones.
* **Separate lifetimes.** Inode-slot refinement and data-tail refinement
  should use different neighborhoods, so metadata churn doesn't fragment
  data regions.
* **Honor locality.** A file's metadata (extent tables, directory buckets)
  allocates from its inode's AG when possible, but **may spill to another
  AG** — the 2015 same-AG rule becomes a preference, not an invariant, to
  remove its ENOSPC-with-free-space failure mode.

## Implementation notes (v1)

* Table blocks live in **arena cells**: a FREE level-0 cell claimed as RSVD
  (`ag_tbl_arena` points at the active one). Table blocks carry magic
  `"5FSA"`, CRC32C, a generation, and a used-record bitmap; records are 128
  bytes at offsets `128 × (1 + index)`.
* Allocation granularity: ≤ 16 blocks exact (L2), 17–255 blocks in 16-block
  L1 cells, ≥ 256 in whole L0 cells. `free` rediscovers granularity from
  the on-disk states, so callers free `(start, length)` only.
* Inode slots: the spec's level-3 slot refinement is deferred — inode
  blocks are ordinary single-block allocations, free slots are recognized
  by being zeroed, and an emptied inode block returns to the allocator.

## Open questions

* Whether level-1 (64 KiB) FULL cells should be the default unit for medium
  files rather than going straight to level 0.
* Per-AG vs global policy for choosing which AG a new file allocates from
  (parallelism vs locality).
* Releasing emptied arena cells back to FREE.
