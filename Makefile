IMAGE ?= agentdesktop-controller:dev
BUILD_PROFILE ?= release

.PHONY: build install test check lint frontend frontend-check desktop desktop-check format gen generate-schema docker clean

build: frontend
	cargo build --workspace

install: frontend
	cargo install --path crates/agentdesktop --locked --force
	cargo install --path crates/controller --locked --force

test: frontend
	cargo test --workspace

check: lint frontend-check

lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

frontend:
	cd frontend && pnpm install --frozen-lockfile
	cd frontend && pnpm build

frontend-check:
	cd frontend && pnpm install --frozen-lockfile
	cd frontend && pnpm check

desktop:
	cd frontend && pnpm install --frozen-lockfile
	cd frontend && pnpm --filter @agentdesktop/desktop-web build
	cargo build -p agentdesktop

desktop-check:
	cd frontend && pnpm install --frozen-lockfile
	cd frontend && pnpm --filter @agentdesktop/desktop-web build
	cargo check -p agentdesktop

format:
	cargo fmt --all
	cd frontend && pnpm format

gen: generate-schema format
	@:

generate-schema:
	cargo xtask schema

docker:
	docker build --build-arg BUILD_PROFILE=$(BUILD_PROFILE) --tag $(IMAGE) .

clean:
	cargo clean
	rm -rf frontend/controller/dist
	rm -rf frontend/controller/storybook-static
	rm -rf frontend/desktop/dist
	rm -rf frontend/desktop/storybook-static
