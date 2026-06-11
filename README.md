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

The specification in `doc/` is format v2 (2026), implemented by the `gofs`
Rust library and tools:

* **Journaled metadata** — whole-block transactions, checksummed commits,
  idempotent replay on mount; data in ordered mode.
* **Refinement-tree allocator** — 1 MiB cells refining to single blocks,
  table blocks in arena cells, coarsening buddy-merge on free.
* **Extent refinement trees** — sparse files, refine-on-collision,
  per-region granularity; small files stay inline (EMBED / 6 inline
  extents).
* **Extendible-hash directories** — SipHash keyed by volume UUID, bucket
  splits, EMBED ↔ hashed conversion both ways.
* **Full namespace** — create/mkdir/unlink/rmdir/rename/link/symlink/
  truncate/setattr, path resolution; read-write FUSE mount.
* **Resize** — online grow, wholesale AG relocation, retiring empty AGs,
  tail shrink; only the AG map moves, never file metadata.
* **fsck** — validates every structure above, tallies the allocator
  against counters, walks the namespace verifying nlinks and mappings.

Also implemented: allocator L3 inode-slot refinement (inodes are fully
allocator-tracked), directory bucket buddy-merge with a freed-bucket
freelist, granule-aligned truncate reclamation, and the **kernel boot
region** — `mkfs.5fs --kernel image` stores the kernel physically
contiguous, raw-readable by a bootloader at `sb_kernel_offset`, visible as
an immutable `/kernel.bin`, replaceable via `debugfs.5fs kernel-update`.

The one open decision is CoW vs journaled write-in-place
([doc/6-journal.md](doc/6-journal.md)) — gated on snapshots becoming a 5OS
requirement. The 2015 C++ prototype (legacy format,
[doc/legacy-2015.md](doc/legacy-2015.md)) has been removed.

## Building

```
make            # cargo build --release, tools appear in bin/ with dotted names
make test       # unit + e2e + model + crash + compliance tests
make stress     # heavy suites: 200k files, dir limits, churn, long model runs
make fixtures   # regenerate the committed reference image (format changes only)
make install    # install mkfs.5fs fsck.5fs debugfs.5fs mount.5fs to /usr/local/sbin
```

## Testing

* **e2e** (`tests/e2e.rs`) — every feature path: formats, sparse trees,
  hashed directories, truncate, resize, journal replay, kernel region.
* **model** (`tests/model.rs`) — seeded random op streams applied to the
  filesystem and an in-memory model in lockstep; any divergence in
  success/failure or content fails, then fsck must be clean. Reproduce a
  failure with `cargo run --example modeldbg <seed>`.
* **crash** (`tests/crash.rs`) — the device layer records every write with
  its sync epoch; hundreds of simulated power-cut states (epoch prefixes,
  unordered in-epoch subsets, torn blocks) must each recover via journal
  replay + superblock self-heal to a clean, prefix-consistent state.
* **compliance** (`tests/compliance.rs` + `fixtures/`) — a committed
  reference image with a manifest: current code must read everything a
  past build wrote, and raw byte-level checks pin the format itself
  (magics, offsets, independently recomputed checksums, kernel
  contiguity).
* **stress** (`tests/stress.rs`, `make stress`) — 200,000 files over a
  three-level tree with full fsck, a directory pushed to the depth limit
  and torn back down, create/delete churn watching for block leaks, and
  terabyte-offset sparse files.

Cargo forbids `.` in binary target names, so `cargo build` produces
`mkfs5fs` etc.; the Makefile copies them to their proper names
(`mkfs.5fs`, `fsck.5fs`, `debugfs.5fs`, `mount.5fs`).

```
mkfs.5fs disk.img --size 256M -L mylabel [--kernel kernel.bin]
fsck.5fs disk.img
debugfs.5fs disk.img sb | agmap | ag 0 | inode 0x30 | scan | journal
debugfs.5fs disk.img ls /some/dir
debugfs.5fs disk.img import hostfile.bin /docs/name.bin
debugfs.5fs disk.img cat /docs/name.bin
debugfs.5fs disk.img mkdir /d | rm /f | rmdir /d | mv /a /b | symlink /l target
debugfs.5fs disk.img grow 512M | shrink 256M | relocate 2 0x4000000 | retire 1
debugfs.5fs disk.img kernel-update new-kernel.bin
mount.5fs disk.img /mnt/point    # read-write; needs FUSE (macFUSE on macOS)
```

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
