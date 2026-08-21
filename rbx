#!/bin/sh
set -u
export RENIUM_AGENT_CLI=1

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

CLI=""
if [ -n "${RENIUM_CLI:-}" ] && [ -x "${RENIUM_CLI}" ]; then
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

usage_exit() {
  echo "$1" >&2
  exit 2
}

ensure_daemon() {
  if "$CLI" daemon status >/dev/null 2>&1; then
    return
  fi
  "$CLI" bd >/dev/null 2>&1 &
  attempts=0
  while [ "$attempts" -lt 50 ]; do
    if "$CLI" daemon status >/dev/null 2>&1; then
      return
    fi
    attempts=$((attempts + 1))
    sleep 0.1
  done
  if ! "$CLI" daemon status >/dev/null 2>&1; then
    echo "Renium daemon did not become ready." >&2
    exit 1
  fi
}

run_with_console() {
  client_flag=$1
  code=$2
  wait_seconds=${3:-2}
  ensure_daemon
  if [ "$client_flag" = "client" ]; then
    if ! "$CLI" clients 2>/dev/null | grep -Eq '"role"[[:space:]]*:[[:space:]]*"play-client"'; then
      "$CLI" play -s || exit $?
      attempts=0
      while [ "$attempts" -lt 60 ]; do
        if "$CLI" clients 2>/dev/null | grep -Eq '"role"[[:space:]]*:[[:space:]]*"play-client"'; then
          break
        fi
        attempts=$((attempts + 1))
        sleep 0.1
      done
      if [ "$attempts" -ge 60 ]; then
        echo "A Studio play client did not connect." >&2
        exit 1
      fi
    fi
    if ! seed=$("$CLI" co --client -n 1 2>/dev/null); then
      echo "Could not read the Studio console baseline." >&2
      exit 1
    fi
  else
    if ! seed=$("$CLI" co -n 1 2>/dev/null); then
      echo "Could not read the Studio console baseline." >&2
      exit 1
    fi
  fi
  since=$(printf '%s\n' "$seed" | sed -n 's/.*"nextSeq": *\([0-9]*\).*/\1/p')
  epoch=$(printf '%s\n' "$seed" | sed -n 's/.*"epoch": *"\([^"]*\)".*/\1/p')
  if [ -z "$since" ] || [ -z "$epoch" ]; then
    echo "Studio console baseline was malformed." >&2
    exit 1
  fi
  if [ "$client_flag" = "client" ]; then
    "$CLI" lx -c -e "$code" || exit $?
  else
    "$CLI" lx -e "$code" || exit $?
  fi
  sleep "$wait_seconds"
  while :; do
    if [ "$client_flag" = "client" ]; then
      page=$("$CLI" co --client -n 200 -s "$since") || exit $?
    else
      page=$("$CLI" co -n 200 -s "$since") || exit $?
    fi
    page_epoch=$(printf '%s\n' "$page" | sed -n 's/.*"epoch": *"\([^"]*\)".*/\1/p')
    if [ -z "$page_epoch" ]; then
      echo "Studio console page was malformed." >&2
      exit 1
    fi
    if [ "$page_epoch" != "$epoch" ]; then
      echo "Studio console restarted while output was being collected." >&2
      epoch=$page_epoch
      since=0
      continue
    fi
    truncated=$(printf '%s\n' "$page" | sed -n 's/.*"truncated": *\(true\|false\).*/\1/p')
    if [ "$truncated" = "true" ]; then
      echo "Studio console output was truncated before it could be read." >&2
      exit 1
    fi
    printf '%s\n' "$page"
    next=$(printf '%s\n' "$page" | sed -n 's/.*"nextSeq": *\([0-9]*\).*/\1/p')
    has_more=$(printf '%s\n' "$page" | sed -n 's/.*"hasMore": *\(true\|false\).*/\1/p')
    if [ "$has_more" != "true" ]; then
      break
    fi
    if [ -z "$next" ] || [ "$next" -le "$since" ]; then
      echo "Renium console cursor did not advance." >&2
      exit 1
    fi
    since=$next
  done
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
