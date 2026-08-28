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

Enable the dedicated writable fleet-configuration ConfigMap:

```yaml
fleetConfiguration:
  enabled: true
  create: true

daemonConfig:
  programs:
    claudeCode: {}
```

The chart creates `<release>-fleet-configuration` with `daemon.yaml` and
`revision` data keys. The controller watches that ConfigMap and the management
UI conditionally replaces it using Kubernetes `resourceVersion`. A successful
save increments the fleet revision and immediately publishes the configuration
to connected devices; no controller restart or Helm upgrade is needed.

The ConfigMap is separate from controller bootstrap configuration and works
when `existingConfigSecret` is set. The chart seeds it from `daemonConfig` at
revision 1 only when the ConfigMap does not already exist. Helm never touches
it afterwards: upgrades, rollbacks, and uninstalls leave the UI-managed data
in place.

When migrating a fleet that already received file-based configuration, do not
let the seed move the fleet revision backwards. Create the ConfigMap yourself
with the currently distributed `daemon.yaml` and a `revision` at or above the
last published one, or raise `revision` on the seeded ConfigMap before
devices reconnect.

Offline renderers such as Argo CD cannot perform the existence lookup and
would reapply the seed on every sync. For those deployments, create the
ConfigMap once outside the renderer with `daemon.yaml` and `revision` keys,
then set:

```yaml
fleetConfiguration:
  enabled: true
  create: false
  name: agentdesktop-fleet-configuration
```

This keeps the renderer from owning or overwriting mutable UI state. Do not let
both GitOps and the UI write the same ConfigMap.

The controller ServiceAccount receives access only to the named ConfigMap.
File-backed configuration remains the default when `fleetConfiguration.enabled`
is false, and the UI remains read-only in that mode.

The management API currently listens on loopback and does not provide separate
browser authentication. Keep it behind a trusted administrative transport such
as `kubectl port-forward`. Do not expose the admin port publicly.

The Service exposes only the TLS-protected fleet API. To inspect the
loopback-only management UI:

```console
kubectl port-forward deployment/agentdesktop 8080:8080
```
