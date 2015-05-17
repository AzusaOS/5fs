#pragma once

#include "vfs.hpp"

class Vfs_GoFS: public Vfs_Interface {
public:
	Vfs_GoFS(Vfs_Interface *parent, vfs_ino_t parent_ino, void **parent_context);

	virtual vfs_ino_t lookup(vfs_ino_t parent, const char16_t *name, size_t name_len);
	virtual int format(const char16_t *name, size_t name_len);

private:
	Vfs_Interface *p_parent;
	vfs_ino_t p_parent_ino;
	void **p_parent_context;
};

