#!/usr/bin/env sh
set -eu

repository="Superwheat/renium"
install_root="${XDG_DATA_HOME:-$HOME/.local/share}/renium"
bin_root="$HOME/.local/bin"
stable_bin_root="$HOME/.renium/bin"
script_root="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"

stop_recorded_daemons() {
  failed=0
  for discovery in "$HOME/.renium"/daemon*.json; do
    [ -f "$discovery" ] || continue
    pid="$(sed -n 's/.*"pid"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$discovery" | head -n 1)"
    if [ -n "$pid" ] && kill -0 "$pid" >/dev/null 2>&1; then
      failed=1
    fi
    if [ -z "$pid" ] || ! kill -0 "$pid" >/dev/null 2>&1; then
      rm -f "$discovery" || failed=1
    fi
  done
  [ "$failed" -eq 0 ]
}

renium_port_in_use() {
  port="$1"
  if [ -r /proc/net/tcp ]; then
    hex_port="$(printf '%04X' "$port")"
    if [ -r /proc/net/tcp6 ]; then
      awk -v suffix=":$hex_port" '$2 ~ suffix "$" && $4 == "0A" { found=1 } END { exit !found }' \
        /proc/net/tcp /proc/net/tcp6
    else
      awk -v suffix=":$hex_port" '$2 ~ suffix "$" && $4 == "0A" { found=1 } END { exit !found }' \
        /proc/net/tcp
    fi
    return
  fi
  if command -v lsof >/dev/null 2>&1; then
    lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1
    return
  fi
  printf '%s\n' "Cannot verify that Renium daemon ports are released." >&2
  return 0
}

assert_renium_ports_released() {
  attempts=0
  while [ "$attempts" -lt 20 ]; do
    occupied=""
    for port in 8780 8781 8782; do
      if renium_port_in_use "$port"; then
        occupied="$occupied $port"
      fi
    done
    [ -n "$occupied" ] || return 0
    attempts=$((attempts + 1))
    sleep 0.05
  done
  printf '%s\n' "Renium daemon ports are still in use:$occupied" >&2
  return 1
}

