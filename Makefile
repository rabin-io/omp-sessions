BINDIR ?= $(HOME)/bin
BINARY := omp-sessions

.PHONY: build install test clean

build:
	cargo build --release

install: build
	install -d "$(BINDIR)"
	install -m 755 "target/release/$(BINARY)" "$(BINDIR)/$(BINARY)"

test:
	cargo test

clean:
	cargo clean
