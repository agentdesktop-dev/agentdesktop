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

### Run the daemon and UI from source

To test daemon and desktop changes together without building an installer, use
a disposable user-mode state directory and a separate socket. Build the desktop
assets once, then run the daemon in the first terminal:

```sh
export AGENTDESKTOP_DEV_STATE="${XDG_STATE_HOME:-$HOME/.local/state}/agentdesktop-dev"
export AGENTDESKTOP_SOCKET="$AGENTDESKTOP_DEV_STATE/agentdesktop.sock"

rm -rf "$AGENTDESKTOP_DEV_STATE"
pnpm --dir frontend --filter @agentdesktop/desktop-web build
cargo run -p agentdesktop -- daemon \
	--user \
	--socket "$AGENTDESKTOP_SOCKET" \
	--config "$HOME/.config/agentdesktop/config.yaml" \
	--state-dir "$AGENTDESKTOP_DEV_STATE"
```

The configuration file must exist and contain the controller-managed or
standalone configuration to test. Removing the disposable state directory
forces a fresh enrollment; omit that line to preserve the development identity.

In a second terminal, point the Tauri development client at the same socket:

```sh
AGENTDESKTOP_SOCKET="${XDG_STATE_HOME:-$HOME/.local/state}/agentdesktop-dev/agentdesktop.sock" \
	pnpm --dir frontend dev:desktop
```

The installed system daemon can remain running because the custom socket keeps
the source-built processes isolated from it. Stop and rerun the first command
after daemon-side Rust changes; Tauri reloads frontend changes automatically.

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
