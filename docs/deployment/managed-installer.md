# Managed installer development

This guide describes the current Linux/systemd development artifact. It is not a Windows service package or a macOS installer.

The managed development artifact contains the connector, installation engine, and an organization bootstrap. Managed identity commands are part of the connector. The artifact does not contain Agent Gateway, provider credentials, policy, client secrets, access tokens, or refresh tokens.

Build a generic managed template and the customizer:

```bash
scripts/build-managed-installer.sh
```

For two-file development, distribute the generic template with an organization JSON and install with:

```bash
agentdesktop-managed-installer install --yes \
  --organization organization.json
```

Create the preferred organization-specific executable with:

```bash
scripts/build-managed-installer.sh \
  organization.json \
  target/release/acme-agentdesktop-installer
```

The customizer validates and normalizes the same strict schema used by two-file installation, then appends a versioned length and SHA-256-protected trailer to the generic executable. Unknown fields are rejected, which prevents secrets or policy from being added as unofficial bootstrap fields. SHA-256 detects accidental corruption; it does not authenticate the publisher.

Customize first, then apply the organization's platform code signature to the final executable. Any customization after signing invalidates the signature and must be rejected by the deployment workflow.

The schema is demonstrated in [the managed organization example](../../examples/managed-organization.json). It contains:

- Format version.
- Organization ID, display name, and HTTPS support URL.
- HTTPS identity issuer, public OAuth client ID, audience, and scopes.
- HTTPS Agent Gateway origin.

The managed installer is suitable for SSH-based development that simulates the per-user portion of MDM installation. It places files, an inactive user-systemd session-agent unit, an inactive machine-forwarder unit template, and the manifest-owned `~/.local/bin/agentdesktop` command link. It does not activate the machine unit, open a browser, request device enrollment, start forwarding, modify Claude settings, or edit shell startup files.

The machine-forwarder template must never be activated from this default user-writable install root. MDM or a system package must copy the connector and machine unit into root-owned locations before enabling it. The machine unit owns the listeners, capture path, and `/run/agentdesktop/sessions.sock`; it has no identity issuer or credential-store arguments. The user unit owns OAuth, enrollment, renewal, and signing, and registers through that socket.

The Fedora managed walkthrough performs the complete device installation over SSH into `/opt/agentdesktop`, exposes `/usr/local/bin/agentdesktop`, installs organization trust, and starts the machine forwarder. This simulates MDM with the VM's passwordless development account. The user then begins with `agentdesktop connect-agents`; enrollment credentials and the user service remain owned by that user.

After MDM has activated the root-owned machine forwarder, the user runs the printed `agentdesktop connect-agents` command. That command signs in through the browser, requests and waits for authority approval, starts the user session agent, verifies machine-service readiness, and separately asks before changing Claude settings.

This is not yet a managed security release. The Go/PostgreSQL enrollment service, administrator approval, certificate issuance, renewal, recovery, and certificate-authenticated native CONNECT walkthrough are implemented. Managed forwarding carries no OAuth or connector header. Remaining release blockers include publishing and consuming certificate revocation state, proving immutable certificate-derived identity propagation for managed capture, signed distribution, and production platform validation.