physical_path() {
  target="$1"
  case "/$target/" in
    *"/../"*|*"/./"*)
      printf '%s\n' "Lifecycle paths must not contain . or .. components: $target" >&2
      return 1
      ;;
  esac
  case "$target" in
    /*) ;;
    *) target="$(pwd)/$target" ;;
  esac
  suffix=""
  existing="$target"
  while [ ! -e "$existing" ]; do
    name="$(basename "$existing")"
    [ "$name" != "/" ] || return 1
    suffix="/$name$suffix"
    existing="$(dirname "$existing")"
  done
  if [ -d "$existing" ]; then
    resolved="$(cd "$existing" && pwd -P)"
  else
    parent="$(cd "$(dirname "$existing")" && pwd -P)"
    resolved="$parent/$(basename "$existing")"
  fi
  printf '%s%s\n' "$resolved" "$suffix"
}

process_start_identity() {
  process_pid="$1"
  if [ -r "/proc/$process_pid/stat" ]; then
    sed 's/.*) //' "/proc/$process_pid/stat" 2>/dev/null |
      awk '{print $20; exit}'
    return
  fi
  ps -o lstart= -p "$process_pid" 2>/dev/null |
    awk '{$1=$1; print; exit}'
}

assert_no_active_update_helper() {
  reservation="$lifecycle_root/update-helper-reservation.json"
  [ -f "$reservation" ] || return 0
  helper_pid="$(sed -n 's/.*"helperPid"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$reservation" | head -n 1)"
  helper_start="$(sed -n 's/.*"helperStartIdentity"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$reservation" | head -n 1)"
  parent_pid="$(sed -n 's/.*"parentPid"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$reservation" | head -n 1)"
  parent_start="$(sed -n 's/.*"parentStartIdentity"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$reservation" | head -n 1)"
  if [ -n "$helper_pid" ] && [ -n "$helper_start" ] &&
    kill -0 "$helper_pid" >/dev/null 2>&1 &&
    [ "$(process_start_identity "$helper_pid")" = "$helper_start" ]
  then
    printf '%s\n' "A Renium update helper is still running." >&2
    return 1
  fi
  if [ -n "$parent_pid" ] && [ -n "$parent_start" ] &&
    kill -0 "$parent_pid" >/dev/null 2>&1 &&
    [ "$(process_start_identity "$parent_pid")" = "$parent_start" ]
  then
    printf '%s\n' "A Renium update is waiting for its helper to take ownership." >&2
    return 1
  fi
}

acquire_lifecycle_lock() {
  if [ "$(uname -s)" = "Darwin" ]; then
    lifecycle_root="$HOME/Library/Application Support/Renium"
    lifecycle_lock_root="$lifecycle_root"
    legacy_lifecycle_root="$lifecycle_root"
  else
    lifecycle_root="${XDG_STATE_HOME:-$HOME/.local/state}/renium"
    legacy_lifecycle_root="${XDG_CONFIG_HOME:-$HOME/.config}/renium"
    lifecycle_lock_root="$legacy_lifecycle_root"
  fi
  install_compare="$(physical_path "$install_root")"
  lifecycle_root="$(physical_path "$lifecycle_root")"
  lifecycle_lock_root="$(physical_path "$lifecycle_lock_root")"
  legacy_lifecycle_root="$(physical_path "$legacy_lifecycle_root")"
  lifecycle_compare="${lifecycle_root%/}"
  case "$lifecycle_compare/" in
    "$install_compare/"*) lifecycle_root="${install_compare}.lifecycle" ;;
  esac
  lifecycle_compare="${lifecycle_root%/}"
  case "$install_compare/" in
    "$lifecycle_compare/"*) lifecycle_root="${install_compare}.lifecycle" ;;
  esac
  if [ "$lifecycle_root" = "$install_root" ]; then
    lifecycle_root="${install_compare}.lifecycle"
  fi
  lock_compare="${lifecycle_lock_root%/}"
  case "$lock_compare/" in
    "$install_compare/"*) lifecycle_lock_root="$lifecycle_root" ;;
  esac
  lock_compare="${lifecycle_lock_root%/}"
  case "$install_compare/" in
    "$lock_compare/"*) lifecycle_lock_root="$lifecycle_root" ;;
  esac
  mkdir -p "$lifecycle_root" "$lifecycle_lock_root"
  lifecycle_lock="$lifecycle_lock_root/lifecycle.lock"
  lifecycle_cleanup="$lifecycle_lock_root/lifecycle.lock.cleanup"
  lifecycle_deadline=$(( $(date +%s) + 1 ))
  lifecycle_start="$(process_start_identity "$$")"
  [ -n "$lifecycle_start" ] || {
    printf '%s\n' "Could not read this process's start identity." >&2
    return 1
  }
  lifecycle_token="$(printf '%s\t%s\t%s' "$$" "$lifecycle_start" "$(date +%s)")"
  lifecycle_temporary="$lifecycle_lock_root/.lifecycle.lock.$$.$(date +%s).tmp"
  (umask 077 && printf '%s' "$lifecycle_token" > "$lifecycle_temporary") || return 1
  while :; do
    if [ -e "$lifecycle_cleanup" ]; then
      if [ "$(date +%s)" -ge "$lifecycle_deadline" ]; then
        rm -f "$lifecycle_temporary"
        printf '%s\n' "Another Renium lifecycle lock operation is still finishing." >&2
        return 1
      fi
      sleep 0.05
      continue
    fi
    if ln "$lifecycle_temporary" "$lifecycle_lock" 2>/dev/null; then
      if [ -f "$lifecycle_lock" ] &&
        [ "$(cat "$lifecycle_lock" 2>/dev/null || true)" = "$lifecycle_token" ]
      then
        rm -f "$lifecycle_temporary"
        if ! assert_no_active_update_helper; then
          rm -f "$lifecycle_lock"
          return 1
        fi
        export RENIUM_LIFECYCLE_LOCK_TOKEN="$lifecycle_token"
        trap 'release_lifecycle_lock' EXIT
        trap 'exit 130' HUP INT TERM
        return 0
      fi
      rm -f "$lifecycle_lock/$(basename "$lifecycle_temporary")"
    fi
    if [ -d "$lifecycle_lock" ]; then
      lifecycle_owner="$lifecycle_lock/owner"
    else
      lifecycle_owner="$lifecycle_lock"
    fi
    holder="$(cat "$lifecycle_owner" 2>/dev/null || true)"
    holder_pid="$(printf '%s' "$holder" | awk -F '\t' 'NF == 3 {print $1}')"
    holder_start="$(printf '%s' "$holder" | awk -F '\t' 'NF == 3 {print $2}')"
    if [ -z "$holder_pid" ]; then
      holder_pid="${holder%%:*}"
      holder_start=""
    fi
    case "$holder_pid" in
      ''|*[!0-9]*)
        if [ "$(date +%s)" -lt "$lifecycle_deadline" ]; then
          sleep 0.05
          continue
        fi
        rm -f "$lifecycle_temporary"
        printf '%s\n' "The Renium lifecycle lock is incomplete or malformed." >&2
        return 1
        ;;
    esac
    if [ -n "$holder_pid" ] && kill -0 "$holder_pid" >/dev/null 2>&1; then
      current_start="$(process_start_identity "$holder_pid")"
      if [ -z "$holder_start" ] || [ "$current_start" = "$holder_start" ]; then
        rm -f "$lifecycle_temporary"
        printf '%s\n' "Another Renium install, update, or uninstall is running." >&2
        return 1
      fi
    fi
    if ! mkdir "$lifecycle_cleanup" 2>/dev/null; then
      if [ "$(date +%s)" -ge "$lifecycle_deadline" ]; then
        rm -f "$lifecycle_temporary"
        printf '%s\n' "Another Renium lifecycle lock operation is still finishing." >&2
        return 1
      fi
      sleep 0.05
      continue
    fi
    current="$(cat "$lifecycle_owner" 2>/dev/null || true)"
    if [ "$current" != "$holder" ]; then
      rmdir "$lifecycle_cleanup" 2>/dev/null || true
      sleep 0.05
      continue
    fi
    if [ -d "$lifecycle_lock" ]; then
      rm -f "$lifecycle_owner"
      rmdir "$lifecycle_lock"
    else
      rm -f "$lifecycle_lock"
    fi
    rmdir "$lifecycle_cleanup"
  done
}

migrate_legacy_install_transaction() {
  [ "$legacy_lifecycle_root" != "$lifecycle_root" ] || return 0
  legacy_transaction="$legacy_lifecycle_root/install-transaction"
  current_transaction="$lifecycle_root/install-transaction"
  if [ -e "$legacy_transaction" ] && [ -e "$current_transaction" ]; then
    printf '%s\n' "Two Renium install transactions need manual cleanup." >&2
    return 1
  fi
  if [ -e "$legacy_transaction" ]; then
    mv "$legacy_transaction" "$current_transaction"
  fi
}

retire_update_state() {
  for root in "$lifecycle_root" "$install_root"; do
    [ -n "$root" ] || continue
    rm -f "$root/update-transaction.json" "$root/update-result.json" \
      "$root/update-helper-reservation.json"
    rm -rf "$root/update-stages"
  done
}

ensure_daemons_stopped() {
  primary="${1:-}"
  fallback="${2:-}"
  stopped=0
  if [ -n "$primary" ] && [ -x "$primary" ] &&
    "$primary" daemon stop --all >/dev/null 2>&1
  then
    stopped=1
  fi
  if [ "$stopped" -eq 0 ] && [ -n "$fallback" ] && [ -x "$fallback" ] &&
    "$fallback" daemon stop --all >/dev/null 2>&1
  then
    stopped=1
  fi
  stop_recorded_daemons
  assert_renium_ports_released
}

release_lifecycle_lock() {
  if [ -n "${lifecycle_lock:-}" ] &&
    [ -f "$lifecycle_lock" ] &&
    [ "$(cat "$lifecycle_lock" 2>/dev/null || true)" = "${lifecycle_token:-}" ]
  then
    rm -f "$lifecycle_lock"
  fi
  unset RENIUM_LIFECYCLE_LOCK_TOKEN
}

editor_extension_root() {
  case "$1" in
    cursor) printf '%s\n' "$HOME/.cursor/extensions" ;;
    code) printf '%s\n' "$HOME/.vscode/extensions" ;;
    code-insiders) printf '%s\n' "$HOME/.vscode-insiders/extensions" ;;
    windsurf) printf '%s\n' "$HOME/.windsurf/extensions" ;;
    *) return 1 ;;
  esac
}

editor_cli() {
  name="$1"
  if command -v "$name" >/dev/null 2>&1; then
    command -v "$name"
    return
  fi
  [ "${os:-}" = "macos" ] || return 1
  case "$name" in
    cursor) app_name="Cursor" ;;
    code) app_name="Visual Studio Code" ;;
    code-insiders) app_name="Visual Studio Code - Insiders" ;;
    windsurf) app_name="Windsurf" ;;
    *) return 1 ;;
  esac
  for applications in "/Applications" "$HOME/Applications"; do
    candidate="$applications/$app_name.app/Contents/Resources/app/bin/$name"
    if [ -x "$candidate" ]; then
      printf '%s\n' "$candidate"
      return
    fi
  done
  return 1
}

is_renium_extension_dir() {
  [ -d "$1" ] || return 1
  case "${1##*/}" in
    local.renium|local.renium-*) return 0 ;;
    *) return 1 ;;
  esac
}

