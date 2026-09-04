# Makefile for Rust project

.PHONY: all build test check clean fmt lint

all: build

build:
	cargo build --release

test:
	cargo test

check:
	cargo check

clean:
	cargo clean

fmt:
	cargo fmt

lint:
	cargo clippy -- -D warnings
