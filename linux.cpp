#include "GoFS.hpp"
#include <sys/types.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <errno.h>

Vfs_Linux_Block::Vfs_Linux_Block(const char *filename) {
	p_refcount = 1;
	p_fd = ::open(filename, O_RDWR | O_CREAT, 0755);
	if (p_fd == -1) {
		perror("open");
		abort();
	}
}

vfs_ino_t Vfs_Linux_Block::lookup(vfs_ino_t parent, const char *name, size_t name_len) {
	return -ENOSYS;
}

int Vfs_Linux_Block::read(vfs_ino_t ino, char *buffer, size_t size, off_t off, void **context) {
	if (ino != 0) return -EBADF;
	auto seek_res = ::lseek(p_fd, off, SEEK_SET);
	if (seek_res == -1) return -errno;
	int res = ::read(p_fd, buffer, size);
	if (res == -1) return -errno;
	return res;
}

int Vfs_Linux_Block::write(vfs_ino_t ino, char *buffer, size_t size, off_t off, void **context) {
	if (ino != 0) return -EBADF;
	auto seek_res = ::lseek(p_fd, off, SEEK_SET);
	if (seek_res == -1) return -errno;
	int res = ::write(p_fd, buffer, size);
	if (res == -1) return -errno;
	return res;
}

int Vfs_Linux_Block::fsync(vfs_ino_t ino, void **context) {
	if (ino != 0) return -EBADF;
	if (::fsync(p_fd) == -1) return -errno;
	return 0;
}

int Vfs_Linux_Block::getattr(vfs_ino_t ino, struct stat *s, void **) {
	if (ino != 0) return -EBADF;
	if (::fstat(p_fd, s) == -1) return -errno;
	return -1;
}