transaction_save_bin_entry() {
  name="$1"
  path="$bin_root/$name"
  backup="$transaction_root/bin-$name"
  mkdir -p "$backup"
  if [ -L "$path" ]; then
    printf '%s' "link" > "$backup/kind"
    readlink "$path" > "$backup/target"
  elif [ -f "$path" ]; then
    printf '%s' "file" > "$backup/kind"
    cp -p "$path" "$backup/file"
  else
    printf '%s' "missing" > "$backup/kind"
  fi
}

transaction_restore_bin_entry() {
  name="$1"
  path="$bin_root/$name"
  backup="$transaction_root/bin-$name"
  kind="$(cat "$backup/kind")"
  rm -f "$path" || return 1
  if [ "$kind" = "link" ]; then
    ln -s "$(cat "$backup/target")" "$path" || return 1
  elif [ "$kind" = "file" ]; then
    cp -p "$backup/file" "$path" || return 1
  fi
}

start_install_transaction() {
  transaction_root="$lifecycle_root/install-transaction"
  if [ -f "$transaction_root/active" ]; then
    printf '%s\n' "An unfinished Renium install transaction must be recovered first." >&2
    return 1
  fi
  rm -rf "$transaction_root"
  mkdir -p "$transaction_root"
  if [ -d "$install_root" ]; then
    : > "$transaction_root/core-existed"
    cp -R "$install_root" "$transaction_root/core"
  fi
  plugin_path="$HOME/Documents/Roblox/Plugins/Renium.rbxm"
  if [ -f "$plugin_path" ]; then
    : > "$transaction_root/plugin-existed"
    cp -p "$plugin_path" "$transaction_root/plugin.rbxm"
  fi
  if [ "$(uname -s)" = "Darwin" ]; then
    managed_studio="$HOME/Applications/Renium Studio.app"
    if [ -d "$managed_studio" ]; then
      : > "$transaction_root/studio-existed"
      ditto "$managed_studio" "$transaction_root/Renium Studio.app"
    fi
  fi
  transaction_save_bin_entry renium
  transaction_save_bin_entry rbx
  {
    editor_extension_root cursor
    editor_extension_root code
    editor_extension_root code-insiders
    editor_extension_root windsurf
    if [ -n "${RENIUM_EXTENSION_ROOT:-}" ]; then
      printf '%s\n' "$RENIUM_EXTENSION_ROOT"
    fi
  } | awk '!seen[$0]++' > "$transaction_root/extension-roots"
  mkdir -p "$transaction_root/extensions"
  extension_index=0
  while IFS= read -r extension_root; do
    snapshot="$transaction_root/extensions/$extension_index"
    mkdir -p "$snapshot"
    printf '%s' "$extension_root" > "$snapshot/root"
    if [ -d "$extension_root" ]; then
      : > "$snapshot/existed"
      for extension in "$extension_root"/local.renium "$extension_root"/local.renium-*; do
        is_renium_extension_dir "$extension" || continue
        cp -R "$extension" "$snapshot/"
      done
      if [ -f "$extension_root/.obsolete" ]; then
        cp -p "$extension_root/.obsolete" "$snapshot/.obsolete"
      fi
    fi
    extension_index=$((extension_index + 1))
  done < "$transaction_root/extension-roots"
  printf '%s\n' "$$" > "$transaction_root/active.next"
  mv "$transaction_root/active.next" "$transaction_root/active"
  sync "$transaction_root/active" >/dev/null 2>&1 || sync >/dev/null 2>&1 || true
}

