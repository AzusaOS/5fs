#include "gofs.hpp"
#include <stdio.h>
#include <errno.h>
#include <string.h>
#include <stdlib.h>
#ifdef __APPLE__
#include <machine/endian.h>
#else
#include <endian.h>
#endif

#if __BYTE_ORDER == __BIG_ENDIAN
#define VAL_BE16(_x) (_x)
#define VAL_BE32(_x) (_x)
#define VAL_BE64(_x) (_x)
#else
#define VAL_BE16(_x) (__builtin_bswap16(_x))
#define VAL_BE32(_x) (__builtin_bswap32(_x))
#define VAL_BE64(_x) (__builtin_bswap64(_x))
#endif

static __inline__ void bits_set(char *loc, uint32_t index, int state) {
	auto r = div(index, 4);
	switch(r.rem) {
		case 0: loc[r.quot] = (loc[r.quot] & 0x3f) | ((state & 0x3) << 6); return;
		case 1: loc[r.quot] = (loc[r.quot] & 0xcf) | ((state & 0x3) << 4); return;
		case 2: loc[r.quot] = (loc[r.quot] & 0xf3) | ((state & 0x3) << 2); return;
		case 3: loc[r.quot] = (loc[r.quot] & 0xfc) | (state & 0x3); return;
	}
}

static __inline__ int bits_get(char *loc, uint32_t index) {
	auto r = div(index, 4);
	return (loc[r.quot] >> ((3-r.rem) * 2)) & 0x3;
}

Vfs_GoFS::Vfs_GoFS(Vfs_Interface *parent, vfs_ino_t parent_ino, void **parent_context) {
	p_parent = parent;
	p_parent_ino = parent_ino;
	p_parent_context = parent_context;
	p_mounted = false;
}

vfs_ino_t Vfs_GoFS::lookup(vfs_ino_t parent, const char16_t *name, size_t name_len) {
	return -ENOSYS;
}

int Vfs_GoFS::format(const char16_t *name, size_t name_len) {
	if (name_len > 32) return -ENAMETOOLONG;
	if (p_mounted) umount();

	// read attributes of disk
	struct stat s;
	auto s_res = p_parent->getattr(p_parent_ino, &s, p_parent_context);
	if (s_res < 0) return s_res;

	memset(&sb, 0, sizeof(gofs_sb_t));

	// set block size
	p_blocksize = s.st_blksize;
	if (p_blocksize < 512) p_blocksize = 512;
	if (p_blocksize > 65536) p_blocksize = 65536;
	sb.sb_blocksize = VAL_BE32(p_blocksize);

	// compute block count
	int64_t blk_count = s.st_size / p_blocksize;
	if (blk_count * p_blocksize != s.st_size) {
		printf("WARNING: Size of disk is not a multiple of block size!\n");
	}

	// compute journal size
	int64_t journal_size = s.st_size / 100;
	if (journal_size > 0x8000000) journal_size = 0x8000000;
	if (journal_size < (2*p_blocksize)) journal_size = p_blocksize*2;

	// fill in some info
	sb.sb_next_ag = VAL_BE32(1);
	sb.sb_inodesize = VAL_BE16(256);
	sb.sb_journal_length = VAL_BE64(journal_size / p_blocksize); // 128MB
	memcpy(sb.sb_disk_name, name, name_len * sizeof(char16_t));

	int16_t ino_per_block = p_blocksize / VAL_BE16(sb.sb_inodesize);
	if ((uint64_t)ino_per_block * VAL_BE16(sb.sb_inodesize) != p_blocksize) {
		printf("ERROR: block size not a multiple of inode size");
		return -EINVAL;
	}

	// make sure each AG size is no higher than 1TB, or 0x10000000000
	int64_t max_blocks_per_ag = 0x10000000000 / p_blocksize;
	auto number_of_ag_div = ldiv(blk_count, max_blocks_per_ag);
	int64_t number_of_ag = number_of_ag_div.quot + (number_of_ag_div.rem?1:0); // unless we have an exact match, this will need to be +1'd

	// if we only have one ag, but got more than 10k blocks, force two ag
	if ((number_of_ag == 1) && (blk_count > 10000))
		number_of_ag = 2;

	int64_t blocks_per_ag = blk_count / number_of_ag; // make all ag the same size. For example 1.5TB disk will have two 750GB ag

	printf("disk is %ld Kbytes (%ld blocks)\n", s.st_size/1024, blk_count);
	printf("block size: %u bytes\n", p_blocksize);

	printf("number of ag: %ld\n", number_of_ag);

	for(int64_t i = 0; i < number_of_ag; i++) {
		if (i != number_of_ag - 1) {
			create_ag(i, blocks_per_ag * i, blocks_per_ag, blocks_per_ag * (i+1));
		} else {
			create_ag(i, blocks_per_ag * i, blk_count - (blocks_per_ag * i), 0); // all remaining blocks
		}
	}

	mount();

	// create root inode
	gofs_in_t root_ino;
	memset(&root_ino, 0, sizeof(gofs_in_t));
	root_ino.in_magic = GOFS_INO_MAGIC;
	root_ino.in_mode = S_IFDIR | 0755;
	root_ino.in_version = 1;
	root_ino.in_format = GOFS_INODE_FORMAT_EMPTY;
	root_ino.in_nlink = 1;
	root_ino.in_uid = 0;
	root_ino.in_gid = 0;
// TODO	root_ino.in_atime = 0;
	root_ino.in_size = 0;
	root_ino.in_nblocks = 0;
	root_ino.in_flags = 0;
	root_ino.in_gen = 0;

	// s.st_blksize
	// s.st_size
	printf("format done\n");
	umount();
	return -EINPROGRESS;
}

