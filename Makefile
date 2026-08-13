IMAGE ?= agentdesktop-controller:dev

.PHONY: build test check lint ui ui-check desktop desktop-check format gen generate-schema docker clean

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
	cd ui && pnpm build

ui-check:
	cd ui && pnpm install --frozen-lockfile
	cd ui && pnpm check

desktop:
	cd desktop && npm ci
	cd desktop && npm run build
	cargo build -p agentdesktop-ui

desktop-check:
	cd desktop && npm ci
	cd desktop && npm run build
	cargo check -p agentdesktop-ui

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
	rm -rf desktop/dist
