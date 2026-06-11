# 5FS — the 5OS FileSystem

5FS (read "go-F-S" — 5 is *go* in Japanese, after Go-OS, the OS that boots in
5 seconds) is a filesystem built around two ideas:

1. **The filesystem is a movable, resizable object.** It grows, shrinks and
   relocates itself online. Block and inode addresses are virtual (allocation
   group relative), so moving data never rewrites file metadata.
2. **Adaptive mesh refinement (AMR) as the single structural principle.**
   At every level — the device map, the allocator, file extents, directories —
   space is covered by coarse cells that refine into finer cells only where
   detail is needed, and coarsen back when it isn't. One recursive idea
   instead of four unrelated data structures.

It also keeps the original 2015 goal: the kernel is stored in physically
contiguous blocks at an offset recorded in the superblock, so a bootloader can
load it with raw block reads and no filesystem driver.

## Status

Design phase. The specification in `doc/` is format v2 (2026). The code in
this repository is the 2015 prototype and implements the legacy format
([doc/legacy-2015.md](doc/legacy-2015.md)); it will be rewritten against v2.

## Specification

* [doc/0-principles.md](doc/0-principles.md) — the AMR cell model, naming, conventions
* [doc/1-layout.md](doc/1-layout.md) — device layout, superblock, allocation groups, the AG map, addressing, boot region
* [doc/2-allocation.md](doc/2-allocation.md) — free space management: the refinement tree allocator
* [doc/3-inodes.md](doc/3-inodes.md) — inode format
* [doc/4-extents.md](doc/4-extents.md) — file data mapping: extent refinement trees, sparse files, adaptive write granularity
* [doc/5-directories.md](doc/5-directories.md) — directories: extendible hashing
* [doc/6-journal.md](doc/6-journal.md) — journal and crash consistency
* [doc/7-resize.md](doc/7-resize.md) — growing, shrinking, relocation and holes

## Headline capabilities (design targets)

* Online grow **and shrink**, including from the middle of the device.
* The filesystem need not be contiguous on the device: allocation groups live
  wherever the AG map says they do, and unmapped holes between them are fine
  (thin provisioning, coexistence with foreign on-disk data).
* Relocation is incremental and never touches file metadata.
* Allocation granularity adapts from 1 MiB extents down to 256-byte inode
  slots through one mechanism (tail packing without special cases).
* Per-region adaptive block granularity inside a single file.
* All metadata is checksummed and self-identifying; the filesystem is
  scannable for recovery.
* 64-bit timestamps.
* Contiguous-kernel guarantee for bootloaders.
