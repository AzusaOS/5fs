# 7. Growing, shrinking, relocation and holes

The capability that motivates the addressing design: because every block
pointer and inode number is AG-relative ([0-principles.md](0-principles.md)),
physical layout changes never touch file metadata — only the AG map.
Everything in this chapter happens online.

## Growing

Create an AG in any unmapped device region, of any size up to the AG limit:
write its header and allocator root, journal the new AG map entry, bump
`sb_next_ag`. The new AG need not be adjacent to anything. Growth is O(1) in
existing data.

## Relocation

The primitive everything else builds on. To move part of an AG:

1. pick a source range and a free destination region (unmapped device space,
   or space reserved from another AG);
2. copy the data;
3. journal an AG map update refining the AG's entry: the moved range's
   segment now points at the new physical offset
   ([1-layout.md](1-layout.md));
4. the old region is now unmapped — reusable or discardable.

Steps are sized at the implementation's convenience (e.g. 1 GiB); a 64 GiB
AG moves in many small journaled steps while mounted. Writes to a range
being copied are handled by copying ahead of the write or re-copying the
dirtied range — the map flips atomically per segment either way. When the
AG ends up physically contiguous again, its map entry coarsens back to one
segment.

AG 0 is IMMOVABLE: the superblock, AG map, journal and kernel region keep
the physical addresses the bootloader and mount path depend on.

## Shrinking

To release a device range (end of device, or anywhere):

1. **Relocate away**: any AG segments inside the range move elsewhere via
   relocation. No per-file work, no reverse mapping — whole segments move
   and the map absorbs the change.
2. **Or retire**: if an AG inside the range is empty (its allocator shows
   all FREE), journal its map entry to RETIRED and drop it. The id is never
   reused.
3. The range is now unmapped: truncate the device/partition if it was the
   tail, or leave it as a hole.

The 2015 constraint ("shrinking needs to be planned in advance") is gone —
any region can be vacated at any time, limited only by total free space.

Emptying a *mostly-empty* AG to retire it does require migrating its live
files (the one operation that touches file metadata, since inode numbers
embed the AG id). Plain relocation never needs this; prefer moving AGs over
emptying them.

## Holes and thin provisioning

Unmapped device regions simply do not belong to the filesystem:

* **Thin provisioning** — vacated regions are discarded (TRIM) wholesale;
  the filesystem also discards freed allocator cells opportunistically
  (coarse cells make discards large and aligned).
* **Coexistence** — other on-disk data (another filesystem, raw areas) can
  live in the holes; 5FS never reads or writes outside mapped segments.
* **Shrink-from-the-middle** — releasing a range in the middle of the
  device is the same operation as releasing the tail.

## Interaction with the boot guarantee

The kernel region is physically contiguous inside immovable AG 0
([1-layout.md](1-layout.md)), so no resize or relocation can invalidate
`sb_kernel_offset`. Replacing the kernel is the only operation that rewrites
that region.
