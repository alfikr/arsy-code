# Makefile for Rust project

.PHONY: all build run test banner check clean fmt lint

all: build

build:
	cargo build --release

run:
	cargo run -p arsy-cli --features tui

test:
	cargo test --workspace --all-features --locked

banner:
	cargo build -p arsy-cli --features tui
	sh crates/arsy-cli/tests/banner_test.sh

check:
	cargo check

clean:
	cargo clean

fmt:
	cargo fmt --all

lint:
	cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
