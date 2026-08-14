# Kubernetes controller with Dex

This example runs the Agentdesktop controller, Dex, and a disposable
PostgreSQL database in Kubernetes. The controller is installed with the
repository's Helm chart. It is intended for a local development cluster, not
production.

## Prerequisites

- A current Kubernetes cluster
- `kubectl`, Helm, and OpenSSL
- The `agentdesktop` binary on the workstation

Run all commands from the repository root.

## Install the development dependencies

Create the namespace, Dex, and PostgreSQL:

```console
kubectl apply -f examples/kubernetes/infrastructure.yaml
kubectl -n agentdesktop rollout status deployment/postgres
kubectl -n agentdesktop rollout status deployment/dex
```

Generate development certificate authorities and install the controller TLS
Secret:

```console
examples/kubernetes/create-tls.sh
```

The script reuses an existing device CA, replaces the controller certificate,
and restarts an existing controller Deployment so it begins serving the new
certificate. This also repairs certificates created with an older example
whose service name was `agentdesktop-controller`.

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

Wait for the controller and Dex LoadBalancers to receive external IP
addresses:

```console
kubectl -n agentdesktop get service agentdesktop dex --watch
```

Both applications use their in-cluster service names. Map those names to the
corresponding LoadBalancer IPs on the workstation:

```text
<CONTROLLER-LOAD-BALANCER-IP> agentdesktop.agentdesktop.svc.cluster.local
<DEX-LOAD-BALANCER-IP> dex.agentdesktop.svc.cluster.local
```

On Linux and macOS, add the entries to `/etc/hosts`. On Windows, add them to
`C:\Windows\System32\drivers\etc\hosts`. The controller is exposed on HTTPS
port 443 and Dex is exposed on port 5556.

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
announcement containing `Managed by Agentdesktop` to the workstation.

## Clean up

```console
helm uninstall agentdesktop --namespace agentdesktop
kubectl delete namespace agentdesktop
rm -f /tmp/agentdesktop-kubernetes-device-ca.pem
```

The PostgreSQL deployment uses `emptyDir`; deleting or restarting its Pod
removes all example data.
