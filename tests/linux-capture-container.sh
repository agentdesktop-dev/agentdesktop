#!/bin/sh
set -eu

image=${LINUX_CAPTURE_TEST_IMAGE:-docker.io/library/rust:1.97-bookworm}
host_net_namespace=$(readlink /proc/self/ns/net)
host_cgroup_namespace=$(readlink /proc/self/ns/cgroup)

cargo run --quiet --bin agentgateway-edge-capture-setup -- \
  render --cgroup /capture-test --redirect-port 15001 |
  podman run --rm --interactive --privileged --cgroupns private \
    --env "HOST_NET_NAMESPACE=$host_net_namespace" \
    --env "HOST_CGROUP_NAMESPACE=$host_cgroup_namespace" \
    "$image" sh -eu -c '
      test "$(readlink /proc/self/ns/net)" != "$HOST_NET_NAMESPACE"
      test "$(readlink /proc/self/ns/cgroup)" != "$HOST_CGROUP_NAMESPACE"
      cat > /rules.nft
      apt-get update >/dev/null
      apt-get install -y --no-install-recommends nftables netcat-openbsd socat iproute2 >/dev/null
      mkdir /sys/fs/cgroup/capture-test
      nft --check --file /rules.nft
      nft --file /rules.nft

      socat -u TCP-LISTEN:15001,reuseaddr OPEN:/tmp/captured,creat &
      listener=$!
      ready=
      for attempt in $(seq 1 1000); do
        if ss -ltn | grep -q ":15001"; then
          ready=yes
          break
        fi
      done
      test "$ready" = yes

      printf %s $$ > /sys/fs/cgroup/capture-test/cgroup.procs
      printf client-tls-bytes | nc -w 2 203.0.113.7 443
      wait "$listener"
      grep -q client-tls-bytes /tmp/captured

      printf quic-probe | nc -u -w 1 203.0.113.7 443 || true
      nft list chain inet agentgateway_edge redirect_tcp | grep -Eq "counter packets [1-9]"
      nft list chain inet agentgateway_edge deny_quic | grep -Eq "counter packets [1-9]"

      nft delete table inet agentgateway_edge
      ! nft list table inet agentgateway_edge >/dev/null 2>&1
      printf "private Linux capture behavior validated\n"
    '