# 5FS - 5OS FileSystem

Disk contains "allocation groups" of variable sizes.

Allocation groups descriptor is stored on allocation group zero.

Global inode number contain <32 bits allocation group id><32 bits local group id>

Inode number is actually inode offset on disk * inode size.

Default inode size = 256 bytes

Max allocation group size = max uint32 * 256 (max uint32 is 4G, so that'd be 1TB max)

Journal: 64MB, stored in each allocation group (beginning of AG)

## Allocation group format

* header
* optional AG0 header (cached data such as free space, etc)
* journal
* bitmap
* optionally reserved space if AG0 and kernel is present. 5FS guarantees that kernel file system/kernel.bin will be stored in continuous blocks. Exact offset is stored in AG0 header.

## Growing file system

Easy as pie: add a allocation group

## Shrinking file system

This needs to be planned in advance. It can be done by removing an allocation group (once it is empty).

