#!/bin/sh
set -u
export RENIUM_AGENT_CLI=1

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
CLI=""
if [ -n "${RENIUM_CLI:-}" ] && [ -x "$RENIUM_CLI" ]; then
  CLI=$RENIUM_CLI
fi
for candidate in "$script_dir/renium" "$script_dir/bin/renium" "${XDG_DATA_HOME:-$HOME/.local/share}/renium/renium" "$script_dir/tools/renium/target/release/renium"; do
  if [ -z "$CLI" ] && [ -x "$candidate" ]; then
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
