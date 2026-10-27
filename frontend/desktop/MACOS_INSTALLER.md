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
Set `APPLE_SIGNING_IDENTITY` to the name or SHA-1 fingerprint of a code-signing
certificate in your macOS keychain to sign the application itself. Reuse the
same certificate and app identifier across builds so Keychain recognizes
upgrades as the same application. A persistent self-signed certificate works
for this; ad-hoc signing (`-`) does not preserve identity across changed builds.

## Release signing

The release workflow signs both architectures of the standalone macOS binary
and the application inside the PKG with a Developer ID Application certificate,
signs the PKG with a Developer ID Installer certificate, and notarizes both.
The PKG is stapled; standalone binaries cannot be, so Gatekeeper checks their
notarization online.

The macOS jobs run in the protected `production-signing` GitHub environment.
Configure these there:

| Name | Kind | Value |
| --- | --- | --- |
| `APPLE_APPLICATION_CERTIFICATE` | Secret | Base64-encoded `.p12` with the Developer ID Application certificate and private key |
| `APPLE_CERTIFICATE_PASSWORD` | Secret | Password protecting that `.p12` |
| `APPLE_INSTALLER_CERTIFICATE` | Secret | Base64-encoded `.p12` with the Developer ID Installer certificate and private key |
| `APPLE_INSTALLER_CERTIFICATE_PASSWORD` | Secret | Password protecting that `.p12` |
| `APPLE_API_KEY` | Secret | Contents of an App Store Connect team API key (`.p8`) |
| `APPLE_API_KEY_ID` | Variable | ID of that API key |
| `APPLE_API_ISSUER` | Variable | Issuer ID of that API key |

Only the Apple Developer Account Holder can create Developer ID certificates.
`notarytool` needs a team API key; individual keys do not work. Missing values
fail the macOS release jobs.

Keychain entries survive upgrades and certificate renewals because the
application identifier remains `dev.agentdesktop.tray`, including for
standalone binaries, and Developer ID signatures are matched by team. Devices
installed from earlier self-signed or ad-hoc builds cannot read their stored
identity after upgrading; the daemon discards it and enrolls again.

To notarize a locally built package, store the API key once, then submit and
staple the package:

```sh
xcrun notarytool store-credentials agentdesktop-notary \
  --key AuthKey_KEYID.p8 --key-id KEYID --issuer ISSUER_ID
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

## Upgrade behavior

The PKG owns process handling for in-place upgrades; MDM does not need a custom
process-kill script. Before replacing the application, `preinstall` stops the
system LaunchDaemon and any per-user fallback LaunchAgents, then asks a running
menu-bar application to terminate. If it does not exit within 10 seconds, the
installer terminates it so the bundle can be replaced safely.

After installing the new payload, `postinstall` starts the system LaunchDaemon.
If the menu-bar application was open before the upgrade and its user is still
logged in, the installer relaunches the new version in that same user session.
This also handles multiple active sessions during fast user switching. An
application that was closed before the upgrade remains closed.

Configuration under `/etc/agentdesktop` and state under `/var/lib/agentdesktop`
are preserved. A silent MDM upgrade can use the same command as installation:

```sh
sudo installer -pkg "Agent Desktop.pkg" -target /
```

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
