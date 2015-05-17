#include "GoFS.hpp"
#include <stdio.h>

int main(int argc, char *argv[]) {
	if (argc != 2) {
		printf("Usage: %s [filename]\n", argv[0]);
		return 1;
	}

	printf("%s: formatting %s\n", argv[0], argv[1]);
	auto b = new Vfs_Linux_Block(argv[1]);
	auto f = new Vfs_GoFS(b, 0, nullptr);
	f->format(VFS_STATIC_STRING("Empty"));
	return 0;
}

