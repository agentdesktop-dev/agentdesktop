# Kubernetes controller with Dex

This example runs the Agentdesktop controller, Dex, and a disposable
PostgreSQL database in Kubernetes. The controller is installed with the
repository's Helm chart. It is intended for a local development cluster, not
production.

## Prerequisites

- A current Kubernetes cluster
- `kubectl`, Helm, and OpenSSL
- The `agentdesktop` binary on the workstation
- An Anthropic API key

Run all commands from the repository root.

## Install the development dependencies

Create the namespace, Dex, and PostgreSQL:

```console
kubectl apply -f examples/kubernetes/infrastructure.yaml
kubectl -n agentdesktop rollout status deployment/postgres
kubectl -n agentdesktop rollout status deployment/dex
```

## Install Agentgateway

Install the standard Kubernetes Gateway API CRDs if the cluster does not
already provide them:

```console
kubectl apply --server-side \
  --filename https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.6.0/standard-install.yaml
```

Install the Agentgateway CRDs and controller with Helm. The model API is
experimental in Agentgateway 1.4 and must be enabled explicitly:

```console
export AGENTGATEWAY_VERSION=v1.4.1

helm upgrade --install agentgateway-crds \
  oci://cr.agentgateway.dev/agentgateway-crds \
  --version "${AGENTGATEWAY_VERSION}" \
  --namespace agentgateway-system \
  --create-namespace

helm upgrade --install agentgateway \
  oci://cr.agentgateway.dev/agentgateway \
  --version "${AGENTGATEWAY_VERSION}" \
  --namespace agentgateway-system \
  --values examples/kubernetes/agentgateway-values.yaml

kubectl -n agentgateway-system rollout status deployment/agentgateway
```

Store the Anthropic API key in the Gateway namespace without writing it to a
values file:

```console
export ANTHROPIC_API_KEY=sk-ant-...
kubectl -n agentgateway-system create secret generic anthropic \
  --from-literal=Authorization="${ANTHROPIC_API_KEY}" \
  --dry-run=client --output=yaml | kubectl apply -f -
```

Create the Gateway and its wildcard Anthropic `AgentgatewayModel`:

```console
kubectl apply -f examples/kubernetes/agentgateway.yaml
kubectl -n agentgateway-system wait \
  --for=condition=Programmed gateway/agentgateway-proxy \
  --timeout=300s
kubectl -n agentgateway-system get agentgatewaymodel anthropic
```

Generate development certificate authorities and install the controller TLS
Secret:

```console
examples/kubernetes/create-tls.sh
```

The script reuses an existing device CA and gateway JWT key, replaces the
controller certificate, publishes the public CA for Agentgateway, and restarts
an existing controller Deployment so it begins serving the new certificate.
This also repairs certificates created with an older example whose service
name was `agentdesktop-controller`.

Install the controller from the local chart:

```console
helm upgrade --install agentdesktop deploy/helm/agentdesktop-controller \
  --namespace agentdesktop \
  --values examples/kubernetes/values.yaml
kubectl -n agentdesktop rollout status deployment/agentdesktop
```

The values use the published `latest` controller image. To use another image,
pass `--set image.repository=... --set image.tag=...` to Helm.

## Map the services on the workstation

Wait for the controller, Dex, and Agentgateway LoadBalancers to receive
external IP addresses:

```console
kubectl -n agentdesktop get service agentdesktop dex --watch
kubectl -n agentgateway-system get service agentgateway-proxy --watch
```

Both applications use their in-cluster service names. Map those names to the
corresponding LoadBalancer IPs on the workstation:

```text
<CONTROLLER-LOAD-BALANCER-IP> agentdesktop.agentdesktop.svc.cluster.local
<DEX-LOAD-BALANCER-IP> dex.agentdesktop.svc.cluster.local
<AGENTGATEWAY-LOAD-BALANCER-IP> agentgateway-proxy.agentgateway-system.svc.cluster.local
```

On Linux and macOS, add the entries to `/etc/hosts`. On Windows, add them to
`C:\Windows\System32\drivers\etc\hosts`. The controller is exposed on HTTPS
port 443, Dex on port 5556, and Agentgateway on HTTP port 80.

In a real deployment, set up proper DNS addresses.

## Enroll the workstation

Copy the development CA used to verify the controller:

```console
kubectl -n agentdesktop get secret agentdesktop-controller-tls \
  --output jsonpath='{.data.device-ca\.pem}' | openssl base64 -d -A \
  > /tmp/agentdesktop-kubernetes-device-ca.pem
```

Start the daemon with the example configuration:

```console
agentdesktop daemon --user --config examples/kubernetes/agentdesktop.yaml
```

When Dex opens in the browser, sign in with `admin@example.com` / `password`.
After enrollment, the controller distributes a Claude Code company
announcement containing `Managed by Agentdesktop` and configures Claude Code's
Anthropic base URL to use Agentgateway. Agentgateway forwards Claude's selected
model to Anthropic using the API key stored in the Kubernetes Secret.

For each request, Claude Code's Agentdesktop credential helper obtains a
short-lived JWT from the controller. Agentgateway requires the
`agentdesktop-controller` issuer and `agentgateway` audience, and validates the
signature against `https://agentdesktop.agentdesktop.svc.cluster.local/.well-known/jwks.json`
on the controller's existing port 443 Service. A backend policy supplies the
development CA for that TLS connection. Agentgateway access logs include the
authenticated `llm.client` and `user` JWT attributes. The Anthropic API key is
never distributed to the workstation.

## Clean up

```console
helm uninstall agentdesktop --namespace agentdesktop
helm uninstall agentgateway --namespace agentgateway-system
helm uninstall agentgateway-crds --namespace agentgateway-system
kubectl delete namespace agentdesktop
kubectl delete namespace agentgateway-system
rm -f /tmp/agentdesktop-kubernetes-device-ca.pem
```

The PostgreSQL deployment uses `emptyDir`; deleting or restarting its Pod
removes all example data.
