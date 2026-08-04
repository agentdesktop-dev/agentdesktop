#!/bin/sh
set -eu

image=${LINUX_CAPTURE_TEST_IMAGE:-docker.io/library/rust:1.97-bookworm}
host_net_namespace=$(readlink /proc/self/ns/net)
host_cgroup_namespace=$(readlink /proc/self/ns/cgroup)

podman run --rm --interactive --privileged --cgroupns private \
    --env "HOST_NET_NAMESPACE=$host_net_namespace" \
    --env "HOST_CGROUP_NAMESPACE=$host_cgroup_namespace" \
    --volume "$PWD:/work:ro" \
    --volume "$PWD/tests/fixtures/hbone-echo-server.py:/hbone-echo-server.py:ro" \
    "$image" sh -eu -c '
      test "$(readlink /proc/self/ns/net)" != "$HOST_NET_NAMESPACE"
      test "$(readlink /proc/self/ns/cgroup)" != "$HOST_CGROUP_NAMESPACE"
      apt-get update >/dev/null
      apt-get install -y --no-install-recommends nftables netcat-openbsd iproute2 python3-h2 >/dev/null
      CARGO_TARGET_DIR=/tmp/target cargo build --quiet --manifest-path /work/Cargo.toml \
        --bin agentdesktop --bin agentdesktop-capture-setup
      mkdir /sys/fs/cgroup/capture-one /sys/fs/cgroup/capture-two
      /tmp/target/debug/agentdesktop-capture-setup install --cgroup /capture-one
      /tmp/target/debug/agentdesktop-capture-setup install --cgroup /capture-two
      nft list set inet agentdesktop captured_cgroups | grep -q '"capture-one"'
      nft list set inet agentdesktop captured_cgroups | grep -q '"capture-two"'

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
      /tmp/target/debug/agentdesktop capture --token-file /tmp/capture-token &
      relay=$!
      ready=
      for attempt in $(seq 1 1000); do
        if ss -ltn | grep -q ":15001"; then
          ready=yes
          break
        fi
      done
      test "$ready" = yes

      sh -c "while test ! -e /tmp/go-one; do :; done; printf client-tls-bytes | nc -w 2 203.0.113.7 443 > /tmp/response-one" &
      client=$!
      printf %s "$client" > /sys/fs/cgroup/capture-one/cgroup.procs
      touch /tmp/go-one
      wait "$client"
      test "$(cat /tmp/response-one)" = gateway-tls-bytes

      sh -c "while test ! -e /tmp/go-quic; do :; done; printf quic-probe | nc -u -w 1 203.0.113.7 443 || true" &
      client=$!
      printf %s "$client" > /sys/fs/cgroup/capture-one/cgroup.procs
      touch /tmp/go-quic
      wait "$client"
      nft list chain inet agentdesktop redirect_tcp | grep -Eq "counter packets [1-9]"
      nft list chain inet agentdesktop deny_quic | grep -Eq "counter packets [1-9]"

      /tmp/target/debug/agentdesktop-capture-setup remove --cgroup /capture-one
      ! nft list set inet agentdesktop captured_cgroups | grep -q '"capture-one"'
      nft list set inet agentdesktop captured_cgroups | grep -q '"capture-two"'

      sh -c "while test ! -e /tmp/go-two; do :; done; printf client-tls-bytes | nc -w 2 203.0.113.7 443 > /tmp/response-two" &
      client=$!
      printf %s "$client" > /sys/fs/cgroup/capture-two/cgroup.procs
      touch /tmp/go-two
      wait "$client"
      test "$(cat /tmp/response-two)" = gateway-tls-bytes

      rmdir /sys/fs/cgroup/capture-two
      /tmp/target/debug/agentdesktop-capture-setup remove --cgroup /capture-two
      nft list set inet agentdesktop captured_cgroups | grep -qv "elements ="
      registration_count=$(python3 -c "import json; print(len(json.load(open(\"/run/agentdesktop/capture-state.json\"))[\"registrations\"]))")
      test "$registration_count" = 0
      nft list table inet agentdesktop >/dev/null
      nft delete table inet agentdesktop
      kill "$relay"
      wait "$relay" || true
      wait "$hbone_server"
      printf "private Linux capture behavior validated\n"
    '