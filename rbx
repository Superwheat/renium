#!/bin/sh
set -u
export RENIUM_AGENT_CLI=1

script_path=$0
while [ -L "$script_path" ]; do
  link=$(readlink "$script_path")
  case "$link" in
    /*) script_path=$link ;;
    *) script_path=$(dirname -- "$script_path")/$link ;;
  esac
done
script_dir=$(CDPATH= cd -- "$(dirname -- "$script_path")" && pwd)
CLI=""
if [ -n "${RENIUM_CLI:-}" ] && [ -f "$RENIUM_CLI" ] && [ -x "$RENIUM_CLI" ]; then
  CLI=$RENIUM_CLI
fi
for candidate in "$script_dir/renium" "$script_dir/bin/renium" "${XDG_DATA_HOME:-$HOME/.local/share}/renium/renium" "$script_dir/tools/renium/target/release/renium"; do
  if [ -z "$CLI" ] && [ -f "$candidate" ] && [ -x "$candidate" ]; then
    CLI=$candidate
  fi
done
if [ -z "$CLI" ]; then
  CLI=$(command -v renium 2>/dev/null || true)
fi
if [ -z "$CLI" ]; then
  echo "Renium CLI not found. Install renium on PATH or set RENIUM_CLI to its full path." >&2
  exit 127
fi

exec "$CLI" "$@"
