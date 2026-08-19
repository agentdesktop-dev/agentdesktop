# Agent Desktop Controller UI

React and Vite administration interface for controller fleet status, device
inventory, managed configuration, and runtime settings.

## Development

From the repository root:

```sh
cd frontend
pnpm install
pnpm dev:controller
```

API requests under `/api` are proxied to `http://127.0.0.1:8080` during local
development.

## Storybook

The controller views have deterministic Storybook states for development,
interaction tests, responsive checks, and automated accessibility checks. The
browser install is required once per Playwright version:

```sh
cd frontend
pnpm --filter @agentdesktop/controller-web exec playwright install chromium
pnpm storybook:controller
```

The controller Storybook runs at `http://localhost:6007/`. Run both UI suites or
build both static Storybooks with:

```sh
pnpm test:storybook
pnpm build:storybook
```
