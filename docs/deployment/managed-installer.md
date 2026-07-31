# Managed installer development

The managed development artifact contains the connector, installation engine, and an organization bootstrap. Managed identity commands are part of the connector. The artifact does not contain Agent Gateway, provider credentials, policy, client secrets, access tokens, or refresh tokens.

Build a generic managed template and the customizer:

```bash
scripts/build-managed-installer.sh
```

For two-file development, distribute the generic template with an organization JSON and install with:

```bash
agentgateway-edge-managed-installer install --yes \
  --organization organization.json
```

Create the preferred organization-specific executable with:

```bash
scripts/build-managed-installer.sh \
  organization.json \
  target/release/acme-agentgateway-edge-installer
```

The customizer validates and normalizes the same strict schema used by two-file installation, then appends a versioned length and SHA-256-protected trailer to the generic executable. Unknown fields are rejected, which prevents secrets or policy from being added as unofficial bootstrap fields. SHA-256 detects accidental corruption; it does not authenticate the publisher.

Customize first, then apply the organization's platform code signature to the final executable. Any customization after signing invalidates the signature and must be rejected by the deployment workflow.

The schema is demonstrated in [the managed organization example](../../examples/managed-organization.json). It contains:

- Format version.
- Organization ID, display name, and HTTPS support URL.
- HTTPS identity issuer, public OAuth client ID, audience, and scopes.
- HTTPS Agent Gateway origin.

The managed installer is suitable for SSH-based development that simulates per-user MDM installation. MDM installation only places files, an inactive user-systemd unit, and the manifest-owned `~/.local/bin/agentgateway-edge` command link. It does not open a browser, request device enrollment, start forwarding, modify Claude settings, or edit shell startup files. The user later runs the printed `agentgateway-edge connect-agents` command. That command signs in through the browser, requests and waits for authority approval, starts the connector, verifies local readiness, and separately asks before changing Claude settings.

This is not a managed security release. The repository's authorization and enrollment authority is a mock, and Agent Gateway does not yet validate DPoP, reject replay, consume approved/revoked device state, construct immutable trusted policy context, or strip connector credentials before provider forwarding. The installer demonstrates distribution and user ownership only until those controls work end to end.