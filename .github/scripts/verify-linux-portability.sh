#!/usr/bin/env bash
# Fails when a Linux binary needs a newer glibc than MAX_GLIBC or links a
# shared library outside the base system set, so it would not start on older
# distributions such as RHEL 8 (glibc 2.28).
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <binary> <max-glibc-version>" >&2
  exit 2
fi
binary="$1"
max_glibc="$2"

allowed_libraries=(
  ld-linux-x86-64.so.2
  ld-linux-aarch64.so.1
  libc.so.6
  libdl.so.2
  libgcc_s.so.1
  libm.so.6
  libpthread.so.0
  librt.so.1
)

status=0

newest_glibc="$(
  objdump -T "${binary}" |
    grep -o 'GLIBC_[0-9][0-9.]*' |
    sed 's/^GLIBC_//' |
    sort -u -V |
    tail -n 1
)"
echo "newest required glibc: ${newest_glibc:-none}"
if [[ -n "${newest_glibc}" ]] &&
  [[ "$(printf '%s\n%s\n' "${newest_glibc}" "${max_glibc}" | sort -V | tail -n 1)" != "${max_glibc}" ]]; then
  echo "error: ${binary} requires glibc ${newest_glibc}, newer than ${max_glibc}" >&2
  status=1
fi

while read -r library; do
  echo "needs: ${library}"
  allowed=false
  for candidate in "${allowed_libraries[@]}"; do
    [[ "${library}" == "${candidate}" ]] && allowed=true
  done
  if [[ "${allowed}" != true ]]; then
    echo "error: ${binary} links ${library}, which is not in the base system set" >&2
    status=1
  fi
done < <(objdump -p "${binary}" | awk '$1 == "NEEDED" { print $2 }')

exit "${status}"
