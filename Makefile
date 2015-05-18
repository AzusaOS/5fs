#!/bin/make

CXX=g++
CXXFLAGS=-Wall -O0 -pipe --std=c++11
COMMON=linux.o vfs.o gofs.o
TOOLS=mkfs.gofs gofsdump
TOOLS_OBJECTS=$(patsubst %,%.o,$(TOOLS))

all: $(TOOLS)

-include $(wildcard *.d)

.SUFFIXES:

.PRECIOUS: %.o

%.o: %.cpp
	$(CXX) $(CXXFLAGS) -MD -c -o $@ $<

%: %.o $(COMMON)
	$(CXX) $(CXXFLAGS) -o $@ $^

clean:
	$(RM) $(COMMON) $(COMMON:.o=.d) $(TOOLS_OBJECTS) $(TOOLS_OBJECTS:.o=.d)

distclean:
	$(RM) $(COMMON) $(COMMON:.o=.d) $(TOOLS_OBJECTS) $(TOOLS_OBJECTS:.o=.d) $(TOOLS) disk.bin

disk.bin:
	@echo "Making 100MB disk"
	dd if=/dev/zero of=disk.bin bs=1048576 seek=100 count=0
	#dd if=/dev/zero of=disk.bin bs=1048576 seek=$$[ 1048576 * 6 ] count=0

.PHONY: test

test: mkfs.gofs gofsdump disk.bin
	./mkfs.gofs disk.bin
	./gofsdump disk.bin
