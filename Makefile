.PHONY: help build-factory build-pool build-manager build-all test \
        build-release deploy-help \
        upload-farm upload-vault instantiate-farm-factory deploy-farm \
        upload-admin-timelock instantiate-admin-timelock deploy-admin-timelock

# Flags to fix "Wasm bytecode could not be deserialized" and reduce size
RUST_FLAGS = "-C link-arg=-s -C target-feature=-bulk-memory"

# NETWORK is consumed by every deploy/ script via deploy/lib.sh.
# Default to testnet for safety — mainnet must be opted into explicitly.
NETWORK ?= testnet

help:
	@echo "Build targets:"
	@echo "  build-all                 Build CLMM wasm artifacts (factory + pool + manager)"
	@echo "  build-factory             Build CLMM factory only"
	@echo "  build-pool                Build CLMM pool only"
	@echo "  build-manager             Build CLMM manager only"
	@echo "  build-zap-lp              Build choice_zap_lp"
	@echo "  build-mts-issuer          Build choice_mts_issuer"
	@echo "  build-pool-seeder         Build choice_pool_seeder"
	@echo "  build-release             Docker workspace-optimizer (all contracts, optimised)"
	@echo "  test                      build-all + integration tests"
	@echo ""
	@echo "Deploy targets (pass NETWORK=testnet|mainnet — default: testnet):"
	@echo "  upload-admin-timelock      Store choice_admin_timelock wasm"
	@echo "  instantiate-admin-timelock Instantiate the timelock (requires TIMELOCK_CODE_ID)"
	@echo "  deploy-admin-timelock      Convenience: upload + instantiate the timelock"
	@echo "  upload-farm                Store choice_farm + choice_farm_factory wasms"
	@echo "  instantiate-farm-factory   Instantiate the factory (requires FARM_CODE_ID + FACTORY_CODE_ID)"
	@echo "  deploy-farm                Convenience: upload-farm + instantiate-farm-factory in one pass"
	@echo "  upload-vault               Store choice_vault wasm"
	@echo "  deploy-help                Same as this section"
	@echo ""
	@echo "Examples:"
	@echo "  make upload-farm"
	@echo "  make upload-farm NETWORK=mainnet"
	@echo "  make instantiate-farm-factory FARM_CODE_ID=123 FACTORY_CODE_ID=124"
	@echo "  make deploy-farm NETWORK=mainnet OWNER=inj1... FEE_COLLECTOR=inj1..."
	@echo "  make deploy-admin-timelock NETWORK=mainnet OWNER=inj1...multisig... TIMELOCK_SECONDS=172800"

deploy-help: help

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

build-factory:
	RUSTFLAGS=$(RUST_FLAGS) cargo build --release --lib --target wasm32-unknown-unknown -p choice_clmm_factory
	cp target/wasm32-unknown-unknown/release/choice_clmm_factory.wasm artifacts/

build-pool:
	RUSTFLAGS=$(RUST_FLAGS) cargo build --release --lib --target wasm32-unknown-unknown -p choice_clmm_pool
	cp target/wasm32-unknown-unknown/release/choice_clmm_pool.wasm artifacts/

build-manager:
	RUSTFLAGS=$(RUST_FLAGS) cargo build --release --lib --target wasm32-unknown-unknown -p choice_clmm_manager
	cp target/wasm32-unknown-unknown/release/choice_clmm_manager.wasm artifacts/

build-zap-lp:
	RUSTFLAGS=$(RUST_FLAGS) cargo build --release --lib --target wasm32-unknown-unknown -p choice-zap-lp
	cp target/wasm32-unknown-unknown/release/choice_zap_lp.wasm artifacts/

build-mts-issuer:
	RUSTFLAGS=$(RUST_FLAGS) cargo build --release --lib --target wasm32-unknown-unknown -p choice-mts-issuer
	cp target/wasm32-unknown-unknown/release/choice_mts_issuer.wasm artifacts/

build-pool-seeder:
	RUSTFLAGS=$(RUST_FLAGS) cargo build --release --lib --target wasm32-unknown-unknown -p choice-pool-seeder
	cp target/wasm32-unknown-unknown/release/choice_pool_seeder.wasm artifacts/

build-all: build-factory build-pool build-manager

# Full optimised build of every workspace contract via the Docker
# workspace-optimizer. Slow (~5–10 min) but produces the artifacts the
# deploy scripts upload to chain.
build-release:
	./build_release.sh

test: build-all
	cargo test --test integration

# ---------------------------------------------------------------------------
# Deploy
# ---------------------------------------------------------------------------

upload-farm:
	NETWORK=$(NETWORK) ./deploy/upload_farm_code.sh

upload-admin-timelock:
	NETWORK=$(NETWORK) ./deploy/upload_admin_timelock.sh

