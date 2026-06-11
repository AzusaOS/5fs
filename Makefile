# Cargo forbids '.' in binary target names, so the build produces mkfs5fs
# etc. and this Makefile gives them their proper dotted names.

PREFIX ?= /usr/local
SBINDIR = $(PREFIX)/sbin
TOOLS = mkfs fsck debugfs mount

build:
	cargo build --release
	@mkdir -p bin
	@for t in $(TOOLS); do \
		if [ -f target/release/$${t}5fs ]; then \
			cp target/release/$${t}5fs bin/$$t.5fs; echo "bin/$$t.5fs"; \
		fi; \
	done

test:
	cargo test

# Heavy suites (200k files, long model runs) — minutes, not seconds.
stress:
	cargo test --release -- --ignored --nocapture --test-threads=2

# Regenerate the committed reference image + manifest (after intentional
# format changes only; compliance tests verify against these).
fixtures:
	GEN_FIXTURES=1 cargo test --release --test compliance generate_fixture -- --ignored --nocapture

install: build
	install -d $(DESTDIR)$(SBINDIR)
	@for t in $(TOOLS); do \
		if [ -f bin/$$t.5fs ]; then install -m 755 bin/$$t.5fs $(DESTDIR)$(SBINDIR)/; fi; \
	done

clean:
	cargo clean
	rm -rf bin

.PHONY: build test install clean
