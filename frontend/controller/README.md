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

Requests under `/api` and `/auth` are proxied to `http://127.0.0.1:8080` during
local development.

## Administrator sessions

The UI reads `GET /auth/session` before requesting fleet data. The controller
serves local (loopback) mode, which grants every operation without sign-in.
A controller built with a compiled-in authentication provider can instead supply
OIDC login, cookie-backed sessions, and role-based permissions behind the same
session API; the browser never stores identity-provider tokens. The `Provider`
trait in `crates/controller/src/management/auth.rs` documents that contract.

The controller rejects unsafe requests from other origins using the browser's
`Sec-Fetch-Site` and `Origin` headers, so the UI sends no anti-CSRF token. The
session's `permissions` only control what the UI shows; the backend enforces
them. A deletion that requires recent authentication offers
`/auth/login?reauthenticate=true` and must be requested again after signing in.

Only trusted foreground pointer, keyboard, touch, or wheel interaction triggers
`POST /auth/activity`, at most once per minute. No session polling, passive page
view, or fleet refresh extends the idle lifetime. An API 401 returns the UI to
sign-in. Authentication routes must be served from the same origin as the UI.

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
