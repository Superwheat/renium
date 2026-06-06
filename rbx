#!/bin/sh
set -u

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

CLI=""
if [ -n "${RENIUM_CLI:-}" ] && [ -x "${RENIUM_CLI}" ]; then
  CLI=$RENIUM_CLI
fi
if [ -z "$CLI" ]; then
  CLI=$(command -v renium 2>/dev/null || true)
fi
for candidate in "$script_dir/renium" "$script_dir/bin/renium" "$script_dir/tools/renium/target/release/renium"; do
  if [ -z "$CLI" ] && [ -x "$candidate" ]; then
    CLI=$candidate
  fi
done
if [ -z "$CLI" ]; then
  echo "Renium CLI not found. Install renium on PATH or set RENIUM_CLI to its full path." >&2
  exit 127
fi

usage_exit() {
  echo "$1" >&2
  exit 2
}

ensure_daemon() {
  if ! pgrep -x renium >/dev/null 2>&1; then
    "$CLI" bd -s >/dev/null 2>&1 &
    sleep 1
  fi
}

run_with_console() {
  client_flag=$1
  code=$2
  wait_seconds=${3:-2}
  ensure_daemon
  since=$("$CLI" co -n 1 2>/dev/null | sed -n 's/.*"nextSeq": *\([0-9]*\).*/\1/p')
  since=${since:-0}
  if [ "$client_flag" = "client" ]; then
    "$CLI" lx -c -e "$code" || exit $?
  else
    "$CLI" lx -e "$code" || exit $?
  fi
  sleep "$wait_seconds"
  "$CLI" co -n 20 -s "$since"
}

cmd=${1:-}
case $cmd in
  l)
    [ -n "${2:-}" ] || usage_exit "Usage: rbx l \"print('hi')\""
    exec "$CLI" lx -e "$2"
    ;;
  lf)
    [ -n "${2:-}" ] || usage_exit "Usage: rbx lf path/to/script.luau"
    exec "$CLI" lx -f "$2"
    ;;
  lc)
    [ -n "${2:-}" ] || usage_exit "Usage: rbx lc \"print('hi')\" [player]"
    if [ -n "${3:-}" ]; then
      exec "$CLI" lx --player "$3" -e "$2"
    fi
    exec "$CLI" lx -c -e "$2"
    ;;
  lcf)
    [ -n "${2:-}" ] || usage_exit "Usage: rbx lcf path/to/script.luau"
    exec "$CLI" lx -c -f "$2"
    ;;
  r)
    [ -n "${2:-}" ] || usage_exit "Usage: rbx r \"print('hi')\""
    run_with_console server "$2"
    ;;
  rc)
    [ -n "${2:-}" ] || usage_exit "Usage: rbx rc \"print('hi')\""
    run_with_console client "$2"
    ;;
  rw)
    [ -n "${2:-}" ] || usage_exit "Usage: rbx rw \"print('hi')\" [consoleWaitSeconds]"
    run_with_console server "$2" "${3:-6}"
    ;;
  rcw)
    [ -n "${2:-}" ] || usage_exit "Usage: rbx rcw \"print('hi')\" [consoleWaitSeconds]"
    run_with_console client "$2" "${3:-6}"
    ;;
  c)
    exec "$CLI" co -n 1
    ;;
  cl)
    exec "$CLI" co -n "${2:-20}"
    ;;
  ps)
    if [ -n "${2:-}" ]; then
      exec "$CLI" play -s --players "$2"
    fi
    exec "$CLI" play -s
    ;;
  px)
    exec "$CLI" play -x
    ;;
  pl)
    "$CLI" play -s || exit $?
    sleep 3
    exec "$CLI" play -x
    ;;
  status)
    exec "$CLI" lx -e "local r=game:GetService('RunService') local s=game:GetService('StudioTestService') return { IsRunning=tostring(r:IsRunning()), IsEdit=tostring(r:IsEdit()), RunState=tostring(r.RunState), EditModeActive=tostring((s :: any).EditModeActive) }"
    ;;
  *)
    exec "$CLI" "$@"
    ;;
esac
