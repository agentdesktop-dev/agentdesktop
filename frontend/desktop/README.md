# Agent Desktop UI

Tauri 2 desktop application for the macOS menu bar, Windows system tray, and
Linux desktop. Closing its window hides it; the tray menu reopens or quits it.

The application is a client of the installed Agent Desktop daemon. It shows
daemon, enrollment, managed configuration, gateway, and discovered-tool state.
The daemon remains responsible for all privileged and policy-sensitive work.

## Development

Install Node.js 24, pnpm, Rust, and the Tauri platform prerequisites.
From the repository root, install the shared frontend workspace and start the
desktop application:

```sh
cd frontend
pnpm install
pnpm dev:desktop
```

Set `AGENTDESKTOP_SOCKET` to override the default Unix socket or Windows named
pipe used by the native client.

## Verification and packaging

From the repository root:

```sh
make desktop-check
make desktop
```

Release installers are built on their target operating system:

```sh
cd frontend
pnpm --filter @agentdesktop/desktop-web dist:mac
pnpm --filter @agentdesktop/desktop-web dist:win
```

Production distribution requires the relevant Apple or Windows signing setup.
See [ARCHITECTURE.md](ARCHITECTURE.md) for the native/webview boundary.
