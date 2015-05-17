#pragma once

#include "vfs.hpp"

typedef uint64_t gofs_blk_t;
typedef uint64_t gofs_ino_t;

// magic value
#define GOFS_AG_HEADER_MAGIC VAL_BE32(0x35465348) /* "5FSH" */

#define GOFS_BLOCK_FREE 0
#define GOFS_BLOCK_INO 1
#define GOFS_BLOCK_DATA 2
#define GOFS_BLOCK_RESERVED 3

typedef unsigned char uuid_t[16];

// header structure
typedef struct {
	uint32_t ag_magic;
	uint32_t ag_num; // ref of current ag, 0 for superblock
	gofs_blk_t ag_free_blocks; // free blocks
	gofs_blk_t ag_reserved_blocks;
	gofs_blk_t ag_ino_blocks;
	gofs_blk_t ag_data_blocks;
} __attribute__((packed)) gofs_ag_t;

typedef struct {
	union {
		gofs_ag_t ag;
		char __ag_reserved[128];
	};
	uint32_t sb_blocksize; // initialized on mkfs
	uint32_t sb_next_ag; // next number for a new ag
	gofs_blk_t sb_free_blocks; // free blocks
	gofs_blk_t sb_reserved_blocks; // reserved (superblock, etc) blocks
	gofs_blk_t sb_ino_blocks; // inodes blocks
	gofs_blk_t sb_data_blocks; // data blocks
	gofs_ino_t sb_root_ino;
	gofs_blk_t sb_journal_start; // where journal is
	gofs_blk_t sb_journal_length; // length of journal in blocks
	gofs_blk_t sb_kernel_offset; // position of kernel (used for bootloader, set to zero if none)
	gofs_blk_t sb_kernel_end; // end of kernel
	uint64_t sb_flags;
	uint16_t sb_inodesize; // default is 256. Must be lower than or equal sb_blocksize. sb_blocksize must be a 2^x multiple of sb_inodesize so in one block there can be 1, 2, 4, 8, 16 etc inodes
	char16_t sb_disk_name[16];
	char reserved[262];
} __attribute__((packed)) gofs_sb_t; // super block (max size = sb_blocksize, which is min 512)

static_assert(sizeof(gofs_sb_t) == 512, "Invalid superblock size");

typedef struct {
	int32_t t_sec;
	int32_t t_nsec;
} __attribute__((packed)) gofs_timestamp_t;

typedef struct {
	uint16_t ino_magic; // The inode signature where these two bytes are 0x494e, or "IN" in ASCII.
	uint16_t ino_mode; // Specifies the mode access bits and type of file using the standard S_Ixxx values defined in stat.h.
	int8_t ino_version; // should be set to 1
	int8_t ino_format; // data storage format
	uint32_t ino_nlink;
	uint32_t ino_uid;
	uint32_t ino_gid;
	gofs_timestamp_t ino_atime; // unless noatime flag is set
	gofs_timestamp_t ino_mtime; // last data change time
	gofs_timestamp_t ino_ctime; // last inode change time
	uint64_t ino_size;
	uint64_t ino_nblocks; // number of blocks in use
	uint32_t ino_flags;
	uint32_t ino_gen;
	char reserved[62]; // make size reach 128 bytes
} __attribute__((packed)) gofs_in_t;

static_assert(sizeof(gofs_in_t)==128, "Invalid size for gofs_in_t");

class Vfs_GoFS: public Vfs_Interface {
public:
	Vfs_GoFS(Vfs_Interface *parent, vfs_ino_t parent_ino, void **parent_context);

	virtual vfs_ino_t lookup(vfs_ino_t parent, const char16_t *name, size_t name_len);
	virtual int format(const char16_t *name, size_t name_len);

private:
	void create_ag(uint32_t ag_num, gofs_blk_t start_block, gofs_blk_t length);

	void read_block(gofs_blk_t, char*, size_t);
	void write_block(gofs_blk_t, char*, size_t);

	Vfs_Interface *p_parent;
	vfs_ino_t p_parent_ino;
	void **p_parent_context;
	uint32_t p_blocksize;

	gofs_sb_t sb;
};

