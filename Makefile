#!/bin/make

CXX=g++
CXXFLAGS=-Wall -O0 -pipe --std=c++11
COMMON=linux.o vfs.o gofs.o

all: mkfs.gofs

-include $(wildcard *.d)

%.o: %.cpp
	$(CXX) $(CXXFLAGS) -MD -c -o $@ $<

mkfs.gofs: $(COMMON) mkfs.o
	$(CXX) $(CXXFLAGS) -o $@ $^

clean:
	$(RM) $(COMMON) mkfs.o

distclean:
	$(RM) $(COMMON) mkfs.o mkfs.gofs disk.bin

disk.bin:
	@echo "Making 100MB disk"
	dd if=/dev/zero of=disk.bin bs=1048576 seek=100 count=0

.PHONY: test

test: mkfs.gofs disk.bin
	./mkfs.gofs disk.bin
