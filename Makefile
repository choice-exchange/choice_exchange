.PHONY: build-factory build-pool build-manager test

# Flags to fix "Wasm bytecode could not be deserialized" and reduce size
RUST_FLAGS="-C link-arg=-s -C target-feature=-bulk-memory"

build-factory:
	RUSTFLAGS=$(RUST_FLAGS) cargo build --release --lib --target wasm32-unknown-unknown -p choice_clmm_factory
	cp target/wasm32-unknown-unknown/release/choice_clmm_factory.wasm artifacts/

build-pool:
	RUSTFLAGS=$(RUST_FLAGS) cargo build --release --lib --target wasm32-unknown-unknown -p choice_clmm_pool
	cp target/wasm32-unknown-unknown/release/choice_clmm_pool.wasm artifacts/

build-manager:
	RUSTFLAGS=$(RUST_FLAGS) cargo build --release --lib --target wasm32-unknown-unknown -p choice_clmm_manager
	cp target/wasm32-unknown-unknown/release/choice_clmm_manager.wasm artifacts/

build-all: build-factory build-pool build-manager

test: build-all
	cargo test --test integration