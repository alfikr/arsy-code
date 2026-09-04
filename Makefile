.PHONY: run test

run:
	cargo run -p arsy-cli --features tui

test:
	cargo test --workspace --all-features --locked
