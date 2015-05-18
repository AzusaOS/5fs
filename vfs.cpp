#include "vfs.hpp"
#include <errno.h>

void Vfs_Interface::forget(vfs_ino_t, unsigned long nlookup) {
}

int Vfs_Interface::mount() {
	return -ENOSYS;
}

int Vfs_Interface::umount() {
	return -ENOSYS;
}

int Vfs_Interface::format(const char16_t *name, size_t name_len) {
	return -ENOSYS;
}

int Vfs_Interface::mknod(vfs_ino_t parent, const char16_t *name, size_t name_len, mode_t mode, dev_t rdev, uid_t owner, gid_t group) {
	return -ENOSYS;
}

int Vfs_Interface::mkdir(vfs_ino_t parent, const char16_t *name, size_t name_len) {
	return -ENOSYS;
}

int Vfs_Interface::unlink(vfs_ino_t parent, const char16_t *name, size_t name_len) {
	return -ENOSYS;
}

int Vfs_Interface::rmdir(vfs_ino_t parent, const char16_t *name, size_t name_len) {
	return -ENOSYS;
}


int Vfs_Interface::symlink(vfs_ino_t parent, const char16_t *name, size_t name_len, const char16_t *link, size_t link_len) {
	return -ENOSYS;
}

ssize_t Vfs_Interface::readlink(vfs_ino_t ino, char16_t *target, size_t len) {
	return -ENOSYS;
}

int Vfs_Interface::rename(vfs_ino_t parent, const char16_t *name, size_t name_len, vfs_ino_t new_parent, const char16_t *new_name, size_t new_name_len) {
	return -ENOSYS;
}

int Vfs_Interface::link(vfs_ino_t ino, vfs_ino_t new_parent, const char16_t *new_name, size_t new_name_len) {
	return -ENOSYS;
}


int Vfs_Interface::create(vfs_ino_t parent, const char16_t *name, size_t name_len, mode_t mode, uid_t owner, gid_t group, vfs_ino_t *ino, void **context) {
	return -ENOSYS;
}

int Vfs_Interface::open(vfs_ino_t ino, void **context) {
	return -ENOSYS;
}

int Vfs_Interface::read(vfs_ino_t ino, char *buffer, size_t size, off_t off, void **context) {
	return -ENOSYS;
}

int Vfs_Interface::write(vfs_ino_t ino, char *buffer, size_t size, off_t off, void **context) {
	return -ENOSYS;
}

int Vfs_Interface::flush(vfs_ino_t ino, void **context) { // flush write buffers to detect any pending write error (space, etc)
	return -ENOSYS;
}

int Vfs_Interface::release(vfs_ino_t ino, void **context) {
	return -ENOSYS;
}

int Vfs_Interface::fsync(vfs_ino_t ino, void **context) { // force writing to disk now
	return -ENOSYS;
}


int Vfs_Interface::getattr(vfs_ino_t ino, struct stat *, void **context) {
	return -ENOSYS;
}

int Vfs_Interface::setattr(vfs_ino_t ino, const struct stat *, void **context) {
	return -ENOSYS;
}


int Vfs_Interface::opendir(vfs_ino_t ino, void **context) {
	return -ENOSYS;
}

int Vfs_Interface::readdir(vfs_ino_t ino, char16_t *buffer, size_t size, off_t off, void **context) {
	return -ENOSYS;
}

int Vfs_Interface::releasedir(vfs_ino_t ino, void **context) {
	return -ENOSYS;
}

int Vfs_Interface::fsyncdir(vfs_ino_t ino, void **context) {
	return -ENOSYS;
}


int Vfs_Interface::statfs(vfs_ino_t ino, struct statfs *) {
	return -ENOSYS;
}

int Vfs_Interface::ioctl(vfs_ino_t ino, int cmd, void *arg, unsigned flags, const void *in_buf, size_t in_bufsz, size_t out_bufsz, void **context) {
	return -ENOSYS;
}

int Vfs_Interface::fallocate(vfs_ino_t ino, int mode, off_t offset, off_t length, void **context) {
	return -ENOSYS;
}


