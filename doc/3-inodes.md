# 3. Inodes

Inodes are dynamically allocated 256-byte slots ([2-allocation.md](2-allocation.md))
anywhere in their AG — there is no inode table and no inode count chosen at
mkfs. The inode number encodes the location:

```
inode number = <32-bit AG id><32-bit slot index>
byte offset within AG = slot index × sb_inodesize
```

Inode numbers are stable across resize and relocation (addresses are
AG-relative, see [0-principles.md](0-principles.md)).

## Layout

A 256-byte inode is a 128-byte header plus a 128-byte format-specific
payload.

| Field | Size | Description |
|---|---|---|
| `in_magic` | 2 | `"IN"` (0x494e) |
| `in_version` | 1 | 2 |
| `in_format` | 1 | payload format, see below |
| `in_csum` | 4 | CRC32C of the whole inode |
| `in_mode` | 2 | type + permission bits (standard `S_Ixxx`) |
| `in_nlink` | 4 | |
| `in_uid`, `in_gid` | 4 + 4 | |
| `in_flags` | 4 | per-file flags (noatime, immutable, …) |
| `in_gen` | 4 | NFS-style generation |
| `in_size` | 8 | bytes |
| `in_nblocks` | 8 | blocks in use (all granularities, in block units) |
| `in_atime`, `in_mtime`, `in_ctime`, `in_btime` | 12 each | 64-bit signed seconds + 32-bit nanoseconds |
| `in_xattr` | 8 | block address of xattr block, 0 if none |
| reserved | to 128 | zero |

Unused slot space is zeroed; an inode is identifiable by magic + checksum
alone, which keeps the filesystem scannable for recovery.

## Payload formats

The format progression is itself the AMR idea applied to the mapping
structure — start minimal, refine as the file grows, coarsen back on
truncate:

| `in_format` | Name | Payload |
|---|---|---|
| 1 | EMPTY | nothing; `in_size` = 0 |
| 2 | EMBED | file data inline, up to 128 bytes |
| 3 | EXTENT | inline array of extent records (about six fit) — covers most files |
| 4 | TREE | root table of an extent refinement tree ([4-extents.md](4-extents.md)) |

Directories use the same progression: EMBED for a handful of entries, then
extent-mapped buckets ([5-directories.md](5-directories.md)).

Hard links are ordinary multiple directory references to one inode number
with `in_nlink` counting them.

## Extended attributes

`in_xattr` points to a single xattr block (magic `"5FSX"` namespace TBD,
checksummed) holding name/value pairs. Larger xattr storage is deferred to a
`compat` feature flag when needed.
