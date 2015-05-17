#include "GoFS.hpp"
#include <errno.h>

Vfs_GoFS::Vfs_GoFS(Vfs_Interface *parent, vfs_ino_t parent_ino, void **parent_context) {
	p_parent = parent;
	p_parent_ino = parent_ino;
	p_parent_context = parent_context;
}

vfs_ino_t Vfs_GoFS::lookup(vfs_ino_t parent, const char *name, size_t name_len) {
	return -ENOSYS;
}

