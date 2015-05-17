#pragma once
#include <stdint.h>
#include <unistd.h>
#include <fcntl.h>
#include <uchar.h>
#include <sys/stat.h>

typedef uint64_t vfs_ino_t;

#define VFS_STATIC_STRING(_x) _x, ((sizeof(_x)/2) - 1)

class Vfs_Interface {
public:
	virtual vfs_ino_t lookup(vfs_ino_t parent, const char16_t *name, size_t name_len) = 0;
	virtual void forget(vfs_ino_t, unsigned long nlookup);

	virtual int format(const char16_t *name, size_t name_len);

	virtual int mknod(vfs_ino_t parent, const char16_t *name, size_t name_len, mode_t mode, dev_t rdev, uid_t owner, gid_t group);
	virtual int mkdir(vfs_ino_t parent, const char16_t *name, size_t name_len);
	virtual int unlink(vfs_ino_t parent, const char16_t *name, size_t name_len);
	virtual int rmdir(vfs_ino_t parent, const char16_t *name, size_t name_len);

	virtual int symlink(vfs_ino_t parent, const char16_t *name, size_t name_len, const char16_t *link, size_t link_len);
	virtual ssize_t readlink(vfs_ino_t ino, char16_t *target, size_t len);
	virtual int rename(vfs_ino_t parent, const char16_t *name, size_t name_len, vfs_ino_t new_parent, const char16_t *new_name, size_t new_name_len);
	virtual int link(vfs_ino_t ino, vfs_ino_t new_parent, const char16_t *new_name, size_t new_name_len);

	virtual int create(vfs_ino_t parent, const char16_t *name, size_t name_len, mode_t mode, uid_t owner, gid_t group, vfs_ino_t *ino, void **context);
	virtual int open(vfs_ino_t ino, void **context);
	virtual int read(vfs_ino_t ino, char16_t *buffer, size_t size, off_t off, void **context);
	virtual int write(vfs_ino_t ino, char16_t *buffer, size_t size, off_t off, void **context);
	virtual int flush(vfs_ino_t ino, void **context); // flush write buffers to detect any pending write error (space, etc)
	virtual int release(vfs_ino_t ino, void **context);
	virtual int fsync(vfs_ino_t ino, void **context); // force writing to disk now

	virtual int getattr(vfs_ino_t ino, struct stat *, void **context);
	virtual int setattr(vfs_ino_t ino, const struct stat *, void **context);

	virtual int opendir(vfs_ino_t ino, void **context);
	virtual int readdir(vfs_ino_t ino, char16_t *buffer, size_t size, off_t off, void **context);
	virtual int releasedir(vfs_ino_t ino, void **context);
	virtual int fsyncdir(vfs_ino_t ino, void **context);

	virtual int statfs(vfs_ino_t ino, struct statfs *);
	virtual int ioctl(vfs_ino_t ino, int cmd, void *arg, unsigned flags, const void *in_buf, size_t in_bufsz, size_t out_bufsz, void **context);
	virtual int fallocate(vfs_ino_t ino, int mode, off_t offset, off_t length, void **context);
};

class Vfs_Linux_Block: public Vfs_Interface {
public:
	Vfs_Linux_Block(const char16_t *filename);

	virtual vfs_ino_t lookup(vfs_ino_t parent, const char16_t *name, size_t name_len);
	virtual int read(vfs_ino_t ino, char16_t *buffer, size_t size, off_t off, void **context);
	virtual int write(vfs_ino_t ino, char16_t *buffer, size_t size, off_t off, void **context);
	virtual int fsync(vfs_ino_t ino, void **context);
	virtual int getattr(vfs_ino_t ino, struct stat *, void **context);

private:
	int p_refcount;
	int p_fd;
};

