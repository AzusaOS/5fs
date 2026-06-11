# 5. Directories: extendible hashing

Directory name space is not spatial, but hash space is — so the AMR instance
for directories is **extendible hashing**: refine hash-space cells where
entries cluster, coarsen when they empty. Unlike fixed-depth schemes (ext4
htree), depth grows only where collisions concentrate, and shrinking
directories actually shrink.

## Hashing

`hash = SipHash-2-4(key = sb_uuid, data = name)` over the raw UTF-16 code
units, 64-bit result. Keying by the volume UUID makes hash-flooding a
directory from outside infeasible. Names are case-sensitive; no
normalization is applied (byte-exact UTF-16 match).

## Structure

A directory is an ordinary inode whose data (mapped via
[4-extents.md](4-extents.md)) contains:

* **Tiny directories** — `in_format` EMBED: entries stored linearly in the
  inode payload. Most directories on a real system stay here.
* **Hashed directories** — one *directory header block* plus *buckets*:

```
directory header block:
  magic        (4) — "5FSD"
  csum         (4)
  gen          (8)
  global_depth (1) — d
  table[2^d]   (4 each) — bucket number, indexed by top d bits of hash

bucket (one block):
  magic        (4) — "5FSb"
  csum         (4)
  gen          (8)
  local_depth  (1)
  entry_count  (2)
  entries[]:
    ino       (8) — inode number
    hash      (8) — cached full hash (avoids rehashing on split)
    type      (1) — file type hint (matches in_mode type bits)
    name_len  (2) — UTF-16 code units
    name          — UTF-16, padded to 8-byte alignment
```

Bucket numbers index into the directory's own file space (bucket *n* is
block *n+1* of the directory file), so buckets are allocated, mapped and
relocated through the ordinary extent machinery.

## Operations

* **Lookup**: hash the name, take the top `global_depth` bits, read the
  bucket the table points at, scan its entries (compare cached hash, then
  full name). Two reads for any directory size once the header is cached.
* **Insert**: append to the bucket. On overflow, **refine**: split the
  bucket by the next hash bit (`local_depth + 1`), redistribute entries; if
  `local_depth` exceeded `global_depth`, double the table. Several table
  slots pointing at one bucket is the normal coarse state.
* **Remove**: delete the entry. When a bucket and its buddy together fit in
  one block, **coarsen**: merge them and decrement `local_depth`; when all
  buckets have depth < `global_depth`, halve the table. A directory that
  empties converts back to EMBED.
* `.` and `..` are not stored; they are synthesized by the VFS (parent is
  tracked by the driver).

## Readdir stability

Iteration order is hash order; the readdir cookie is `<hash><per-bucket
sequence>`, which survives splits and merges (entries move between buckets
but keep their hash). Exact cookie encoding is pinned at format freeze.

## Limits

* Name length: 255 UTF-16 code units.
* `global_depth` max 9 in v1 (the table must fit one 4 KiB header block:
  512 buckets). Growing past that needs a multi-block header under a
  feature flag.

## Implementation notes (v1)

* Buddy-merge is implemented: after a removal, a bucket and its buddy
  (same local depth) that fit one block re-merge with `local_depth - 1`,
  and the table halves whenever every slot pair agrees. Freed bucket
  numbers go on a **freelist in the header block** (count at offset 18,
  entries growing down from the block end) and are reused by later splits,
  so the directory file does not grow while churning.
* A directory that empties completely collapses to FMT_EMPTY and frees its
  blocks.
* Readdir currently returns name order (collected in memory), not
  hash-order cookies.
