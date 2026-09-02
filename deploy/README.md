# Deploying the controller

The Helm chart in `helm/agentdesktop-controller` runs the controller in
Kubernetes using an external PostgreSQL database.

The deployment requires an OpenID Connect provider and a Kubernetes Secret
containing the controller TLS material:

- `controller.pem`
- `controller-key.pem`
- `device-ca.pem`
- `device-ca-key.pem`

When `gatewayJwt` is configured, the Secret must also contain the configured
JWT signing key, such as `gateway-jwt-key.pem`.

The controller certificate must be valid for the address used by device
daemons. Keep both private key files secret.

## Helm

The chart does not install PostgreSQL. Set `databaseUrl` to the URL
of an existing database and configure OIDC before installing it:

```console
kubectl create secret generic agentdesktop-controller-tls \
  --from-file=controller.pem \
  --from-file=controller-key.pem \
  --from-file=device-ca.pem \
  --from-file=device-ca-key.pem

helm install agentdesktop deploy/helm/agentdesktop-controller \
  --set-string databaseUrl='postgresql://agentdesktop:password@postgres.example.com:5432/agentdesktop?sslmode=require' \
  --set-string oidc.issuer='https://id.example.com' \
  --set-string oidc.clientId='agentdesktop'
```

For production, avoid placing database credentials in Helm values. Create a
Secret containing the complete controller configuration under the key
`controller.yaml`, then set `existingConfigSecret` to its name.
See `values.yaml` for all chart settings.

## Update fleet configuration from the UI

Enable database-backed fleet configuration:

```yaml
fleetConfiguration:
  enabled: true

daemonConfig:
  programs:
    claudeCode: {}
```

On first startup, the controller stores `daemonConfig` in its existing SQL
database at `fleetConfiguration.seedRevision` (default `1`). Once initialized,
the database is authoritative and later Helm value changes do not overwrite UI
edits. Each save uses the current revision for optimistic concurrency,
increments it, and publishes the accepted configuration to connected devices.
Other controller replicas poll for the new revision, so devices connected to
them may receive it a few seconds later.

When migrating a fleet from file-backed configuration, preserve monotonic
revisions. Set `fleetConfiguration.seedRevision` to the currently distributed
revision when the seed is unchanged, or to a higher revision when changing its
content. The seed is ignored after the database row has been initialized.

When `existingConfigSecret` supplies the complete controller configuration,
select database storage in that secret instead of setting the Helm option:

```yaml
daemonConfig:
  database: {}
```

Do not also set the chart's `fleetConfiguration.enabled` or `daemonConfig`
values; the chart rejects those ambiguous combinations.

File-backed configuration remains the default when `fleetConfiguration.enabled`
is false, and the UI remains read-only in that mode. Use file-backed mode when
GitOps should remain the authoritative writer.

The management API currently listens on loopback and does not provide separate
browser authentication. Keep it behind a trusted administrative transport such
as `kubectl port-forward`. Do not expose the admin port publicly.

The Service exposes only the TLS-protected fleet API. To inspect the
loopback-only management UI:

```console
kubectl port-forward deployment/agentdesktop 8080:8080
```
