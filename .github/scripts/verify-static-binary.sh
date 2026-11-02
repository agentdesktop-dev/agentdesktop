#!/usr/bin/env bash
# Fails when a Linux binary uses a dynamic loader or links a shared library, so
# it would depend on the host's libc (for example glibc 2.28 on RHEL 8).
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <binary>" >&2
  exit 2
fi
binary="$1"

status=0

if readelf --program-headers "${binary}" | grep -q 'INTERP'; then
  echo "error: ${binary} requests a dynamic loader" >&2
  status=1
fi

while read -r library; do
  echo "error: ${binary} links ${library}" >&2
  status=1
done < <(readelf --dynamic "${binary}" | awk '/(NEEDED)/ { gsub(/[][]/, "", $NF); print $NF }')

if [[ "${status}" -eq 0 ]]; then
  echo "${binary} is statically linked"
fi
exit "${status}"