# Requires TIMELOCK_CODE_ID. OWNER, TIMELOCK_SECONDS, LABEL, ADMIN are
# optional and forwarded if set.
instantiate-admin-timelock:
	@if [ -z "$(TIMELOCK_CODE_ID)" ]; then \
		echo "ERROR: TIMELOCK_CODE_ID is required."; \
		echo "       Run 'make upload-admin-timelock' first to get it."; \
		exit 1; \
	fi
	NETWORK=$(NETWORK) \
	TIMELOCK_CODE_ID=$(TIMELOCK_CODE_ID) \
	$(if $(OWNER),OWNER=$(OWNER),) \
	$(if $(TIMELOCK_SECONDS),TIMELOCK_SECONDS=$(TIMELOCK_SECONDS),) \
	$(if $(LABEL),LABEL=$(LABEL),) \
	$(if $(ADMIN),ADMIN=$(ADMIN),) \
	./deploy/instantiate_admin_timelock.sh

# Convenience: upload then instantiate the admin timelock. Captures the
# code id from upload_admin_timelock.sh's machine-readable marker and feeds
# it into the instantiate step.
deploy-admin-timelock:
	@echo "==> Uploading admin timelock wasm (NETWORK=$(NETWORK))..."
	@set -e; \
	upload_output=$$(NETWORK=$(NETWORK) ./deploy/upload_admin_timelock.sh); \
	echo "$$upload_output"; \
	timelock_id=$$(echo "$$upload_output" | sed -n 's/^DEPLOY_CAPTURE_TIMELOCK_CODE_ID=//p'); \
	if [ -z "$$timelock_id" ]; then \
		echo "ERROR: failed to parse timelock code id from upload output."; \
		exit 1; \
	fi; \
	echo ""; \
	echo "==> Instantiating admin timelock with TIMELOCK_CODE_ID=$$timelock_id..."; \
	NETWORK=$(NETWORK) \
	TIMELOCK_CODE_ID=$$timelock_id \
	$(if $(OWNER),OWNER=$(OWNER),) \
	$(if $(TIMELOCK_SECONDS),TIMELOCK_SECONDS=$(TIMELOCK_SECONDS),) \
	$(if $(LABEL),LABEL=$(LABEL),) \
	$(if $(ADMIN),ADMIN=$(ADMIN),) \
	./deploy/instantiate_admin_timelock.sh

upload-vault:
	NETWORK=$(NETWORK) ./deploy/upload_vault_code.sh

# Requires FARM_CODE_ID and FACTORY_CODE_ID. OWNER, FEE_COLLECTOR,
# FARM_OWNER, FEE_INJ_BASE, LABEL, ADMIN are optional and forwarded if set.
instantiate-farm-factory:
	@if [ -z "$(FARM_CODE_ID)" ] || [ -z "$(FACTORY_CODE_ID)" ]; then \
		echo "ERROR: FARM_CODE_ID and FACTORY_CODE_ID are required."; \
		echo "       Run 'make upload-farm' first to get them."; \
		exit 1; \
	fi
	NETWORK=$(NETWORK) \
	FARM_CODE_ID=$(FARM_CODE_ID) \
	FACTORY_CODE_ID=$(FACTORY_CODE_ID) \
	$(if $(OWNER),OWNER=$(OWNER),) \
	$(if $(FEE_COLLECTOR),FEE_COLLECTOR=$(FEE_COLLECTOR),) \
	$(if $(FARM_OWNER),FARM_OWNER=$(FARM_OWNER),) \
	$(if $(FEE_INJ_BASE),FEE_INJ_BASE=$(FEE_INJ_BASE),) \
	$(if $(LABEL),LABEL=$(LABEL),) \
	$(if $(ADMIN),ADMIN=$(ADMIN),) \
	./deploy/instantiate_farm_factory.sh

# Convenience: upload then instantiate. Captures the two code ids from
# upload_farm_code.sh's machine-readable marker lines and feeds them into
# the instantiate step.
deploy-farm:
	@echo "==> Uploading farm + factory wasms (NETWORK=$(NETWORK))..."
	@set -e; \
	upload_output=$$(NETWORK=$(NETWORK) ./deploy/upload_farm_code.sh); \
	echo "$$upload_output"; \
	farm_id=$$(echo "$$upload_output" | sed -n 's/^DEPLOY_CAPTURE_FARM_CODE_ID=//p'); \
	factory_id=$$(echo "$$upload_output" | sed -n 's/^DEPLOY_CAPTURE_FACTORY_CODE_ID=//p'); \
	if [ -z "$$farm_id" ] || [ -z "$$factory_id" ]; then \
		echo "ERROR: failed to parse code ids from upload output."; \
		exit 1; \
	fi; \
	echo ""; \
	echo "==> Instantiating factory with FARM_CODE_ID=$$farm_id FACTORY_CODE_ID=$$factory_id..."; \
	NETWORK=$(NETWORK) \
	FARM_CODE_ID=$$farm_id \
	FACTORY_CODE_ID=$$factory_id \
	$(if $(OWNER),OWNER=$(OWNER),) \
	$(if $(FEE_COLLECTOR),FEE_COLLECTOR=$(FEE_COLLECTOR),) \
	$(if $(FARM_OWNER),FARM_OWNER=$(FARM_OWNER),) \
	$(if $(FEE_INJ_BASE),FEE_INJ_BASE=$(FEE_INJ_BASE),) \
	$(if $(LABEL),LABEL=$(LABEL),) \
	$(if $(ADMIN),ADMIN=$(ADMIN),) \
	./deploy/instantiate_farm_factory.sh
