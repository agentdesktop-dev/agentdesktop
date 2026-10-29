# Enrollment lab

This scenario runs the whole device enrollment flow in Docker, with two
devices:

- **desktop**: the desktop app and a system daemon in an XFCE session that
  you open in your browser through noVNC. It enrolls with the browser flow,
  with Firefox inside the container.
- **headless**: the headless daemon. It enrolls with the device authorization
  flow, and you approve its code from the desktop device, as you would
  approve a server's code from your laptop.

Dex is the organization's OIDC provider, and the controller pushes a minimal
policy that lets devices request gateway credentials.
[Device identity and enrollment](../../docs/device-identity.md) describes the
flow being exercised.

Nothing is installed or configured on the host. Keys, the controller
database, and device identities live in Docker volumes. The only host
listeners are two loopback ports:

| Port | Service |
| --- | --- |
| `127.0.0.1:16080` | Desktop device (noVNC) |
| `127.0.0.1:18080` | Controller UI |

## Start

From the repository root:

```console
docker compose -f examples/enrollment/compose.yaml up -d --build
```

The first build compiles the controller, the desktop app, and the headless
daemon, and takes a while.

## Enroll the desktop device

1. Open <http://127.0.0.1:16080/vnc.html?autoconnect=1&resize=remote>.
2. In the Agent Desktop window, select **Enroll device**. Firefox opens in the
   remote session.
3. Sign in as `admin@example.com` with password `password`.

The page reports that Agentdesktop is connected, and the app switches to its
status view.

## Enroll the headless device

Print the verification URL and code:

```console
docker compose -f examples/enrollment/compose.yaml exec headless \
  agentdesktop-headless enrollment
```

Dex is only reachable inside the lab, so approve the code from the desktop
device: open the URL in Firefox in the remote session and sign in as
`user@example.com` with password `password`. Within a few seconds
`enrollment` reports `enrolled`.

Your host's clipboard doesn't reach the remote session directly. To get the
URL into its Firefox, do one of the following:

- Open the noVNC sidebar from the tab on the left edge of the page, select
  the clipboard icon, and paste the URL into the text box. The text is then
  on the remote session's clipboard, and Ctrl+V pastes it into Firefox.
- Open the URL in the desktop's Firefox from your host:

  ```console
  url=$(docker compose -f examples/enrollment/compose.yaml exec headless \
    agentdesktop-headless enrollment | sed -n 's/^Sign in at: //p')
  docker compose -f examples/enrollment/compose.yaml exec -u dev \
    -e DISPLAY=:1 -e HOME=/home/dev desktop firefox-esr "$url"
  ```

  The URL includes the code, so Dex goes straight to the sign-in page.

## Explore

Open the controller UI at <http://127.0.0.1:18080> to see both devices. Each
device's page shows its hostname, `desktop-device` or `headless-device`, and
the issuer and subject of the user who enrolled it.

Request a controller-signed gateway credential. Paste the JWT into a decoder
to see the user, device, and client claims:

```console
docker compose -f examples/enrollment/compose.yaml exec headless \
  agentdesktop-headless credential
```

In the desktop session, open a terminal and run `agentdesktop credential` for
the desktop device.

The daemons run as system daemons and write their logs under their state
directory:

```console
docker compose -f examples/enrollment/compose.yaml exec headless \
  sh -c 'tail -f /var/lib/agentdesktop/logs/*.log'
```

Use `desktop` in place of `headless` for the desktop daemon.

### Revoke the user at the identity provider

Dex keeps its sessions and signing keys in memory. Restarting it invalidates
every token it has issued:

```console
docker compose -f examples/enrollment/compose.yaml restart dex
```

The controller rejects the next request or stream revalidation from each
device. Each daemon then removes its local identity and starts enrolling
again. To see it immediately rather than at the next five-minute
revalidation, restart a device:

```console
docker compose -f examples/enrollment/compose.yaml restart headless
```

### Log out

In the desktop app's **Status** view, select **Sign out** and confirm. The
daemon removes its local identity and returns to enrollment. The controller
keeps the device record.

## Reset

Stop the scenario and delete all of its state:

```console
docker compose -f examples/enrollment/compose.yaml down --volumes
```

## Automated test

`e2e.sh` checks the same flows without the desktop app, and CI runs it:

```console
make e2e-enrollment BUILD_PROFILE=debug
```

It enrolls two headless devices, one with the device authorization flow and
one with the browser flow, signing in to Dex with curl. It then checks the
controller's device records and the gateway credential's claims, and that
revoking the user at Dex makes a device enroll again. The flow is set by the
daemon configuration, not the binary, so the desktop app's enrollment runs
the same code as the browser-flow device. The test runs as its own Compose
project with no host ports, so it does not disturb a running lab. Set
`KEEP=1` to leave its services running for debugging.

## How it works

The services share a Docker network, laid out like a real deployment:

- Dex and the controller are the organization's services, at
  `http://dex:5556/dex` and `https://controller:8443`. The lab's keys give the
  controller a certificate for the name `controller`.
- Each device is its own container with its own hostname, like a separate
  machine. Both daemons and Firefox in the desktop session reach Dex by the
  same issuer URL, which the controller requires.
- In the browser flow, Firefox runs on the same device as the daemon, so the
  daemon's loopback OIDC callback on port 51327 stays inside the desktop
  container.

The controller's management UI listens only on loopback. Like
`kubectl port-forward`, the `controller-ui` service joins the controller's
network namespace and forwards the published port 18080 to it. If the
controller container restarts, run
`docker compose -f examples/enrollment/compose.yaml up -d` to reconnect it.

In the desktop device, supervisord runs the daemon as root, Xvnc, noVNC, and
an XFCE session for the user `dev`. `dev` is in the `agentdesktop` group,
which grants the desktop app access to the daemon's local API. The app starts
from XFCE autostart. It runs with WebKitGTK's sandbox disabled, because the
sandbox cannot start in an unprivileged container.

`Dockerfile` builds both device images. For faster rebuilds while changing
the daemon or app, build with
`docker compose -f examples/enrollment/compose.yaml build --build-arg BUILD_PROFILE=debug`.
