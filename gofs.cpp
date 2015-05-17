#include "gofs.hpp"
#include <stdio.h>
#include <errno.h>
#include <string.h>
#ifdef __APPLE__
#include <machine/endian.h>
#else
#include <endian.h>
#endif

#if __BYTE_ORDER == __BIG_ENDIAN
#define VAL_BE32(_x) (_x)
#else
#define VAL_BE32(_x) (__builtin_bswap32(_x))
#endif

Vfs_GoFS::Vfs_GoFS(Vfs_Interface *parent, vfs_ino_t parent_ino, void **parent_context) {
	p_parent = parent;
	p_parent_ino = parent_ino;
	p_parent_context = parent_context;
}

vfs_ino_t Vfs_GoFS::lookup(vfs_ino_t parent, const char16_t *name, size_t name_len) {
	return -ENOSYS;
}

int Vfs_GoFS::format(const char16_t *name, size_t name_len) {
	struct stat s;
	auto s_res = p_parent->getattr(p_parent_ino, &s, p_parent_context);
	if (s_res < 0) return s_res;

	if (name_len > 32) return -ENAMETOOLONG;

	int64_t blk_count = s.st_size / s.st_blksize;
	if (blk_count * s.st_blksize != s.st_size) {
		printf("WARNING: Size of disk is not a multiple of block size!\n");
	}

	gofs_sb_t sb;
	memset(&sb, 0, sizeof(gofs_sb_t));
	sb.ag.ag_magic = GOFS_AG_HEADER_MAGIC;
	sb.ag.ag_num = 0; // root ag
	sb.ag.ag_dblocks = 0; // ?
	sb.sb_blocksize = s.st_blksize;
	sb.sb_next_ag = 1;
	sb.sb_inodesize = 256;
	memcpy(sb.sb_disk_name, name, name_len);

	int16_t ino_per_block = sb.sb_blocksize / sb.sb_inodesize;
	if (ino_per_block * sb.sb_inodesize != sb.sb_blocksize) {
		printf("ERROR: block size not a multiple of inode size");
		return -EINVAL;
	}

	// make sure each AG size is no higher than 1TB, or 0x3fffffff
	int64_t max_blocks_per_ag = 0x40000000 / sb.sb_blocksize;
	int64_t number_of_ag = blk_count / max_blocks_per_ag; // unless we have an exact match, this will need to be +1'd
	if (blk_count % max_blocks_per_ag) number_of_ag += 1;
	int64_t blocks_per_ag = blk_count / number_of_ag; // make all ag the same size. For example 1.5TB disk will have two 750GB ag

	printf("disk is %ld bytes (%ld blocks)\n", s.st_size, blk_count);
	printf("block size: %ld bytes\n", s.st_blksize);

	printf("struct size: %d\n", sizeof(gofs_sb_t));
	printf("inode size: %d\n", sizeof(gofs_in_t));

	printf("number of ag: %ld\n", number_of_ag);

	// s.st_blksize
	// s.st_size
	printf("format\n");
	return -EINPROGRESS;
}
