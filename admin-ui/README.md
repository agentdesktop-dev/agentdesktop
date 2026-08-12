# Agent Desktop Administration

The React administrator console embedded in the Agent Desktop enrollment server. `/admin/` is the only production administration UI; the Vite server on port `1430` is a hot-reload development view of the same APIs and data.

## Capabilities

- Administrator sign-in through OAuth Authorization Code with PKCE.
- Enrollment counts for pending, issuing, approved, and rejected records.
- Authoritative active/revoked device counts and certificate-expiry risk.
- Pending enrollment approval and rejection with inline confirmation.
- Enrollment review with validated IdP username, immutable OAuth subject, and client-reported machine name.
- Device inventory with client-reported device name, owner, current certificate expiry, certificate generations, successful renewals, and persisted revocation state.
- Ranked, server-paged fleet inventory for agent versions, MCP servers, skills, and plugins with endpoint counts, fleet percentages, search, asset filtering, and per-device report drill-down.
- Organization-wide Allow/Deny desired policy for each supported agent.
- Fleet-wide and per-device force-rescan requests; online managed desktops poll every 30 seconds.
- Inventory and policy views that distinguish reported state from unavailable enforcement capabilities.
- Search by device name, subject, enrollment ID, device ID, or public-key fingerprint.
- Same-origin enrollment APIs under the hosting enrollment server.

The browser completes OAuth Authorization Code with PKCE. The access token is scoped to the browser tab through `sessionStorage`, removed on sign-out or expiry, and never written to local storage or a URL. Production deployments must serve the enrollment server and `/admin/` over organization-trusted HTTPS.

## Development

Start the disposable managed infrastructure from the repository root, then install Node.js 20 or newer dependencies and run Vite:

```bash
scripts/managed-walkthrough.sh start
npm --prefix admin-ui install
npm --prefix admin-ui run dev
```

Open `http://127.0.0.1:1430/` and sign in. Vite proxies `/v1` to `http://127.0.0.1:8091` by default, so this view uses the real walkthrough database. Override the proxy with `ADMIN_UI_SERVER_URL` when developing against another same-origin-compatible enrollment server.

The canonical walkthrough UI needs no separate frontend process:

```text
http://127.0.0.1:8091/admin/
```

The walkthrough authorization server accepts that browser callback and supplies an administrator-scoped token. Stop the infrastructure with `scripts/managed-walkthrough.sh stop`.

## Build and test

```bash
npm --prefix admin-ui run build
cd control-plane && go test ./internal/adminui ./internal/api
```

The Vite build is written to `control-plane/internal/adminui/static/` and embedded in the enrollment-server binary. The configured OAuth public client must allow the deployed `/admin/` callback URL.

## Current server limits

The enrollment and device list APIs return at most 100 records. Enrollment tabs mark a count with `+` when that bound is reached; the fleet summary returns authoritative organization-wide counts.

The server exposes latest-only managed macOS reports for five known agent families, configured MCP server names/transports, and skill/plugin names. Inventory aggregation and device search are server-paged and are not limited to the 100 records returned by the legacy device list. It does not expose model/provider inventory, project-scoped discovery, audit-event reads, connector heartbeat/online state, sandbox decisions, or budget data.

The UI persists a simple organization-wide desired policy that marks each supported agent Allow or Deny. Endpoint blocking is not implemented, so the page labels enforcement unavailable.

See [Architecture](ARCHITECTURE.md) for the trust boundary and command surface.
