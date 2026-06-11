# 4. File data mapping: extent refinement trees

How a file's offset space maps to blocks. Two representations exist, and a
file moves between them as it grows or shrinks (see
[3-inodes.md](3-inodes.md) for the EMPTY/EMBED/EXTENT/TREE progression).

## EXTENT format: inline extents

The inode payload holds a small sorted array of classic extent records:

```
extent record (20 bytes):
  file_offset (8) — in blocks
  block_addr  (8) — AG-relative block address
  length      (4) — in blocks
```

About six records fit in the 128-byte payload. Ranges not covered by any
record are **holes** and read as zeros — sparse files exist in the cheapest
format too. When a file needs more records than fit inline, it converts to
TREE.

## TREE format: the refinement tree

The AMR instance over a file's offset space. The file is covered by coarse
cells; a cell is:

* **FREE** — a hole; reads as zeros, no storage
* **FULL** — mapped to one contiguous run of blocks: the cell record holds
  the block address of its start
* **REFINED** — subdivided; the cell record points to a child node

```
tree node (one block, allocated as fs metadata):
  magic  (4)  — "5FST"
  csum   (4)  — CRC32C
  gen    (8)
  level  (1)  — cell size = node's range / fanout
  states       — 2 bits per child
  cells[]      — 8 bytes per child: block address (FULL) or node reference (REFINED)
```

The root lives in the inode payload (a reduced-width node), so small trees
add no extra reads. Cell sizes are powers of the fanout times the block
size; the minimum data cell is one block. Exact fanout constants (inline
root vs block-sized nodes) are pinned at format freeze — the structure, not
the constants, is the format.

Properties:

* **Lookup** is radix descent by file offset — no binary search, depth is
  refinement depth at that offset, not entry count.
* **Sparseness is structural.** `punch_hole` sets cells FREE and frees
  blocks; a fully-FREE node coarsens away. Nothing special-cases holes.
* **Alignment composes with the allocator.** A FULL cell of size S is
  backed by a contiguous, S-aligned allocation
  ([2-allocation.md](2-allocation.md) coarse cells), so large files are
  physically as coarse as they are logically.

## Adaptive write granularity

The reason this structure exists rather than a B+tree: **granularity adapts
per region of one file**, driven by observed writes.

* Sequential writes allocate and map coarse cells (up to 1 MiB as one
  record).
* Partial overwrites of a coarse cell refine it: the cell splits, untouched
  children keep pointing into the original blocks, rewritten children point
  at new blocks. A region that keeps taking 4 KiB random writes settles at
  block granularity; the cold remainder of the file stays coarse.
* Sequential rewrite or an explicit defrag coarsens refined regions back.

This is what fixed per-file record sizes (ZFS `recordsize`) cannot do: the
hot header pages of a database file and its cold bulk get different
granularities automatically.

Version 2 baseline is journaled write-in-place ([6-journal.md](6-journal.md));
overwrites of FULL cells happen in place and refinement is triggered by
allocation patterns (append, holes, fragmentation). The tree is deliberately
**CoW-ready**: under copy-on-write the same refine-on-partial-overwrite rule
bounds write amplification to the refined cell size. Whether v2 data paths
adopt CoW is an open decision recorded in [6-journal.md](6-journal.md).

## Limits

* File size: 2^64 bytes.
* A single FULL cell maps at most one contiguous run; runs larger than the
  maximum cell size are simply several sibling FULL cells.
* `in_nblocks` counts blocks actually backed (holes cost nothing).
