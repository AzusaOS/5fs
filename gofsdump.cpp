#include "gofs.hpp"
#include <stdio.h>
#include <locale>
#include <codecvt>

void dump_ag(uint32_t ag_num, const gofs_ag_t *ag) {
	printf("ag[%u].ag_magic = 0x%x\n", ag_num, GOFS_BE32(ag->ag_magic));
	printf("ag[%u].ag_num = %u\n", ag_num, GOFS_BE32(ag->ag_num));
	printf("ag[%u].ag_length = %u blocks\n", ag_num, GOFS_BE32(ag->ag_length));
	printf("ag[%u].ag_free_blocks = %u blocks\n", ag_num, GOFS_BE32(ag->ag_free_blocks));
	printf("ag[%u].ag_rsvd_blocks = %u blocks\n", ag_num, GOFS_BE32(ag->ag_rsvd_blocks));
	printf("ag[%u].ag_part_blocks = %u blocks\n", ag_num, GOFS_BE32(ag->ag_part_blocks));
	printf("ag[%u].ag_full_blocks = %u blocks\n", ag_num, GOFS_BE32(ag->ag_full_blocks));
	printf("ag[%u].ag_this = %lu\n", ag_num, GOFS_BE64(ag->ag_this));
	printf("ag[%u].ag_next = %lu\n", ag_num, GOFS_BE64(ag->ag_next));
	printf("ag[%u].ag_data_alloc_pos = %u\n", ag_num, GOFS_BE32(ag->ag_data_alloc_pos));
	printf("ag[%u].ag_ino_alloc_pos = %u\n", ag_num, GOFS_BE32(ag->ag_ino_alloc_pos));
}

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

	auto sb = f->superBlock();
	printf("sb.sb_blocksize = %u\n", GOFS_BE32(sb->sb_blocksize));
	printf("sb.sb_next_ag = %u\n", GOFS_BE32(sb->sb_next_ag));
	printf("sb.sb_free_blocks = %lu blocks\n", GOFS_BE64(sb->sb_free_blocks));
	printf("sb.sb_rsvd_blocks = %lu blocks\n", GOFS_BE64(sb->sb_rsvd_blocks));
	printf("sb.sb_part_blocks = %lu blocks\n", GOFS_BE64(sb->sb_part_blocks));
	printf("sb.sb_full_blocks = %lu blocks\n", GOFS_BE64(sb->sb_full_blocks));
	printf("sb.sb_root_ino = %lu\n", GOFS_BE64(sb->sb_root_ino));
	printf("sb.sb_journal_start = %lu\n", GOFS_BE64(sb->sb_journal_start));
	printf("sb.sb_journal_length = %lu blocks\n", GOFS_BE64(sb->sb_journal_length));
	printf("sb.sb_kernel_offset = %lu\n", GOFS_BE64(sb->sb_kernel_offset));
	printf("sb.sb_kernel_end = %lu\n", GOFS_BE64(sb->sb_kernel_end));
	printf("sb.sb_flags = %lu\n", GOFS_BE64(sb->sb_flags));
	printf("sb.sb_inodesize = %u bytes\n", GOFS_BE16(sb->sb_inodesize));
	printf("sb.sb_disk_name = \"%s\"\n", convert.to_bytes(sb->sb_disk_name).c_str());
	dump_ag(0, &sb->ag);
	return 0;
}
