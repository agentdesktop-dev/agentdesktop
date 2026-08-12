IMAGE ?= agentdesktop-controller:dev

.PHONY: build test check lint ui ui-check format gen generate-schema docker clean

build: ui
	cargo build --workspace

test: ui
	cargo test --workspace

check: lint ui-check

lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

ui:
	cd ui && pnpm install --frozen-lockfile
	cd ui && pnpm frontend:build

ui-check:
	cd ui && pnpm install --frozen-lockfile
	cd ui && pnpm check

format:
	cargo fmt --all
	cd ui && pnpm format

gen: generate-schema format
	@:

generate-schema:
	cargo xtask schema

docker:
	docker build --tag $(IMAGE) .

clean:
	cargo clean
	rm -rf ui/dist
