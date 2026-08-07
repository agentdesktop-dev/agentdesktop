#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)

command -v cargo-xwin >/dev/null 2>&1 || {
  printf '%s\n' 'cargo-xwin is required; see CONTRIBUTE.md' >&2
  exit 1
}

cd "$root"
RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-D warnings" \
  cargo xwin check --target x86_64-pc-windows-msvc --all-targets