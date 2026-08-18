# macOS installer

The macOS package installs:

- `agentdesktop.app` under `/Applications`.
- The `dev.agentdesktop.daemon` system LaunchDaemon.
- An upgrade-safe machine configuration at `/etc/agentdesktop/config.yaml`.
- Private daemon state under `/var/lib/agentdesktop`.

The package creates the `agentdesktop` local group and adds existing local users
with UIDs from 500 through 65533, plus the current console user. The daemon uses
that group to restrict access to its local socket.

## Build

Install Node.js 24, pnpm, Rust, and the Tauri macOS prerequisites. From the
repository root, build a package for the current architecture:

```sh
cd frontend
pnpm install --frozen-lockfile
pnpm --filter @agentdesktop/desktop-web dist:mac
```

Pass a Rust target to produce an architecture-specific package:

```sh
pnpm --filter @agentdesktop/desktop-web dist:mac -- \
  --target aarch64-apple-darwin
```

Packages are written below `target/<target>/release/bundle/pkg`, or below
`target/release/bundle/pkg` when no target is supplied.

Set `AGENTDESKTOP_VERSION` to override the package and application version.
Set `APPLE_INSTALLER_SIGNING_IDENTITY` to a Developer ID Installer identity to
sign the package. `APPLE_INSTALLER_KEYCHAIN` optionally selects its keychain.
Application signing continues to use Tauri's standard Apple signing variables.

For production distribution, notarize and staple the resulting package after
signing it:

```sh
xcrun notarytool submit "Agent Desktop.pkg" \
  --keychain-profile agentdesktop-notary --wait
xcrun stapler staple "Agent Desktop.pkg"
```

## Install and verify

Install interactively through Finder or from a privileged deployment process (replace `Agent Desktop.pkg` with a full path to the package):

```sh
sudo installer -pkg "Agent Desktop.pkg" -target /
sudo launchctl print system/dev.agentdesktop.daemon
"/Applications/agentdesktop.app/Contents/MacOS/agentdesktop" status
```

Open `/Applications/agentdesktop.app` to start the menu bar application. The
LaunchDaemon starts automatically at boot and immediately after installation.
If the application is distributed without the PKG, launching it registers a
per-user LaunchAgent and starts the daemon in user mode. Installing the PKG
later removes that fallback before starting the privileged service.

## Configure through MDM

Deploy a root-owned configuration to `/etc/agentdesktop/config.yaml`. The
installer creates an empty configuration only when that path does not already
exist, so MDM may provision it before or after installing the package.

```yaml
controller:
  address: https://agentdesktop.example.com
  heartbeatInterval: 30s
```

When the controller uses a private CA, deploy its PEM certificate separately
and set `controller.caCertificatePath` to that protected machine path. Restart
the daemon after changing its local configuration:

```sh
sudo launchctl kickstart -k system/dev.agentdesktop.daemon
```

Accounts created after package installation must be authorized separately:

```sh
sudo dseditgroup -o edit -a USERNAME -t user agentdesktop
```

Run that command from an MDM login or account-provisioning script. The user may
need to sign out and back in before existing processes inherit new group access.

## Remove

macOS packages do not provide an uninstall command. Stop the service and remove
the installed payload explicitly:

```sh
launchctl bootout "gui/$(id -u)/dev.agentdesktop.daemon.user" 2>/dev/null || true
rm -f "$HOME/Library/LaunchAgents/dev.agentdesktop.daemon.user.plist"
sudo launchctl bootout system/dev.agentdesktop.daemon
sudo rm -f /Library/LaunchDaemons/dev.agentdesktop.daemon.plist
sudo rm -rf "/Applications/agentdesktop.app"
sudo pkgutil --forget dev.agentdesktop.installer
```

These commands intentionally preserve `/etc/agentdesktop` and
`/var/lib/agentdesktop`. Remove those directories separately only when tenant
configuration and device identity should also be deleted.