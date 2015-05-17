#include "gofs.hpp"
#include <stdio.h>
#include <errno.h>
#include <endian.h>

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

	int64_t blk_count = s.st_size / s.st_blksize;

	if (blk_count * s.st_blksize != s.st_size) {
		printf("WARNING: Size of disk is not a multiple of block size!\n");
	}

	printf("disk is %ld bytes (%ld blocks)\n", s.st_size, blk_count);
	printf("block size: %ld bytes\n", s.st_blksize);

	printf("struct size: %d\n", sizeof(gofs_sb_t));
	printf("inode size: %d\n", sizeof(gofs_in_t));

	// s.st_blksize
	// s.st_size
	printf("format\n");
	return -EINPROGRESS;
}