restore_install_transaction() {
  transaction_root="$lifecycle_root/install-transaction"
  if [ ! -f "$transaction_root/active" ]; then
    if [ -e "$transaction_root" ]; then
      rm -rf "$transaction_root"
    fi
    return 0
  fi
  for snapshot in "$transaction_root"/extensions/*; do
    [ -d "$snapshot" ] || continue
    extension_root="$(cat "$snapshot/root")"
    mkdir -p "$extension_root" || return 1
    for extension in "$extension_root"/local.renium "$extension_root"/local.renium-*; do
      is_renium_extension_dir "$extension" || continue
      rm -rf "$extension" || return 1
    done
    rm -f "$extension_root/.obsolete" || return 1
    for extension in "$snapshot"/local.renium "$snapshot"/local.renium-*; do
      is_renium_extension_dir "$extension" || continue
      cp -R "$extension" "$extension_root/" || return 1
    done
    if [ -f "$snapshot/.obsolete" ]; then
      cp -p "$snapshot/.obsolete" "$extension_root/.obsolete" || return 1
    fi
    if [ ! -f "$snapshot/existed" ]; then
      rmdir "$extension_root" 2>/dev/null || true
    fi
  done
  rm -rf "$install_root" || return 1
  if [ -f "$transaction_root/core-existed" ]; then
    mkdir -p "$(dirname "$install_root")" || return 1
    cp -R "$transaction_root/core" "$install_root" || return 1
  fi
  plugin_path="$HOME/Documents/Roblox/Plugins/Renium.rbxm"
  if [ -f "$transaction_root/plugin-existed" ]; then
    mkdir -p "$(dirname "$plugin_path")" || return 1
    cp -p "$transaction_root/plugin.rbxm" "$plugin_path" || return 1
  else
    rm -f "$plugin_path" || return 1
  fi
  if [ "$(uname -s)" = "Darwin" ]; then
    managed_studio="$HOME/Applications/Renium Studio.app"
    rm -rf "$managed_studio" || return 1
    if [ -f "$transaction_root/studio-existed" ]; then
      mkdir -p "$(dirname "$managed_studio")" || return 1
      ditto "$transaction_root/Renium Studio.app" "$managed_studio" || return 1
    fi
  fi
  mkdir -p "$bin_root" || return 1
  transaction_restore_bin_entry renium || return 1
  transaction_restore_bin_entry rbx || return 1
  rm -rf "$transaction_root"
}

complete_install_transaction() {
  transaction_root="$lifecycle_root/install-transaction"
  rm -f "$transaction_root/active"
  rm -rf "$transaction_root"
}

remove_owned_bin_entry() {
  name="$1"
  path="$bin_root/$name"
  if [ -L "$path" ] && [ "$(readlink "$path")" = "$install_root/$name" ]; then
    rm -f "$path"
  fi
}

recover_core_install() {
  install_parent="$(dirname "$install_root")"
  mkdir -p "$install_parent"
  recovery=""
  recovery_count=0
  for candidate in \
    "$install_parent"/.renium-previous-* \
    "$install_parent"/.renium-core-previous-*
  do
    [ -x "$candidate/renium" ] || continue
    recovery="$candidate"
    recovery_count=$((recovery_count + 1))
  done
  stage_recovery=""
  stage_count=0
  for candidate in \
    "$install_parent"/.renium-install-* \
    "$install_parent"/.renium-core-next-*
  do
    [ -x "$candidate/renium" ] || continue
    stage_recovery="$candidate"
    stage_count=$((stage_count + 1))
  done
  if [ ! -d "$install_root" ]; then
    if [ "$recovery_count" -gt 1 ]; then
      printf '%s\n' "Multiple interrupted Renium core backups need manual cleanup in $install_parent." >&2
      return 1
    fi
    if [ "$recovery_count" -eq 1 ]; then
      mv "$recovery" "$install_root"
    else
      if [ "$stage_count" -gt 1 ]; then
        printf '%s\n' "Multiple interrupted Renium core stages need manual cleanup in $install_parent." >&2
        return 1
      fi
      if [ "$stage_count" -eq 1 ]; then
        mv "$stage_recovery" "$install_root"
      fi
    fi
  fi
  if [ -d "$install_root" ]; then
    for candidate in \
      "$install_parent"/.renium-previous-* \
      "$install_parent"/.renium-core-previous-* \
      "$install_parent"/.renium-install-* \
      "$install_parent"/.renium-core-next-*
    do
      [ -e "$candidate" ] || continue
      rm -rf "$candidate"
    done
  fi
}

recover_managed_studio() {
  [ "$(uname -s)" = "Darwin" ] || return 0
  parent="$HOME/Applications"
  target="$parent/Renium Studio.app"
  mkdir -p "$parent"
  recovery=""
  recovery_count=0
  for candidate in \
    "$parent"/.Renium\ Studio.previous-*.app \
    "$parent"/.Renium\ Studio.app.update-* \
    "$parent"/.Renium\ Studio.transaction-*/previous.app
  do
    [ -x "$candidate/Contents/MacOS/ReniumStudio" ] || continue
    recovery="$candidate"
    recovery_count=$((recovery_count + 1))
  done
  if [ ! -d "$target" ]; then
    if [ "$recovery_count" -gt 1 ]; then
      printf '%s\n' "Multiple interrupted Renium Studio backups need manual cleanup in $parent." >&2
      return 1
    fi
    if [ "$recovery_count" -eq 1 ]; then
      mv "$recovery" "$target"
    fi
  fi
  if [ -d "$target" ]; then
    for candidate in \
      "$parent"/.Renium\ Studio.previous-*.app \
      "$parent"/.Renium\ Studio.app.update-* \
      "$parent"/.Renium\ Studio.transaction-*
    do
      [ -e "$candidate" ] || continue
      rm -rf "$candidate"
    done
  fi
}

