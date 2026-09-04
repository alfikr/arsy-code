# Makefile for Rust project

.PHONY: all build run test check clean fmt lint

all: build

build:
	cargo build --release

run:
	cargo run -p arsy-cli --features tui

test:
	cargo test --workspace --all-features --locked

check:
	cargo check

clean:
	cargo clean

fmt:
	cargo fmt --all

lint:
	cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
