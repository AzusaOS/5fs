//! 5FS (GoFS) — the 5OS filesystem, v2 format. See doc/ for the specification.
//!
//! Implements the v2 format: journaled metadata transactions, the
//! refinement-tree allocator (L0 cells refining to single blocks), extent
//! refinement trees with sparse files, extendible-hash directories, full
//! namespace operations, and resize primitives (grow/relocate/retire).

pub mod alloc;
pub mod device;
pub mod dir;
pub mod extent;
pub mod fmt;
pub mod fs;
pub mod fsck;
pub mod journal;
pub mod mkfs;
pub mod resize;
