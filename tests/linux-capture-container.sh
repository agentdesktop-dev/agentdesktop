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
    --volume "$PWD:/work:ro" \
    --volume "$PWD/tests/fixtures/hbone-echo-server.py:/hbone-echo-server.py:ro" \
    "$image" sh -eu -c '
      test "$(readlink /proc/self/ns/net)" != "$HOST_NET_NAMESPACE"
      test "$(readlink /proc/self/ns/cgroup)" != "$HOST_CGROUP_NAMESPACE"
      cat > /rules.nft
      apt-get update >/dev/null
      apt-get install -y --no-install-recommends nftables netcat-openbsd iproute2 python3-h2 >/dev/null
      CARGO_TARGET_DIR=/tmp/target cargo build --quiet --manifest-path /work/Cargo.toml --bin agentgateway-edge-connector
      mkdir /sys/fs/cgroup/capture-test
      nft --check --file /rules.nft
      nft --file /rules.nft

      python3 /hbone-echo-server.py &
      hbone_server=$!
      ready=
      for attempt in $(seq 1 1000); do
        if ss -ltn | grep -q ":15008"; then
          ready=yes
          break
        fi
      done
      test "$ready" = yes
      printf private-container-token > /tmp/capture-token
      chmod 0600 /tmp/capture-token
      /tmp/target/debug/agentgateway-edge-connector capture --token-file /tmp/capture-token &
      relay=$!
      ready=
      for attempt in $(seq 1 1000); do
        if ss -ltn | grep -q ":15001"; then
          ready=yes
          break
        fi
      done
      test "$ready" = yes

      printf %s $$ > /sys/fs/cgroup/capture-test/cgroup.procs
  response=$(printf client-tls-bytes | nc -w 2 203.0.113.7 443)
  test "$response" = gateway-tls-bytes

      printf quic-probe | nc -u -w 1 203.0.113.7 443 || true
      nft list chain inet agentgateway_edge redirect_tcp | grep -Eq "counter packets [1-9]"
      nft list chain inet agentgateway_edge deny_quic | grep -Eq "counter packets [1-9]"

      nft delete table inet agentgateway_edge
      ! nft list table inet agentgateway_edge >/dev/null 2>&1
      kill "$relay"
      wait "$relay" || true
      wait "$hbone_server"
      printf "private Linux capture behavior validated\n"
    '