acquire_lifecycle_lock
migrate_legacy_install_transaction
restore_install_transaction
recover_core_install
recover_managed_studio

if [ "${1:-}" = "--uninstall" ]; then
  extension_failure=0
  component_failure=0
  if [ -n "${RENIUM_EXTENSION_ROOT:-}" ] && [ -z "${RENIUM_EDITOR_CLI:-}" ]; then
    printf '%s\n' "RENIUM_EDITOR_CLI is required with RENIUM_EXTENSION_ROOT." >&2
    exit 1
  fi
  ensure_daemons_stopped "$install_root/renium" ""
  retire_update_state
  start_install_transaction
  for editor in cursor code code-insiders windsurf; do
    if command -v "$editor" >/dev/null 2>&1; then
      "$editor" --extensions-dir "$(editor_extension_root "$editor")" \
        --uninstall-extension local.renium || extension_failure=1
    fi
  done
  if [ -n "${RENIUM_EXTENSION_ROOT:-}" ]; then
    custom_is_standard=0
    for editor in cursor code code-insiders windsurf; do
      if command -v "$editor" >/dev/null 2>&1 &&
        [ "$(editor_extension_root "$editor")" = "$RENIUM_EXTENSION_ROOT" ]
      then
        custom_is_standard=1
      fi
    done
    if [ "$custom_is_standard" -eq 0 ]; then
      "$RENIUM_EDITOR_CLI" --extensions-dir "$RENIUM_EXTENSION_ROOT" \
        --uninstall-extension local.renium || extension_failure=1
    fi
  fi
  for extension_root in \
    "$HOME/.cursor/extensions" \
    "$HOME/.vscode/extensions" \
    "$HOME/.vscode-insiders/extensions" \
    "$HOME/.windsurf/extensions" \
    ${RENIUM_EXTENSION_ROOT:+"$RENIUM_EXTENSION_ROOT"}
  do
    for extension in "$extension_root"/local.renium "$extension_root"/local.renium-*; do
      is_renium_extension_dir "$extension" || continue
      rm -rf "$extension" || extension_failure=1
    done
  done
  rm -f "$HOME/Documents/Roblox/Plugins/Renium.rbxm" || component_failure=1
  if [ "$(uname -s)" = "Darwin" ]; then
    rm -rf "$HOME/Applications/Renium Studio.app" || component_failure=1
  fi
  rm -rf "$install_root" || component_failure=1
  remove_owned_bin_entry renium || component_failure=1
  remove_owned_bin_entry rbx || component_failure=1
  if [ -L "$stable_bin_root/rbx" ] && [ "$(readlink "$stable_bin_root/rbx")" = "$install_root/rbx" ]; then
    rm -f "$stable_bin_root/rbx" || component_failure=1
  fi
  if [ -L "$stable_bin_root/renium" ] && [ "$(readlink "$stable_bin_root/renium")" = "$install_root/renium" ]; then
    rm -f "$stable_bin_root/renium" || component_failure=1
  fi
  rmdir "$stable_bin_root" 2>/dev/null || true
  if [ "$extension_failure" -ne 0 ] || [ "$component_failure" -ne 0 ]; then
    if ! restore_install_transaction; then
      printf '%s\n' "Renium uninstall failed and rollback was incomplete." >&2
      exit 1
    fi
    printf '%s\n' "Renium uninstall failed; the previous installation was restored." >&2
    exit 1
  fi
  complete_install_transaction
  retire_update_state
  printf '%s\n' "Renium was uninstalled."
  exit 0
fi

interactive=0
if [ "${1:-}" = "--interactive" ]; then
  interactive=1
  shift
