#!/bin/make

CXX=g++
CXXFLAGS=-Wall -O0 -pipe --std=c++11
COMMON=linux.o vfs.o gofs.o

all: mkfs.gofs

mkfs.gofs: $(COMMON) mkfs.o
	$(CXX) $(CXXFLAGS) -o $@ $^

clean:
	$(RM) $(COMMON) mkfs.o
