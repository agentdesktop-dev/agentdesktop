# Agent Desktop UI

Tauri 2 desktop application for the macOS menu bar, Windows system tray, and
Linux desktop. Closing its window hides it; the tray menu reopens or quits it.

The application is a client of the installed Agent Desktop daemon. It shows
daemon, enrollment, managed configuration, gateway, and discovered-tool state.
The daemon remains responsible for all privileged and policy-sensitive work.
On macOS, launching an app-only installation starts a per-user daemon through
launchd. The PKG installation uses its privileged system LaunchDaemon instead.

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

### Storybook

The desktop views have deterministic Storybook states for development,
interaction tests, responsive checks, and automated accessibility checks. The
browser install is required once per Playwright version:

```sh
cd frontend
pnpm --filter @agentdesktop/desktop-web exec playwright install chromium
pnpm storybook:desktop
```

Run the browser-based story tests or build the static Storybook with:

```sh
pnpm test:storybook
pnpm build:storybook
```

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

The macOS package installs the application and a privileged LaunchDaemon while
preserving machine configuration across upgrades. See
[MACOS_INSTALLER.md](MACOS_INSTALLER.md) for building, MDM configuration,
signing, installation, and removal.

The Windows command builds a per-machine MSI containing the desktop application,
a headless Windows service, and a default machine configuration under
`%ProgramData%\AgentDesktop`. It must run on Windows with WiX's VBSCRIPT
prerequisite enabled. The release workflow installs and exercises unsigned MSIs
as CI artifacts; production distribution still requires Authenticode signing.
See [WINDOWS_INSTALLER.md](WINDOWS_INSTALLER.md) for prerequisites, build,
installation, CI, and signing instructions.

Production distribution requires the relevant Apple or Windows signing setup.
See [ARCHITECTURE.md](ARCHITECTURE.md) for the native/webview boundary.
