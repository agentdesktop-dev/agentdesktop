IMAGE ?= agentdesktop-controller:dev
BUILD_PROFILE ?= release

.PHONY: build install test check lint frontend frontend-check desktop desktop-dev desktop-check format gen generate-schema docker clean

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

desktop-dev:
	@set -eu; \
	dev_state="$${AGENTDESKTOP_DEV_STATE:-$${XDG_STATE_HOME:-$$HOME/.local/state}/agentdesktop-dev}"; \
	dev_socket="$${AGENTDESKTOP_SOCKET:-$$dev_state/agentdesktop.sock}"; \
	dev_config="$${AGENTDESKTOP_DEV_CONFIG:-$${XDG_CONFIG_HOME:-$$HOME/.config}/agentdesktop/config.yaml}"; \
	mkdir -p "$$dev_state"; \
	cargo build -p agentdesktop; \
	target/debug/agentdesktop --socket "$$dev_socket" daemon \
		--user \
		--config "$$dev_config" \
		--state-dir "$$dev_state" & \
	daemon_pid=$$!; \
	cleanup() { \
		kill "$$daemon_pid" 2>/dev/null || true; \
		wait "$$daemon_pid" 2>/dev/null || true; \
	}; \
	trap cleanup EXIT; \
	trap 'exit 130' INT; \
	trap 'exit 143' TERM; \
	attempt=0; \
	until target/debug/agentdesktop --socket "$$dev_socket" status >/dev/null 2>&1; do \
		if ! kill -0 "$$daemon_pid" 2>/dev/null; then \
			echo "Agentdesktop development daemon exited before becoming ready." >&2; \
			wait "$$daemon_pid"; \
			exit 1; \
		fi; \
		attempt=$$((attempt + 1)); \
		if [ "$$attempt" -ge 100 ]; then \
			echo "Timed out waiting for Agentdesktop development daemon." >&2; \
			exit 1; \
		fi; \
		sleep 0.1; \
	done; \
	AGENTDESKTOP_SOCKET="$$dev_socket" pnpm --dir frontend dev:desktop

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
