# Deploying the controller

The Helm chart in `helm/agentdesktop-controller` runs the controller in
Kubernetes using an external PostgreSQL database.

The deployment requires an OpenID Connect provider and a Kubernetes Secret
containing the controller TLS material:

- `controller.pem`
- `controller-key.pem`
- `device-ca.pem`
- `device-ca-key.pem`

The controller certificate must be valid for the address used by device
daemons. Keep both private key files secret.

## Helm

The chart does not install PostgreSQL. Set `controller.databaseUrl` to the URL
of an existing database and configure OIDC before installing it:

```console
kubectl create secret generic agentdesktop-controller-tls \
  --from-file=controller.pem \
  --from-file=controller-key.pem \
  --from-file=device-ca.pem \
  --from-file=device-ca-key.pem

helm install agentdesktop deploy/helm/agentdesktop-controller \
  --set-string controller.databaseUrl='postgresql://agentdesktop:password@postgres.example.com:5432/agentdesktop?sslmode=require' \
  --set-string controller.oidc.issuer='https://id.example.com' \
  --set-string controller.oidc.clientId='agentdesktop'
```

For production, avoid placing database credentials in Helm values. Create a
Secret containing the complete controller configuration under the key
`controller.yaml`, then set `controller.existingConfigSecret` to its name.
See `values.yaml` for all chart settings.

The Service exposes only the TLS-protected fleet API. To inspect the
loopback-only management UI:

```console
kubectl port-forward deployment/agentdesktop-agentdesktop-controller 8080:8080
```
