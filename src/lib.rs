//! 5FS (GoFS) — the 5OS filesystem, v2 format. See doc/ for the specification.
//!
//! This library implements a v0 subset of the v2 format: mkfs, structural
//! fsck, image inspection, offline import, and read-only access. The
//! refinement-tree allocator currently operates at level 0 (whole cells)
//! only; extent trees, hashed directories, and the journal write path are
//! not implemented yet.

pub mod device;
pub mod fmt;
pub mod fs;
pub mod fsck;
pub mod mkfs;
