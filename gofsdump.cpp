#include "gofs.hpp"
#include <stdio.h>
#include <locale>
#include <codecvt>

int main(int argc, char *argv[]) {
	if (argc != 2) {
		printf("Usage: %s [filename]\n", argv[0]);
		return 1;
	}

	printf("%s: dumping %s\n", argv[0], argv[1]);
	std::wstring_convert<std::codecvt_utf8_utf16<char16_t>,char16_t> convert;
	auto b = new Vfs_Linux_Block(convert.from_bytes(argv[1]).c_str());
	auto f = new Vfs_GoFS(b, 0, nullptr);
	f->mount();
	return 0;
}
