# 6. Journal and crash consistency

Version 2 baseline: **metadata journaling, write-in-place**, data in ordered
mode. The journal is the one structure where AMR does not apply — it is a
sequential log, and any structure imposed on it would be overhead.

## Placement

One journal per volume, in AG 0, located by `sb_journal_start` /
`sb_journal_length`. Default size 128 MiB. The journal occupies RSVD blocks
and, living in the IMMOVABLE AG 0, never relocates.

## What is journaled

Every metadata mutation, as whole-block (physical) records:

* superblock, AG headers
* AG map updates (both copies' generations advance through the journal)
* allocator root and child tables
* inode slots (journaled as the containing block)
* extent tree nodes, directory header blocks and buckets, xattr blocks

File **data** is not journaled. Ordered mode: data blocks referenced by a
transaction are written to their final location *before* the transaction
commits, so metadata never points at unwritten data. (A `compat` flag may
later add full data journaling for paranoid mounts.)

## Format

The journal is a circular log of transactions:

```
descriptor block: magic "5FSJ", csum, sequence number,
                  count + list of target block addresses (AG-relative)
data blocks:      the new contents of each listed block
commit block:     magic "5FSC", csum, sequence number,
                  CRC32C over the whole transaction
```

A transaction is valid only if its commit block is present, its sequence
number is the expected next, and the transaction checksum matches — torn
writes are detected by checksum, not by hope.

Pinned layout: descriptor and commit blocks carry `magic(4) csum(4)
seq(8)`; the descriptor adds `count(4)` and the target block addresses from
offset 24; the commit adds the transaction CRC32C at offset 16. A
transaction never wraps across the journal end (the writer restarts at
block 0 instead). The superblock's `sb_journal_seq`/`sb_journal_head` are
the checkpoint: replay starts there, also probing block 0 to catch an
unrecorded wrap.

## Replay and checkpoint

* **Mount**: scan from the last checkpoint, replay every valid transaction
  in sequence order to its final location, stop at the first invalid one.
  Replay is idempotent.
* **Checkpoint**: once journaled blocks reach their final locations, the
  journal tail advances. The superblock records the checkpoint sequence.

## Transaction scope

Single journal = one serialization point for commits. Mitigations: per-AG
structures keep transactions small and independent; batching/group-commit
amortizes the sequential writes. Operations and their transaction contents:

| Operation | Journaled blocks |
|---|---|
| allocator refine/coarsen | parent table + child table + AG header counters |
| file append | inode + extent node(s) + allocator tables (data written first, ordered) |
| directory insert with split | bucket + buddy bucket + header block + inode |
| AG relocation step | AG map copy + superblock gen (data segment copied first) |

## Open decision: copy-on-write

The extent refinement tree ([4-extents.md](4-extents.md)) is deliberately
CoW-ready: under CoW, a partial overwrite refines the cell and rewrites only
the touched children, bounding write amplification to the refined cell size
— and CoW would supersede ordered mode, give O(1) crash consistency for
data, and open the door to snapshots and reflinks.

It is **not** the v2 baseline because it drags in reference counting (or a
deferred-free scheme), space accounting for shared blocks, and a
garbage-collection story — a substantial scope increase. The decision gate:
if snapshots become a 5OS requirement, switch to CoW *before* implementation
starts, since it deletes most of this journal's data-path rules. Either way
the on-disk structures above stay; CoW would add a per-extent refcount
structure under an `incompat` flag.
