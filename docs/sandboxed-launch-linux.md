# Sandboxed launch on Linux (preview)

`agentdesktop run claude` works on Linux as it does on Windows (see
**Sandboxed launch** in the README): the daemon starts Claude Code in an OS
container with the workspace, its own state and little else, no credential
inside, and the daemon's relay as its only way to the model and to git.

On Linux the container is the **bubblewrap backend of Microsoft's MXC
executor** (`lxc-exec`). This page covers what a Linux host needs and where
the Linux box differs from the Windows one.

## Host requirements

- An x86-64 or AArch64 Linux host with unprivileged user namespaces.
- `bubblewrap` 0.5.0 or newer, `slirp4netns`, `iptables`, `iproute2` and
  `util-linux` (`unshare`, `nsenter`).
- The MXC executor `lxc-exec` (tested with MXC 1.1.0).
- A user-mode daemon with the executor named in its local configuration:

  ```yaml
  daemon:
    llmProxy:
      listen: 127.0.0.1:18095
    containment:
      executor: /opt/mxc/lxc-exec
  ```

Debian and Ubuntu:

```bash
sudo apt install bubblewrap slirp4netns iptables iproute2 util-linux
```

### Ubuntu 24.04: AppArmor profile

Ubuntu 24.04 sets `kernel.apparmor_restrict_unprivileged_userns=1`, which
stops an unconfined program from creating the user namespace the box needs.
Give the executor a profile that allows it (adjust the path to where
`lxc-exec` is installed):

```
# /etc/apparmor.d/mxc-lxc-exec
abi <abi/4.0>,
include <tunables/global>

profile mxc-lxc-exec /opt/mxc/lxc-exec flags=(unconfined) {
  userns,
  include if exists <local/mxc-lxc-exec>
}
```

```bash
sudo apparmor_parser -r /etc/apparmor.d/mxc-lxc-exec
```

### Checking a host

The daemon asks the executor at startup whether it can confine a box to the
relay on this host (`lxc-exec --available-backends`). When it cannot,
`containment.available` in daemon-info is `false` and `containment.error`
carries the executor's reason, for example a missing `slirp4netns` or refused
user namespaces. `agentdesktop run` then fails with that reason instead of
starting a weaker box.

## How the box reaches the relay

A Linux box has a private network namespace; it does not share host loopback.
The daemon names its relay as the box's proxy (`runtimeConfig.networkProxy`
in the MXC request), and MXC opens exactly that one port, at the gateway
address of the box's network (`10.0.2.2`). Everything else is dropped:

- direct connections to the internet,
- every other port on host loopback,
- inbound connections.

The relay answers under that gateway address for sandbox sessions only. MXC
also points the box's `HTTP_PROXY` and `HTTPS_PROXY` at the relay; the relay
serves requests addressed to itself and refuses to forward anything else
(`CONNECT` and requests for other hosts get `403 egress_not_supported`), so a
tool in the box that tries to reach another site through the proxy fails.

## Differences from the Windows box

| Topic | Windows | Linux |
|---|---|---|
| Programs | Claude Code, OpenCode, Copilot CLI | Claude Code only |
| Network | egress denied by default, host loopback shared; `egress: allow` available | the relay is the only destination; `daemon.containment.egress: allow` is refused |
| Git over HTTPS through the relay | yes | yes |
| Git over ssh (`daemon.containment.git.ssh`) | yes | not yet; a launch with ssh hosts configured is refused |
| Configuration files that do not exist yet (`.mcp.json`, `.claude/settings.json`, `.git/hooks`, ...) | creation is denied | creation is not prevented; the integrity watch reports it when the launch ends |
| Configuration files that exist | read-only | read-only |
| Terminal | same-terminal hosting, or a window | the caller's terminal; without one, output goes to the daemon log |
| Wrap mode, managed shim and packaging | yes | not tested |
| Cleanup of launches left by a previous daemon | at startup | not yet |

Why creation of missing configuration files is not prevented: the bubblewrap
backend masks a denied path by mounting over it, and for a path that does not
exist inside a writable directory it first creates that path on the host. The
daemon therefore denies only paths that exist.

### Terminal behaviour

`agentdesktop run claude --wait` from a terminal runs Claude Code on that
terminal. The box has no controlling terminal, so the terminal's signals go
to `agentdesktop run`, not to the program:

- Inside Claude Code's interactive UI, Ctrl+C is a key press the program
  handles itself.
- Outside it (`claude -p ...`), Ctrl+C ends `agentdesktop run`, and the daemon
  then stops the box. Closing the terminal does the same.
- Resizing the terminal window is not passed on to the program.
