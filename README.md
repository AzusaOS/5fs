# 5FS - 5OS FileSystem

Disk contains "allocation groups" of variable sizes.

Allocation groups descriptor is stored on allocation group zero (TODO create and define inode).

Global inode number contain <32 bits allocation group id><32 bits local group id>

Inode number is actually inode offset on disk * inode size.

Default inode size = 256 bytes

Max allocation group size = max uint32 * 256 (max uint32 is 4G, so that'd be 1TB max)

Journal: single journal, 128MB

## Bitmap

The bitmap stored in each AG contains the status (available or not) of each block. More than that, it also contains information if the block is free, partially filled, full or reserved.

Partially filled blocks can only be partially filled with inodes. In that case unused memory will be zero, while the inode will have the inode header ("IN").

Inodes can point to blocks of data outside of the current AG, but extra data (external index, B+ tables, etc) will be stored in the same AG.

## Allocation group format

* header
* optional AG0 header (cached data such as free space, etc - in same block as header)
* bitmap
* journal (if AG0)
* optionally reserved space if AG0 and kernel is present. 5FS guarantees that kernel file system/kernel.bin will be stored in continuous blocks. Exact offset is stored in AG0 header.

## Growing file system

Easy as pie: add a allocation group

## Shrinking file system

This needs to be planned in advance. It can be done by removing an allocation group (once it is empty).

