#pragma once

#include "vfs.hpp"
#include <map>

typedef uint64_t gofs_blk_t;
typedef uint64_t gofs_ino_t;

#if __BYTE_ORDER == __BIG_ENDIAN
#define GOFS_BE16(_x) (_x)
#define GOFS_BE32(_x) (_x)
#define GOFS_BE64(_x) (_x)
#else
#define GOFS_BE16(_x) (__builtin_bswap16(_x))
#define GOFS_BE32(_x) (__builtin_bswap32(_x))
#define GOFS_BE64(_x) (__builtin_bswap64(_x))
#endif

// magic value
#define GOFS_AG_HEADER_MAGIC GOFS_BE32(0x35465348) /* "5FSH" */
#define GOFS_INO_MAGIC GOFS_BE16(0x494e)

// SO, data blocks are "FULL", inodes will be FULL if max inodes, else it'll be INO_AVA
#define GOFS_BLOCK_FREE 0
#define GOFS_BLOCK_PART 1
#define GOFS_BLOCK_FULL 2
#define GOFS_BLOCK_RSVD 3

// inode format type
#define GOFS_INODE_FORMAT_EMPTY 1
#define GOFS_INODE_FORMAT_EMBED 2
#define GOFS_INODE_FORMAT_BLOCK 3
#define GOFS_INODE_FORMAT_BTREE 4

typedef unsigned char uuid_t[16];

// header structure
typedef struct {
	uint32_t ag_magic;
	uint32_t ag_num; // ref of current ag, 0 for superblock
	uint32_t ag_length;
	uint32_t ag_free_blocks; // free blocks
	uint32_t ag_rsvd_blocks;
	uint32_t ag_part_blocks;
	uint32_t ag_full_blocks;
	gofs_blk_t ag_this;
	gofs_blk_t ag_next;
	uint32_t ag_data_alloc_pos; // position for next allocation. Go back to zero when reaching end of AG
	uint32_t ag_ino_alloc_pos; // position for next inode allocation. Can point to a partial block.
} __attribute__((packed)) gofs_ag_t;

typedef struct {
	gofs_ag_t *ag;
	char *bitmap;
} gofs_ag_info_t;

typedef struct {
	union {
		gofs_ag_t ag;
		char __ag_reserved[128];
	};
	uint32_t sb_blocksize; // initialized on mkfs
	uint32_t sb_next_ag; // next number for a new ag
	gofs_blk_t sb_free_blocks; // free blocks
	gofs_blk_t sb_rsvd_blocks; // reserved (superblock, etc) blocks
	gofs_blk_t sb_part_blocks; // inodes blocks
	gofs_blk_t sb_full_blocks; // data blocks
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
	uint16_t in_magic; // The inode signature where these two bytes are 0x494e, or "IN" in ASCII.
	uint16_t in_mode; // Specifies the mode access bits and type of file using the standard S_Ixxx values defined in stat.h.
	int8_t in_version; // should be set to 1
	int8_t in_format; // data storage format
	uint32_t in_nlink;
	uint32_t in_uid;
	uint32_t in_gid;
	gofs_timestamp_t in_atime; // unless noatime flag is set
	gofs_timestamp_t in_mtime; // last data change time
	gofs_timestamp_t in_ctime; // last inode change time
	uint64_t in_size;
	uint64_t in_nblocks; // number of blocks in use
	uint32_t in_flags;
	uint32_t in_gen;
	char reserved[62]; // make size reach 128 bytes
} __attribute__((packed)) gofs_in_t;

static_assert(sizeof(gofs_in_t)==128, "Invalid size for gofs_in_t");

class Vfs_GoFS: public Vfs_Interface {
public:
	Vfs_GoFS(Vfs_Interface *parent, vfs_ino_t parent_ino, void **parent_context);

	virtual vfs_ino_t lookup(vfs_ino_t parent, const char16_t *name, size_t name_len);
	virtual int format(const char16_t *name, size_t name_len);

	virtual int mount();
	virtual int umount();

	const gofs_sb_t *superBlock() const;

private:
	void create_ag(uint32_t ag_num, gofs_blk_t start_block, gofs_blk_t length, gofs_blk_t next);
	void ag_dirty(uint32_t ag_num);
	uint64_t store_inode(gofs_in_t*, uint32_t target_ag = 0xffffffff);
	
	void read_bitmap(gofs_ag_info_t*);
	void bitmap_dirty(gofs_ag_info_t*, uint32_t pos);

	void read_block(gofs_blk_t, char*, size_t);
	void write_block(gofs_blk_t, char*, size_t);

	Vfs_Interface *p_parent;
	vfs_ino_t p_parent_ino;
	void **p_parent_context;
	uint32_t p_blocksize;
	uint16_t p_inodesize;
	bool p_mounted;

	gofs_sb_t sb;

	std::map<uint32_t,gofs_ag_info_t*> ag_map;
};