fi
local_cli="$script_root/renium"
version="${1:-}"
if [ -z "$version" ] && [ -f "$local_cli" ]; then
  chmod +x "$local_cli"
  version="$("$local_cli" --version 2>/dev/null | awk '{print $2; exit}')"
fi
if [ -z "$version" ]; then
  version="$(curl -fsSL "https://api.github.com/repos/$repository/releases/latest" | sed -n 's/.*"tag_name":[[:space:]]*"v\([^"]*\)".*/\1/p' | head -n 1)"
fi
if [ -z "$version" ]; then
  printf '%s\n' "Could not determine the latest Renium version." >&2
  exit 1
fi

case "$(uname -s)" in
  Darwin) os="macos"; target_os="darwin" ;;
  Linux) os="linux"; target_os="linux" ;;
  *) printf '%s\n' "Renium supports this installer on macOS and Linux." >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64) arch="x64" ;;
  arm64|aarch64) arch="arm64" ;;
  *) printf '%s\n' "Renium does not provide a build for $(uname -m)." >&2; exit 1 ;;
esac
studio_arch="$arch"
if [ "$os" = "macos" ]; then
  case "$arch" in
    x64) studio_host_arch="x86_64" ;;
    arm64) studio_host_arch="arm64" ;;
  esac
  studio_executable=""
  for candidate in \
    "/Applications/RobloxStudio.app/Contents/MacOS/RobloxStudio" \
    "$HOME/Applications/RobloxStudio.app/Contents/MacOS/RobloxStudio"
  do
    if [ -x "$candidate" ]; then
      studio_executable="$candidate"
      break
    fi
  done
  if [ -n "$studio_executable" ]; then
    studio_archs="$(lipo -archs "$studio_executable")"
    case " $studio_archs " in
      *" $studio_host_arch "*) studio_arch="$arch" ;;
      *" arm64 "*) studio_arch="arm64" ;;
      *" x86_64 "*) studio_arch="x64" ;;
      *) printf '%s\n' "Roblox Studio uses an unsupported architecture." >&2; exit 1 ;;
    esac
  fi
fi

archive_name="renium-$version-$os-$arch.zip"
studio_archive_name="renium-$version-$os-$studio_arch.zip"
base_url="https://github.com/$repository/releases/download/v$version"
use_local_package=0
if [ -f "$local_cli" ]; then
  chmod +x "$local_cli"
  local_version="$("$local_cli" --version 2>/dev/null | awk '{print $2; exit}')"
  if [ "$local_version" = "$version" ]; then
    use_local_package=1
  fi
fi
stage="$(mktemp -d "${TMPDIR:-/tmp}/renium-install.XXXXXX")"
transaction_active=0
committed=0
staged_install=""
release_manifest="$stage/update-manifest.json"
fetch_release_manifest() {
  [ -f "$release_manifest" ] && return
  curl -fsSL "$base_url/update-manifest.json" -o "$release_manifest"
  manifest_version="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$release_manifest" | head -n 1)"
  if [ "$manifest_version" != "$version" ]; then
    printf '%s\n' "The Renium $version update manifest is invalid." >&2
    return 1
  fi
}
manifest_sha256() {
  asset_name="$1"
  fetch_release_manifest || return 1
  expected="$(awk -v suffix="/$asset_name\"" '
    index($0, suffix) { waiting = 1; next }
    waiting && /"sha256"[[:space:]]*:/ {
      line = $0
      sub(/^.*"sha256"[[:space:]]*:[[:space:]]*"/, "", line)
      sub(/".*$/, "", line)
      if (value != "" && value != line) inconsistent = 1
      value = line
      matches += 1
      waiting = 0
    }
    END { if (matches >= 1 && !inconsistent && value != "") print value; else exit 1 }
  ' "$release_manifest")" || {
    printf '%s\n' "$asset_name is missing or inconsistent in the Renium update manifest." >&2
    return 1
  }
  case "$expected" in
    *[!0-9a-fA-F]*|'')
      printf '%s\n' "$asset_name has an invalid digest in the Renium update manifest." >&2
      return 1
      ;;
  esac
  if [ "${#expected}" -ne 64 ]; then
    printf '%s\n' "$asset_name has an invalid digest in the Renium update manifest." >&2
    return 1
  fi
  printf '%s\n' "$expected" | tr '[:upper:]' '[:lower:]'
}
download_release_asset() {
  asset_name="$1"
  destination="$2"
  expected="$(manifest_sha256 "$asset_name")" || return 1
  curl -fsSL "$base_url/$asset_name" -o "$destination"
  if command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$destination" | awk '{print $1}')"
  else
    actual="$(sha256sum "$destination" | awk '{print $1}')"
  fi
  if [ "$actual" != "$expected" ]; then
    printf '%s\n' "$asset_name failed SHA-256 verification." >&2
    return 1
  fi
}
cleanup_install() {
  code=$?
  trap - EXIT HUP INT TERM
  if [ "$transaction_active" -eq 1 ] && [ "$committed" -eq 0 ]; then
    if ! rollback_install; then
      printf '%s\n' "Renium installation failed and rollback was incomplete. Recovery files were kept in $lifecycle_root/install-transaction." >&2
      code=1
    fi
  fi
  if [ -n "$staged_install" ] && [ -d "$staged_install" ]; then
    rm -rf "$staged_install" || printf '%s\n' "Could not remove incomplete install files at $staged_install." >&2
  fi
  rm -rf "$stage" || printf '%s\n' "Could not remove temporary install files at $stage." >&2
  release_lifecycle_lock
  exit "$code"
}
trap cleanup_install EXIT HUP INT TERM

