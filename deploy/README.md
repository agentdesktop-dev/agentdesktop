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
`controller.yaml`, then set `existingConfigSecret` to its name. The chart then
does not render the configuration, so it must set the listen addresses the
Services need: `fleetListener.listen: 0.0.0.0:8443`, with `gatewayJwt`,
`gatewayListener.listen: 0.0.0.0:8444` (with `gatewayListener.tls` matching the
chart's value), and with `metricsListener.enabled`,
`metricsListener.listen: 0.0.0.0:9464`. The controller defaults the first two to
loopback and starts no metrics listener unless it is set.
See `values.yaml` for all chart settings.

The `<release>` Service exposes only the TLS-protected fleet API. With
`gatewayJwt`, the chart adds a `<release>-gateway` ClusterIP Service for LLM
gateways, which serves only the gateway JWKS; point gateways' JWKS URL at it.
Set `gatewayListener.tls: true` unless the network between gateways and the
controller is trusted, and include the Service name in the controller
certificate.

The controller limits connections and enrollment per client address. With the
Kubernetes default external traffic policy, it may see node addresses instead of
devices' own, and many devices then share one address's connection limit and
enrollment budget. Set `service.externalTrafficPolicy` to `Local` to keep
devices' addresses when your load balancer sends traffic only to nodes that run
the controller. With the default policy, or behind a load balancer that proxies
connections, raise `fleetListener.limits.connectionsPerAddress`, and for large
rollouts raise `fleetListener.limits.enrollmentBurst` or shorten
`fleetListener.limits.enrollmentRefill`.

Set `metricsListener.enabled: true` to serve the controller's metrics in the
Prometheus text format from a `<release>-metrics` ClusterIP Service, at
`/metrics` on port 9464. They count each time a listener's limits act and the
connections open on each listener; see the root README for the list. Add
`metricsListener.service.annotations` for a scraper that discovers targets by
annotation. `agentdesktop_limit_hits_total` rising for `connections_per_address`
or `enrollment_rate` on the `fleet` listener is the sign that many devices
share one address, as described above.

To inspect the loopback-only management UI:

```console
kubectl port-forward deployment/agentdesktop 8080:8080
```

## Network policy

Set `networkPolicy.enabled=true` to restrict the controller pod with a
Kubernetes NetworkPolicy. It is off by default, and enforcement requires a CNI
that implements NetworkPolicy.

When it is enabled:

- Ingress to the fleet port (gRPC API, gateway JWKS, usage ingest) is allowed
  from `networkPolicy.ingress.fleet.from`. The default empty list allows any source.
- Ingress to the management port is allowed only from
  `networkPolicy.ingress.admin.from`, by default pods in the release namespace.
  The generated configuration binds it to loopback anyway.
- With `metricsListener.enabled`, ingress to the metrics port is allowed only
  from `networkPolicy.ingress.metrics.from`, by default pods in the release
  namespace. A Prometheus in another namespace needs a `namespaceSelector`
  there.
- Egress is limited to DNS (`networkPolicy.egress.dns`, kube-dns by default),
  the database (`networkPolicy.egress.database`), the OIDC issuer
  (`networkPolicy.egress.idp`), and any rules in `networkPolicy.egress.extra`,
  such as a KMS endpoint.

Each `to` list takes standard NetworkPolicy peers. For example, with
PostgreSQL in the same namespace and an external IdP:

```yaml
networkPolicy:
  enabled: true
  egress:
    database:
      to:
      - podSelector:
          matchLabels:
            app: postgres
    idp:
      to:
      - ipBlock:
          cidr: 203.0.113.0/24
```

NetworkPolicy cannot match hostnames. An IdP outside the cluster therefore
needs either its egress CIDRs as `ipBlock` peers, or an empty `idp.to` plus an
FQDN-aware policy from your CNI, such as Cilium's `toFQDNs`, that allows it.
When `database.to` or `idp.to` is empty, no rule is rendered for that
destination. The controller fails at startup if it cannot reach either one.