void Vfs_GoFS::create_ag(uint32_t ag_num, gofs_blk_t start_block, gofs_blk_t length, gofs_blk_t next) {
	gofs_ag_t ag;
	memset(&ag, 0, sizeof(gofs_ag_t));

	printf("ag length: %ld\n", length);

	// compute bitmap size
	auto div_res = ldiv(length, 4);
	int64_t bitmap_size = div_res.quot + (div_res.rem?1:0);
	printf("ag bitmap length: %ld\n", bitmap_size);
	div_res = ldiv(bitmap_size, p_blocksize);
	int64_t bitmap_size_blocks = div_res.quot + (div_res.rem?1:0);
	printf("ag bitmap length: %ld blocks\n", bitmap_size_blocks);

	int64_t reserved = 1 + bitmap_size_blocks;

	if (ag_num == 0) {
		// need journal
		if (sb.sb_journal_length) {
			sb.sb_journal_start = reserved;
			reserved += VAL_BE64(sb.sb_journal_length);
		}
		sb.ag.ag_magic = GOFS_AG_HEADER_MAGIC;
		sb.ag.ag_num = 0; // root ag
		sb.ag.ag_free_blocks = VAL_BE64(length - reserved);
		sb.ag.ag_reserved_blocks = VAL_BE64(reserved);
		sb.ag.ag_next = VAL_BE64(next);

		write_block(start_block, (char*)&sb, sizeof(sb));
	} else {
		ag.ag_magic = GOFS_AG_HEADER_MAGIC;
		ag.ag_num = VAL_BE32(ag_num);
		ag.ag_free_blocks = VAL_BE32(length - reserved);
		ag.ag_reserved_blocks = VAL_BE64(reserved);
		ag.ag_next = VAL_BE64(next);

		write_block(start_block, (char*)&ag, sizeof(ag));
	}

	// make & write bitmap
	char *bitmap_block = (char*)malloc(p_blocksize);
	for(int64_t i = 0; i < bitmap_size_blocks; i++) {
		if (i * p_blocksize * 4 > reserved) {
			// only zeroes!
			memset(bitmap_block, 0, p_blocksize);
			write_block(start_block + 1 + i, bitmap_block, p_blocksize);
			continue;
		}
		if ((i+1) * p_blocksize * 4 < reserved) {
			// only ff
			memset(bitmap_block, 0xff, p_blocksize);
			write_block(start_block + 1 + i, bitmap_block, p_blocksize);
			continue;
		}
		// mix of
		memset(bitmap_block, 0, p_blocksize);
		int num_of_bits = reserved % (p_blocksize * 4); // number of bits to set to 1
		for(int i = 0; i < num_of_bits; i++) {
			// set this bit to 1
			bits_set(bitmap_block, i, GOFS_BLOCK_RESERVED);
		}
		write_block(start_block + 1 + i, bitmap_block, p_blocksize);
	}
	free(bitmap_block);
}

void Vfs_GoFS::write_block(gofs_blk_t block, char *buf, size_t buf_size) {
	if (buf_size > p_blocksize) {
		printf("trying to write to block %lu more data than acceptable, giving up", block);
		abort();
	}

	p_parent->write(p_parent_ino, buf, buf_size, block * p_blocksize, p_parent_context);
}

void Vfs_GoFS::read_block(gofs_blk_t block, char *buf, size_t buf_size) {
	if (buf_size > p_blocksize) {
		printf("trying to read from block %lu more data than acceptable, giving up", block);
		abort();
	}

	p_parent->read(p_parent_ino, buf, buf_size, block * p_blocksize, p_parent_context);
}

int Vfs_GoFS::mount() {
	if (p_mounted) umount();
	read_block(0, (char*)&sb, sizeof(sb));

	if (sb.ag.ag_magic != GOFS_AG_HEADER_MAGIC)
		return -EINVAL;

	// TODO more checks
	p_blocksize = VAL_BE32(sb.sb_blocksize);
	p_mounted = true;
	return 0;
}

int Vfs_GoFS::umount() {
	if (!p_mounted) return -EINVAL;

	p_mounted = false; // TODO flush buffers, etc
	return 0;
}