editor_architecture() {
  editor_cli="$1"
  version_output="$("$editor_cli" --version 2>&1)" || {
    printf '%s\n' "Could not inspect the architecture of $editor_cli." >&2
    return 1
  }
  detected=""
  for token in $(printf '%s\n' "$version_output" | tr '[:upper:]' '[:lower:]' | tr -cs '[:alnum:]_' '\n'); do
    case "$token" in
      x64|x86_64|amd64) candidate="x64" ;;
      arm64|aarch64) candidate="arm64" ;;
      *) continue ;;
    esac
    if [ -n "$detected" ] && [ "$detected" != "$candidate" ]; then
      printf '%s\n' "$editor_cli reported more than one architecture." >&2
      return 1
    fi
    detected="$candidate"
  done
  if [ -z "$detected" ]; then
    printf '%s\n' "$editor_cli did not report a supported architecture." >&2
    return 1
  fi
  printf '%s\n' "$detected"
}

editor_installs="$stage/editor-installs"
: > "$editor_installs"
for candidate in cursor code code-insiders windsurf; do
  if candidate_cli="$(editor_cli "$candidate")"; then
    candidate_dir="$(dirname "$candidate_cli")"
    case ":$PATH:" in
      *":$candidate_dir:"*) ;;
      *) PATH="$candidate_dir:$PATH" ;;
    esac
    export PATH
    candidate_arch="$(editor_architecture "$candidate_cli")"
    printf '%s\t%s\t%s\n' \
      "$candidate" \
      "$(editor_extension_root "$candidate")" \
      "$candidate_arch" >> "$editor_installs"
  fi
done
if [ -n "${RENIUM_EXTENSION_ROOT:-}" ]; then
  if [ -z "${RENIUM_EDITOR_CLI:-}" ] || ! command -v "$RENIUM_EDITOR_CLI" >/dev/null 2>&1; then
    printf '%s\n' "RENIUM_EDITOR_CLI is required with RENIUM_EXTENSION_ROOT." >&2
    exit 1
  fi
  if ! awk -F '	' -v root="$RENIUM_EXTENSION_ROOT" '$2 == root { found=1 } END { exit !found }' \
    "$editor_installs"
  then
    custom_arch="$(editor_architecture "$RENIUM_EDITOR_CLI")"
    printf '%s\t%s\t%s\n' \
      "$RENIUM_EDITOR_CLI" \
      "$RENIUM_EXTENSION_ROOT" \
      "$custom_arch" >> "$editor_installs"
  fi
fi
if [ "$interactive" -eq 0 ]; then
  for candidate in cursor code code-insiders windsurf; do
    candidate_root="$(editor_extension_root "$candidate")"
    has_renium=0
    for extension in "$candidate_root"/local.renium "$candidate_root"/local.renium-*; do
      if is_renium_extension_dir "$extension"; then
        has_renium=1
        break
      fi
    done
    if [ "$has_renium" -eq 1 ] &&
      ! awk -F '	' -v root="$candidate_root" '$2 == root { found=1 } END { exit !found }' "$editor_installs"
    then
      printf '%s\n' "Renium is installed in $candidate_root, but its exact editor CLI is unavailable. Set RENIUM_EXTENSION_ROOT to that path and RENIUM_EDITOR_CLI to the matching editor command." >&2
      exit 1
    fi
  done
fi
if [ "$interactive" -eq 1 ]; then
  printf '\n%s\n' "Choose where to install the Renium extension:"
  editor_count="$(awk 'END { print NR }' "$editor_installs")"
  if [ "$editor_count" -eq 0 ]; then
    printf '%s\n' "No supported editors were found. Install Cursor, Visual Studio Code, or Windsurf, then run this installer again."
  else
    awk -F '\t' '
      $1 == "cursor" { name = "Cursor" }
      $1 == "code" { name = "Visual Studio Code" }
      $1 == "code-insiders" { name = "Visual Studio Code Insiders" }
      $1 == "windsurf" { name = "Windsurf" }
      $1 != "cursor" && $1 != "code" && $1 != "code-insiders" && $1 != "windsurf" { name = $1 }
      { print NR ". " name }
    ' "$editor_installs"
  fi
  printf '%s\n' "0. Exit"
  while :; do
    printf '%s' "Choose an option: "
    IFS= read -r choice
    if [ "$choice" = "0" ]; then
      printf '%s\n' "Installation cancelled."
      exit 3
    fi
    case "$choice" in
      ''|*[!0-9]*) ;;
      *)
        if [ "$choice" -ge 1 ] && [ "$choice" -le "$editor_count" ]; then
          selected_editor_installs="$stage/selected-editor-install"
          sed -n "${choice}p" "$editor_installs" > "$selected_editor_installs"
          editor_installs="$selected_editor_installs"
          break
        fi
        ;;
    esac
    printf '%s\n' "Enter a number from 0 to $editor_count."
  done
fi
if [ "$use_local_package" -eq 0 ]; then
  download_release_asset "$archive_name" "$stage/$archive_name"
fi
if [ "$studio_archive_name" != "$archive_name" ]; then
  download_release_asset "$studio_archive_name" "$stage/$studio_archive_name"
