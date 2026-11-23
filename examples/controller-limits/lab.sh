#!/bin/sh
# lab.sh policy NAME     Replace the controller's policy with policies/NAME.yaml.
# lab.sh limits on|off   Replace the device's local configuration with
#                        device/limits-on.yaml or limits-off.yaml. Restart the
#                        device to load it.
set -eu

usage() {
  echo "usage: lab.sh policy NAME | limits on|off" >&2
  exit 2
}

# Write next to the target and rename, so the controller's watcher sees one
# complete file.
replace() {
  [ -f "$1" ] || { echo "no such file: ${1#/lab/}" >&2; exit 1; }
  cp "$1" "$2.tmp"
  mv "$2.tmp" "$2"
}

[ $# -eq 2 ] || usage
case "$1:$2" in
  policy:*/* | policy:.*) usage ;;
  policy:*)
    replace "/lab/policies/$2.yaml" /policy/policy.yaml
    echo "controller policy: policies/$2.yaml"
    ;;
  limits:on | limits:off)
    replace "/lab/device/limits-$2.yaml" /device-config/agentdesktop.yaml
    echo "device limits: $2 (restart the device to load them)"
    ;;
  *) usage ;;
esac