fi
for extension_arch in x64 arm64; do
  if ! awk -F '	' -v arch="$extension_arch" '$3 == arch { found=1 } END { exit !found }' \
    "$editor_installs"
  then
    continue
  fi
  vsix_name="renium-$version-$target_os-$extension_arch.vsix"
  if [ -f "$script_root/$vsix_name" ]; then
    cp "$script_root/$vsix_name" "$stage/$vsix_name"
  else
    download_release_asset "$vsix_name" "$stage/$vsix_name"
  fi
done

mkdir -p "$bin_root" "$stable_bin_root"
if [ "$use_local_package" -eq 1 ]; then
  cli="$local_cli"
else
  mkdir -p "$stage/expanded"
  unzip -q "$stage/$archive_name" -d "$stage/expanded"
  cli="$(find "$stage/expanded" -type f -name renium | head -n 1)"
  if [ -z "$cli" ]; then
    printf '%s\n' "$archive_name does not contain renium." >&2
    exit 1
  fi
fi
studio_cli="$cli"
if [ "$studio_archive_name" != "$archive_name" ]; then
  mkdir -p "$stage/studio-expanded"
  unzip -q "$stage/$studio_archive_name" -d "$stage/studio-expanded"
  studio_cli="$(find "$stage/studio-expanded" -type f -name renium | head -n 1)"
  if [ -z "$studio_cli" ]; then
    printf '%s\n' "$studio_archive_name does not contain renium." >&2
    exit 1
  fi
  chmod +x "$studio_cli"
fi
plugin_source=""
if [ "$os" = "macos" ]; then
  if [ -f "$script_root/Renium.rbxm" ]; then
    plugin_source="$script_root/Renium.rbxm"
  else
    plugin_source="$stage/Renium.rbxm"
    download_release_asset "Renium.rbxm" "$plugin_source"
  fi
fi
install_parent="$(dirname "$install_root")"
transaction_id="$$-${stage##*.}"
staged_install="$install_parent/.renium-install-$transaction_id"
previous_install="$install_parent/.renium-previous-$transaction_id"
mkdir -p "$install_parent"
mkdir "$staged_install"
cp "$cli" "$staged_install/renium"
for support_file in rbx renium-agents.md; do
  if [ -f "$(dirname "$cli")/$support_file" ]; then
    cp "$(dirname "$cli")/$support_file" "$staged_install/$support_file"
  fi
done
if [ -d "$(dirname "$cli")/renium-guides" ]; then
  cp -R "$(dirname "$cli")/renium-guides" "$staged_install/renium-guides"
fi
if [ -n "$plugin_source" ]; then
  cp "$plugin_source" "$staged_install/Renium.rbxm"
fi
chmod +x "$staged_install/renium"
if [ -f "$staged_install/rbx" ]; then
  chmod +x "$staged_install/rbx"
fi
ensure_daemons_stopped "$install_root/renium" "$staged_install/renium"
retire_update_state
require_owned_or_missing_bin_entry() {
  name="$1"
  path="$bin_root/$name"
  if [ ! -e "$path" ] && [ ! -L "$path" ]; then
    return 0
  fi
  if [ -L "$path" ] && [ "$(readlink "$path")" = "$install_root/$name" ]; then
    return 0
  fi
  printf '%s\n' "Refusing to replace unowned $path. Move or remove it, then run the installer again." >&2
  return 1
}
require_owned_or_missing_bin_entry renium
if [ -f "$staged_install/rbx" ]; then
  require_owned_or_missing_bin_entry rbx
fi
rollback_install() {
  restore_install_transaction
}
start_install_transaction
transaction_active=1
while IFS="$(printf '\t')" read -r editor editor_root editor_arch; do
  [ -n "$editor" ] || continue
  vsix_name="renium-$version-$target_os-$editor_arch.vsix"
  if ! "$editor" --extensions-dir "$editor_root" \
    --install-extension "$stage/$vsix_name" --force
  then
    printf '%s\n' "The editor extension could not be installed." >&2
    exit 1
  fi
done < "$editor_installs"
if [ -e "$install_root" ]; then
  mv "$install_root" "$previous_install"
fi
if ! mv "$staged_install" "$install_root"; then
  exit 1
fi
if ! ln -sf "$install_root/renium" "$bin_root/renium"; then
  exit 1
fi
if [ -f "$install_root/rbx" ]; then
  if ! ln -sf "$install_root/rbx" "$bin_root/rbx"; then
    exit 1
  fi
else
  remove_owned_bin_entry rbx
fi
ln -sf "$install_root/renium" "$stable_bin_root/renium"
if [ -f "$install_root/rbx" ]; then
  ln -sf "$install_root/rbx" "$stable_bin_root/rbx"
fi
if [ "$os" = "macos" ]; then
  if ! "$studio_cli" setup --file "$install_root/Renium.rbxm"; then
    exit 1
  fi
fi
complete_install_transaction
retire_update_state
committed=1
transaction_active=0
rm -rf "$previous_install" || printf '%s\n' "Renium was installed, but the previous core could not be removed." >&2

printf '%s\n' "Renium $version was installed in $install_root."
case ":$PATH:" in
  *":$bin_root:"*) ;;
  *) printf '%s\n' "Add $bin_root to PATH before using rbx." ;;
esac
