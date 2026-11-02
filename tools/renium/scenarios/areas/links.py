"""Link packages and read-only mirrors (matrix/links.md, LINK rows). Rows run offline unless they
say otherwise; every row keeps the global package library and plugin home inside the scratch copy."""

from __future__ import annotations

import functools
import hashlib
import json
import os
import shutil
import stat
import subprocess
import time
from pathlib import Path

from run import LOCAL, FIXTURES, Context, Scenario

RS = "ReplicatedStorage"
RS_STORE = "instances/ReplicatedStorage.renium"
UTIL = "src/ReplicatedStorage/Shared/Util.luau"
LOGGER_MIRROR = "src/ReplicatedStorage/Logger.luau"
SHARED = '["ReplicatedStorage","Shared"]'
COPY = '["ReplicatedStorage","SharedCopy"]'
ICON = '["ReplicatedStorage","TopbarPlus","Icon"]'
COPY_TARGET = '{"service":"ReplicatedStorage","path":["ReplicatedStorage","SharedCopy"]}'


def _env(ctx: Context):
    os.environ["RENIUM_GLOBAL_PACKAGES_DIR"] = str(ctx.root / "global-packages")
    os.environ["RENIUM_PLUGIN_HOME"] = str(ctx.root / "plugin-home")


def _pack_shared(ctx: Context, *extra: str):
    _env(ctx)
    return ctx.rbx("lkp", "--link-folder", "links", "--id", "shared", "--service", RS, "--path", SHARED, *extra, timeout=60)


def _mirror_link(ctx: Context):
    _env(ctx)
    ctx.write("links/Logger.luau", 'return "v1"\n')
    ctx.rbx("lka", "--id", "logger", "--source", "links/Logger.luau", "--service", RS, "--path", '["ReplicatedStorage","Logger"]', timeout=60)
    return ctx.rbx("lk", timeout=60)


def _package_copy(ctx: Context, *extra: str):
    """L2: the shared package (L1) applied at a second path."""
    _pack_shared(ctx, *extra)
    ctx.rbx("lka", "--id", "shared", "--service", RS, "--path", COPY, timeout=60)
    return ctx.rbx("lk", timeout=60)


def _override_bs(ctx: Context, *args: str):
    """A store edit inside a read-only package: with --override-packages once bs has the flag (D-L6),
    plain until then."""
    result = ctx.rbx("bs", *args, "--override-packages", ok=None, timeout=60)
    if not result.ok and "--override-packages" in result.stderr:
        result = ctx.rbx("bs", *args, timeout=60)
    ctx.expect(result.ok, f"bs {' '.join(args)}: {result.error[:300]}")
    return result


def _writable(path: Path):
    os.chmod(path, stat.S_IREAD | stat.S_IWRITE)


def _digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _lock(ctx: Context) -> dict:
    return json.loads(ctx.read(".renium/link.lock.json"))


def _manifest(ctx: Context, root: Path | None = None) -> dict:
    return json.loads(((root or ctx.root) / "renium-link.json").read_text(encoding="utf-8"))


def _stored_attribute(ctx: Context, target: str, name: str):
    found = ctx.rbx("bg", RS, target, name, ok=None, timeout=60)
    return found.json if found.ok else None


def _found(ctx: Context, *args: str) -> list:
    return ctx.rbx("f", *args, "--all", timeout=60).json or []


def _poll(ctx: Context, check, seconds: float, message: str, interval: float = 1.0):
    deadline = time.monotonic() + seconds
    while True:
        value = check()
        if value:
            return value
        if time.monotonic() >= deadline:
            ctx.expect(False, message)
        time.sleep(interval)


# ---------------------------------------------------------------- A. lkp


def pack_a_subtree_into_the_project(ctx: Context):
    """LINK-001/003: lkp writes the package into the link folder, inlines the script source, registers the
    read-only link and the lock entry; a second lkp resaves the package."""
    packed = _pack_shared(ctx)
    ctx.expect(packed.get("id") == "shared" and packed.get("source") == "links/shared.renium", f"lkp: {packed.json}")
    ctx.expect(packed.get("instances") == 2 and packed.get("path") == ["Shared"], f"lkp counts: {packed.json}")
    ctx.expect(all(any(p.endswith("Util.luau") for p in packed.get(key) or []) for key in ("embeddedSourceFiles", "deletedSourceFiles")), f"lkp source files: {packed.json}")
    ctx.expect((ctx.root / "links" / "shared.renium").is_file(), "package file written")
    ctx.expect(not (ctx.root / UTIL).exists(), "Util.luau inlined into the store")
    ctx.expect_in("return { version = 1 }", str(ctx.rbx("bg", RS, "Shared.Util", "Source", timeout=60).json), "Util source kept in the store")
    manifest = _manifest(ctx)
    ctx.expect([(link["id"], link["readOnly"]) for link in manifest["links"]] == [("shared", True)], f"manifest: {manifest}")
    ctx.expect("shared" in json.dumps(_lock(ctx)), "lock entry")
    before = (ctx.root / "links" / "shared.renium").read_bytes()
    lock_before = ctx.read(".renium/link.lock.json")
    _override_bs(ctx, RS, "Shared", "-p", "Mark", "--scope", "attribute", "--str", "v2")
    again = _pack_shared(ctx)
    ctx.expect(again.get("source") == "links/shared.renium", f"second lkp: {again.json}")
    ctx.expect((ctx.root / "links" / "shared.renium").read_bytes() != before, "package bytes changed")
    ctx.expect(ctx.read(".renium/link.lock.json") != lock_before, "lock hash changed")
    ctx.expect(ctx.rbx("lk", "--check", timeout=60).get("differenceCount") == 0, "no drift after a resave")


def global_library_pack(ctx: Context):
    """LINK-002: lkp without --link-folder saves into the global library, never under Documents."""
    _env(ctx)
    documents = Path(os.environ.get("USERPROFILE") or os.environ.get("HOME") or "~").expanduser() / "Documents" / "Renium" / "Packages" / "shared.renium"
    stamp = documents.stat().st_mtime_ns if documents.exists() else None
    packed = ctx.rbx("lkp", "--id", "shared", "--service", RS, "--path", SHARED, timeout=60)
    ctx.expect(packed.get("source") == "~global/shared.renium", f"lkp: {packed.json}")
    ctx.expect((ctx.root / "global-packages" / "shared.renium").is_file(), "package in RENIUM_GLOBAL_PACKAGES_DIR")
    ctx.expect((documents.stat().st_mtime_ns if documents.exists() else None) == stamp, f"{documents} was written")
    ctx.expect(not (ctx.root / "links").exists(), "no project link folder")


def pack_inside_or_around_a_link(ctx: Context):
    """LINK-004: lkp refuses a subtree inside an existing link and one that contains a link, and writes nothing."""
    _env(ctx)
    ctx.write("links/Logger.luau", 'return "v1"\n')
    ctx.rbx("lka", "--id", "inner", "--source", "links/Logger.luau", "--service", RS, "--path", '["ReplicatedStorage","Shared","Logger"]', timeout=60)
    manifest = ctx.read("renium-link.json")
    around = ctx.rbx("lkp", "--link-folder", "links", "--id", "shared", "--service", RS, "--path", SHARED, ok=False, timeout=60)
    ctx.expect_in('Shared contains the existing link "inner"', around.error, "lkp around a link")
    ctx.expect(ctx.read("renium-link.json") == manifest and not (ctx.root / "links" / "shared.renium").exists(), "nothing written")
    ctx.rbx("lkb", "--link", "inner", "--remove", timeout=60)
    _pack_shared(ctx)
    manifest = ctx.read("renium-link.json")
    inside = ctx.rbx("lkp", "--link-folder", "links", "--id", "util", "--service", RS, "--path", '["ReplicatedStorage","Shared","Util"]', ok=False, timeout=60)
    ctx.expect_in('Shared.Util is inside the existing link "shared"', inside.error, "lkp inside a link")
    ctx.expect(ctx.read("renium-link.json") == manifest and not (ctx.root / "links" / "util.renium").exists(), "nothing written")


def pack_refusals(ctx: Context):
    """LINK-005/006: lkp refuses a second id for the same target, an id whose link targets something else,
    an unsafe id, a missing instance and --share together with --link-folder."""
    _pack_shared(ctx)
    other = ctx.rbx("lkp", "--link-folder", "links", "--id", "other", "--service", RS, "--path", SHARED, ok=False, timeout=60)
    ctx.expect_in("is already a renium-link target owned by link shared.", other.error, "second id refused")
    _mirror_link(ctx)
    unrelated = ctx.rbx("lkp", "--link-folder", "links", "--id", "logger", "--service", "Workspace", "--path", '["Workspace","Props"]', ok=False, timeout=60)
    ctx.expect_in("Link logger does not target Workspace.Props; refusing to overwrite", unrelated.error, "unrelated id refused")
    bad_id = ctx.rbx("lkp", "--link-folder", "links", "--id", "a/b", "--service", "Workspace", "--path", '["Workspace","Props"]', ok=False, timeout=60)
    ctx.expect_in("link id must be one filesystem-safe Roblox instance name", bad_id.error, "unsafe id refused")
    missing = ctx.rbx("lkp", "--link-folder", "links", "--id", "nope", "--service", RS, "--path", '["ReplicatedStorage","Nope"]', ok=False, timeout=60)
    ctx.expect_in("Instance not found: ReplicatedStorage.Nope", missing.error, "missing instance refused")
    clash = ctx.rbx("lkp", "--share", "--link-folder", "x", "--service", RS, "--path", SHARED, ok=False, timeout=60)
    ctx.expect(clash.code == 2 and "cannot be used with" in clash.stderr, f"--share with --link-folder: exit {clash.code} {clash.stderr[:200]}")


def packed_source_survives_a_push(ctx: Context):
    """LINK-007: after lkp inlined Util's source, a store push leaves Studio's script text intact."""
    _pack_shared(ctx)
    ctx.open_studio()
    original = ctx.luau("return game.ReplicatedStorage.Shared.Util.Source")
    ctx.rbx("ps", RS_STORE, "--yes", timeout=120)
    ctx.expect(ctx.luau("return game.ReplicatedStorage.Shared.Util.Source") == original, "Util source unchanged by the push")


def missing_link_package_stops_the_comparison(ctx: Context):
    """LINK-073: a comparison whose read-only package file is missing or unreadable stops with the package
    error and leaves both sides alone instead of comparing as if nothing were packed."""
    _pack_shared(ctx)
    ctx.open_studio()
    original = ctx.luau("return game.ReplicatedStorage.Shared.Util.Source")
    package = ctx.root / "links" / "shared.renium"
    intact = package.read_bytes()
    package.unlink()
    started = ctx.rbx("lon", "--wait", "60", ok=None, timeout=200)
    status = ctx.rbx("lst", ok=None, timeout=60)
    text = json.dumps([started.json, started.error, status.json, status.error])
    ctx.expect("shared" in text, f"the missing package is named: {text[:400]}")
    ctx.expect(not (ctx.root / UTIL).exists(), "the packed script's file is not pulled back")
    ctx.rbx("lof", ok=None, timeout=60)
    package.write_bytes(b"not a settings store")
    pushed = ctx.rbx("ps", RS_STORE, "--yes", ok=False, timeout=120)
    ctx.expect_in("shared.renium", pushed.error, "the unreadable package is named")
    ctx.expect(ctx.luau("return game.ReplicatedStorage.Shared.Util.Source") == original, "Util untouched")
    package.write_bytes(intact)


# ---------------------------------------------------------------- B. lka, lk, lks, lkb


def file_mirror_link_lifecycle(ctx: Context):
    """LINK-010/012/013/014: lka registers a file mirror, lk writes it read-only, --check reports drift
    without writing, lks reports the link and its drift."""
    _env(ctx)
    ctx.write("links/Logger.luau", 'return "v1"\n')
    added = ctx.rbx("lka", "--id", "logger", "--source", "links/Logger.luau", "--service", RS, "--path", '["ReplicatedStorage","Logger"]', timeout=60)
    ctx.expect(added.get("linkCount") == 1 and added.get("targetCount") == 1 and added.get("target") == {"service": RS, "path": ["Logger"], "ords": []}, f"lka: {added.json}")
    ctx.expect(_manifest(ctx)["links"][0]["readOnly"] is True, "the link is read-only")
    ctx.expect(not (ctx.root / LOGGER_MIRROR).exists(), "lka writes nothing under src")
    applied = ctx.rbx("lk", timeout=60)
    ctx.expect(applied.get("processedTargets") == 1 and any(p.endswith("Logger.luau") for p in applied.get("changedPaths") or []), f"lk: {applied.json}")
    mirror = ctx.root / LOGGER_MIRROR
    ctx.expect(mirror.read_text(encoding="utf-8") == 'return "v1"\n', "mirror content")
    ctx.expect(not os.access(mirror, os.W_OK), "mirror is read-only")
    ctx.write("links/Logger.luau", 'return "v2"\n')
    mtime = mirror.stat().st_mtime
    lock = ctx.root / ".renium" / "link.lock.json"
    lock_bytes, lock_mtime = lock.read_bytes(), lock.stat().st_mtime
    check = ctx.rbx("lk", "--check", timeout=60)
    ctx.expect(check.get("check") is True and (check.get("differenceCount") or 0) >= 1, f"lk --check: {check.json}")
    ctx.expect(mirror.stat().st_mtime == mtime and mirror.read_text(encoding="utf-8") == 'return "v1"\n', "--check wrote nothing")
    ctx.expect(lock.read_bytes() == lock_bytes and lock.stat().st_mtime == lock_mtime, "--check left the lock alone")
    status = ctx.rbx("lks", timeout=60)
    ctx.expect(status.get("linkCount") == 1 and (status.get("driftedTargets") or 0) >= 1, f"lks: {status.json}")


def link_add_refusals(ctx: Context):
    """LINK-011: lka refuses an unknown source type, a new id without --source, a target owned by another
    link, an empty path segment and ords that do not match the path."""
    _mirror_link(ctx)
    manifest = ctx.read("renium-link.json")
    svn = ctx.rbx("lka", "--id", "svn", "--source-type", "svn", "--source", "x", "--service", RS, "--path", '["ReplicatedStorage","Svn"]', ok=False, timeout=60)
    ctx.expect_in("Unknown --source-type svn", svn.error, "source type")
    sourceless = ctx.rbx("lka", "--id", "x", "--service", RS, "--path", '["ReplicatedStorage","X"]', ok=False, timeout=60)
    ctx.expect_in("--source is required when creating a new link", sourceless.error, "missing source")
    owned = ctx.rbx("lka", "--id", "other", "--source", "links/Logger.luau", "--service", RS, "--path", '["ReplicatedStorage","Logger"]', ok=False, timeout=60)
    ctx.expect_in("owned by link logger.", owned.error, "owned target")
    empty = ctx.rbx("lka", "--id", "logger", "--source", "links/Logger.luau", "--service", RS, "--path", '["ReplicatedStorage",""]', ok=False, timeout=60)
    ctx.expect_in("link target path segment cannot be empty", empty.error, "empty segment")
    ords = ctx.rbx("lka", "--id", "logger", "--source", "links/Logger.luau", "--service", RS, "--path", '["ReplicatedStorage","Logger"]', "--ords", "[1,1,1]", ok=False, timeout=60)
    ctx.expect_in("link target ords must match the target path length", ords.error, "ords length")
    ctx.expect(ctx.read("renium-link.json") == manifest, "refusals leave the manifest alone")


def status_reports_links_and_drift(ctx: Context):
    """LINK-014: lks over a package link and a drifting mirror link."""
    _package_copy(ctx)
    _mirror_link(ctx)
    ctx.write("links/Logger.luau", 'return "v2"\n')
    status = ctx.rbx("lks", timeout=60)
    ctx.expect(status.get("manifestExists") is True and status.get("lockExists") is True and status.get("linkCount") == 2 and (status.get("driftedTargets") or 0) >= 1, f"lks: {status.json}")
    targets = status.get("targets") or []
    logger = [t for t in targets if t.get("linkId") == "logger"]
    ctx.expect(len(logger) == 1 and logger[0].get("drift") is True and logger[0]["mirrors"][0].get("drift") is True, f"logger target: {logger}")
    shared = [t for t in targets if t.get("linkId") == "shared"]
    ctx.expect(len(shared) == 2 and all(t.get("isPackage") is True and t.get("rootClass") == "Folder" and t.get("drift") is False for t in shared), f"shared targets: {shared}")


def mirror_push_fans_out_and_edits_revert(ctx: Context):
    """LINK-015/016: pushing the canonical file updates the mirror and Studio; pushing an edited mirror
    reverts it to the canonical text and Studio never sees the edit."""
    _mirror_link(ctx)
    ctx.open_studio()
    ctx.rbx("ps", RS_STORE, "--yes", timeout=120)
    ctx.write("links/Logger.luau", 'return "v2"\n')
    pushed = ctx.rbx("ps", "links/Logger.luau", "--yes", timeout=120)
    ctx.expect(pushed.ok, f"canonical push: {pushed.json}")
    mirror = ctx.root / LOGGER_MIRROR
    ctx.expect(mirror.read_text(encoding="utf-8") == 'return "v2"\n' and not os.access(mirror, os.W_OK), "mirror follows the canonical file")
    ctx.expect(ctx.luau("return game.ReplicatedStorage.Logger.Source") == 'return "v2"\n'.strip() or 'return "v2"' in ctx.luau("return game.ReplicatedStorage.Logger.Source"), "Studio has v2")
    _writable(mirror)
    mirror.write_text('return "hack"\n', encoding="utf-8")
    ctx.rbx("ps", LOGGER_MIRROR, "--yes", ok=None, timeout=120)
    ctx.expect(mirror.read_text(encoding="utf-8") == 'return "v2"\n', "mirror reverted")
    ctx.expect("hack" not in ctx.luau("return game.ReplicatedStorage.Logger.Source"), "Studio never saw the edit")


def package_at_a_second_path(ctx: Context):
    """LINK-017/019: a package id can target a second path; an edit inside the read-only copy shows as drift
    and lk restores the target (the edit itself is D-L6, see LINK-018)."""
    _pack_shared(ctx)
    added = ctx.rbx("lka", "--id", "shared", "--service", RS, "--path", COPY, timeout=60)
    ctx.expect(added.get("targetCount") == 2, f"lka second path: {added.json}")
    applied = ctx.rbx("lk", timeout=60)
    ctx.expect(applied.get("changedSettingsIds"), f"lk: {applied.json}")
    ctx.expect(any(entry.get("path") == "ReplicatedStorage.SharedCopy.Util" for entry in _found(ctx, RS, "-n", "Util")), "SharedCopy/Util in the store")
    shared = [t for t in ctx.rbx("lks", timeout=60).get("targets") or [] if t.get("linkId") == "shared"]
    ctx.expect(len(shared) == 2 and all(t.get("drift") is False for t in shared), f"lks after lk: {shared}")
    removed = ctx.rbx("rm", RS, "-i", "SharedCopy.Util", ok=False, timeout=60)
    ctx.expect_in("read-only link", removed.error, "rm refusal")
    renamed = ctx.rbx("rn", RS, "Other", "-i", "SharedCopy", ok=False, timeout=60)
    ctx.expect_in("read-only link", renamed.error, "rn refusal")
    _override_bs(ctx, RS, "SharedCopy", "-p", "Mark", "--scope", "attribute", "--str", "x")
    ctx.expect((ctx.rbx("lk", "--check", timeout=60).get("differenceCount") or 0) >= 1, "a store edit inside the package shows as drift")
    ctx.rbx("lk", timeout=60)
    ctx.expect(_stored_attribute(ctx, "SharedCopy", "Mark") is None, "lk removed the attribute")
    ctx.expect(ctx.rbx("lk", "--check", timeout=60).get("differenceCount") == 0, "lk restored the target")


def read_only_package_edits_refused(ctx: Context):
    """LINK-018 (D-L6): every edit inside a read-only package copy is refused without --override-packages
    and leaves the store alone; bs is not (D-L6)."""
    _package_copy(ctx)
    store = ctx.root / RS_STORE
    before = _digest(store)
    removed = ctx.rbx("rm", RS, "-i", "SharedCopy.Util", ok=False, timeout=60)
    ctx.expect_in('Cannot edit read-only link "shared" at ReplicatedStorage.SharedCopy. Use --override-packages', removed.error, "rm refusal")
    unlinked = ctx.rbx("upl", RS, "-i", "SharedCopy", ok=False, timeout=60)
    ctx.expect_in('Cannot edit read-only link "shared" at ReplicatedStorage.SharedCopy. Use --override-packages', unlinked.error, "upl refusal")
    ctx.expect(_digest(store) == before, "rm and upl left the store alone")
    edited = ctx.rbx("bs", RS, "SharedCopy", "-p", "Mark", "--scope", "attribute", "--str", "x", ok=None, timeout=60)
    ctx.expect(not edited.ok and 'Cannot edit read-only link "shared"' in edited.error, f"bs inside a read-only package was not refused (D-L6): {(edited.stdout or edited.error)[:300]}")
    ctx.expect(_digest(store) == before, "bs left the store alone")


def writable_package_keeps_edits(ctx: Context):
    """LINK-020: a writable package link keeps local edits of its copy across lk."""
    _package_copy(ctx, "--writable")
    ctx.rbx("bs", RS, "SharedCopy", "-p", "Mark", "--scope", "attribute", "--str", "w", timeout=60)
    applied = ctx.rbx("lk", timeout=60)
    copy = [link for link in applied.get("links") or [] if link.get("path") == ["SharedCopy"]]
    ctx.expect(copy and copy[0].get("preservedEdits") is True, f"lk on a writable copy: {applied.json}")
    ctx.expect(_stored_attribute(ctx, "SharedCopy", "Mark") == "w", "edit kept")
    status = ctx.rbx("lks", timeout=60)
    ctx.expect([link.get("readOnly") for link in status.get("links") or []] == [False], f"lks: {status.json}")


def force_target_recreates_one_target(ctx: Context):
    """LINK-021: --force-target recreates only the named target."""
    _package_copy(ctx)
    shared = [entry for entry in _found(ctx, RS, "-n", "Shared")]
    ctx.rbx("rm", RS, "-i", "SharedCopy", "--override-packages", timeout=60)
    ctx.expect(not _found(ctx, RS, "-n", "SharedCopy"), "SharedCopy removed")
    forced = ctx.rbx("lk", "--force-target", COPY_TARGET, timeout=60)
    ctx.expect([link.get("skipped") for link in forced.get("links") or [] if link.get("path") == ["Shared"]] == [True], f"Shared touched: {forced.json}")
    ctx.expect([entry.get("c") for entry in _found(ctx, RS, "-n", "SharedCopy")] == ["Folder"], "SharedCopy back")
    ctx.expect(_found(ctx, RS, "-n", "Shared") == shared, "Shared untouched")
    ctx.expect(ctx.rbx("lk", "--check", timeout=60).get("differenceCount") == 0, "no drift after the forced apply")


def unowned_file_under_a_target(ctx: Context):
    """LINK-022 (D-L9): lk refuses to delete a file it does not own under a package target and keeps it.
    It does when a package change re-applies the target; with the package unchanged lk and lk --check
    do not see the unowned script at all (D-L9)."""
    _package_copy(ctx)
    extra = ctx.write("src/ReplicatedStorage/SharedCopy/Extra.luau", "return 1\n")
    checked = ctx.rbx("lk", "--check", ok=None, timeout=60)
    applied = ctx.rbx("lk", ok=None, timeout=60)
    _override_bs(ctx, RS, "Shared", "-p", "Mark", "--scope", "attribute", "--str", "v2")
    _pack_shared(ctx)
    repacked = ctx.rbx("lk", ok=False, timeout=60)
    ctx.expect_in("Package target contains an unowned file that Renium will not delete", repacked.error, "refusal after a package change")
    ctx.expect(extra.read_text(encoding="utf-8") == "return 1\n", "file kept")
    ctx.expect((checked.get("differenceCount") or 0) >= 1, f"lk --check missed the unowned script inside the read-only copy (D-L9): {checked.json}")
    ctx.expect(not applied.ok and "unowned file" in applied.error, f"plain lk ignored the unowned script inside the read-only copy (D-L9): {(applied.stdout or applied.error)[:300]}")


def break_and_remove_targets(ctx: Context):
    """LINK-023/024/025: lkb retains a broken target that lk skips and lka unbreaks; --remove drops it
    and keeps the mirror writable; refusals name what is missing."""
    _mirror_link(ctx)
    broken = ctx.rbx("lkb", "--service", RS, "--path", '["ReplicatedStorage","Logger"]', timeout=60)
    ctx.expect(broken.get("targetsRetained") is True and broken.get("unlockedFiles") == 1, f"lkb: {broken.json}")
    ctx.expect(os.access(ctx.root / LOGGER_MIRROR, os.W_OK), "mirror unlocked")
    ctx.expect([target.get("path") for target in _manifest(ctx).get("broken") or []] == [[RS, "Logger"]], f"manifest broken: {_manifest(ctx)}")
    ctx.write("links/Logger.luau", 'return "v2"\n')
    skipped = ctx.rbx("lk", timeout=60)
    ctx.expect(skipped.get("processedTargets") == 0 and (ctx.root / LOGGER_MIRROR).read_text(encoding="utf-8") == 'return "v1"\n', f"lk skips the broken target: {skipped.json}")
    targets = ctx.rbx("lks", timeout=60).get("targets") or []
    ctx.expect([t.get("broken") for t in targets if t.get("linkId") == "logger"] == [True], f"lks broken: {targets}")
    ctx.rbx("lka", "--id", "logger", "--source", "links/Logger.luau", "--service", RS, "--path", '["ReplicatedStorage","Logger"]', timeout=60)
    ctx.expect(not _manifest(ctx).get("broken"), "lka cleared the break")
    ctx.rbx("lk", timeout=60)
    ctx.expect(not os.access(ctx.root / LOGGER_MIRROR, os.W_OK), "lka unbroke the target")
    removed = ctx.rbx("lkb", "--link", "logger", "--remove", timeout=60)
    ctx.expect(removed.get("manifestRemoved") is True, f"lkb --remove: {removed.json}")
    ctx.expect((ctx.root / LOGGER_MIRROR).exists() and os.access(ctx.root / LOGGER_MIRROR, os.W_OK), "mirror kept and writable")
    nothing = ctx.rbx("lkb", ok=False, timeout=60)
    ctx.expect_in("Specify --link <id>", nothing.error, "lkb needs a selector")
    _mirror_link(ctx)
    missing = ctx.rbx("lkb", "--service", RS, "--path", '["ReplicatedStorage","Nope"]', ok=False, timeout=60)
    ctx.expect_in("Nope is not a renium-link target; nothing to break.", missing.error, "unknown target")
    unknown = ctx.rbx("lkb", "--link", "nope", ok=False, timeout=60)
    ctx.expect_in("No link with id nope", unknown.error, "unknown link id")


def git_source_offline(ctx: Context):
    """LINK-026/027: a git link from a local repository resolves to the commit sha without network; offline
    before the first fetch warns, and --strict turns the warning into a failure."""
    _env(ctx)
    repo = ctx.root / "gitsrc"
    git = ["git", "-c", "core.autocrlf=false", "-c", "user.name=s", "-c", "user.email=s@s"]
    subprocess.run(["git", "init", "-q", "-b", "main", str(repo)], check=True, capture_output=True, timeout=60)
    ctx.write("gitsrc/Logger.luau", 'return "g1"\n')
    subprocess.run(git + ["add", "-A"], cwd=repo, check=True, capture_output=True, timeout=60)
    subprocess.run(git + ["commit", "-q", "-m", "g1"], cwd=repo, check=True, capture_output=True, timeout=60)
    ctx.rbx("lka", "--id", "glog", "--source-type", "git", "--source", str(repo), "--ref", "main", "--subpath", "Logger.luau", "--service", RS, "--path", '["ReplicatedStorage","GitLogger"]', timeout=60)
    offline = ctx.rbx("lk", "--offline", timeout=60)
    ctx.expect_in("is not cached yet", str((offline.get("warnings") or [""])[0]), "offline warning")
    strict = ctx.rbx("lk", "--offline", "--strict", ok=False, timeout=60)
    ctx.expect(strict.code != 0 and "--strict is set" in strict.error, f"--strict: exit {strict.code} {strict.error[:200]}")
    mirror = ctx.root / "src" / "ReplicatedStorage" / "GitLogger.luau"
    for text in ("g1", "g2"):
        if text == "g2":
            ctx.write("gitsrc/Logger.luau", 'return "g2"\n')
            subprocess.run(git + ["commit", "-q", "-am", "g2"], cwd=repo, check=True, capture_output=True, timeout=60)
        sha = subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo, check=True, capture_output=True, text=True, timeout=60).stdout.strip()
        applied = ctx.rbx("lk", timeout=120)
        ctx.expect([link.get("resolvedRef") for link in applied.get("links") or []] == [sha], f"resolved ref {sha}: {applied.json}")
        ctx.expect(mirror.read_text(encoding="utf-8").replace("\r\n", "\n") == f'return "{text}"\n', f"mirror holds {text}")


def wally_link_source(ctx: Context):
    """LINK-028: a wally link resolves from the installed package index offline; without the index lk warns."""
    _wally(ctx, full=True)
    ctx.rbx("lka", "--id", "promise", "--source-type", "wally", "--source", "evaera/promise", "--ref", "^4", "--service", RS, "--path", '["ReplicatedStorage","Promise"]', timeout=60)
    applied = ctx.rbx("lk", "--offline", timeout=60)
    ctx.expect([link.get("resolvedRef") for link in applied.get("links") or []] == ["evaera/promise@4.0.0"], f"lk: {applied.json}")
    shutil.rmtree(ctx.root / "Packages" / "_Index")
    missing = ctx.rbx("lk", "--offline", timeout=60)
    ctx.expect_in("Wally package index not found", str((missing.get("warnings") or [""])[0]), "missing index warning")


def apply_under_live_sync(ctx: Context):
    """LINK-029: under Live Sync an overridden edit of a read-only copy is refused by the worker; lk restores the copy and the path clears."""
    _package_copy(ctx)
    ctx.open_studio()
    ctx.rbx("ps", RS_STORE, "--yes", timeout=180)
    ctx.rbx("lon", "--wait", "60", timeout=200)
    _override_bs(ctx, RS, "SharedCopy", "-p", "Mark", "--scope", "attribute", "--str", "x")
    blocked = ctx.rbx("lst", "--wait", "30", ok=None, timeout=120)
    ctx.expect_in("read-only link", str(blocked.pointer("/daemon/error") or blocked.get("error") or ""), "Live Sync refuses to push the overridden copy")
    applied = ctx.rbx("lk", timeout=60)
    ctx.expect(applied.get("changedPaths"), f"lk: {applied.json}")
    settled = ctx.rbx("lst", "--wait", "60", timeout=150)
    ctx.expect(settled.pointer("/daemon/pendingCount", 0) == 0, f"pending after lk: {settled.json}")
    ctx.expect(ctx.luau('return game.ReplicatedStorage.SharedCopy:GetAttribute("Mark") == nil') is True, "Studio copy restored")


def pull_restores_a_package_studio_changed(ctx: Context):
    """LINK-030: a pull restores a read-only package the Studio side edited and never writes an empty script."""
    _pack_shared(ctx)
    ctx.rbx("lka", "--id", "shared", "--service", RS, "--path", COPY, timeout=60)
    ctx.rbx("lk", timeout=60)
    ctx.open_studio()
    ctx.rbx("ps", RS_STORE, "--yes", timeout=120)
    ctx.luau("game.ReplicatedStorage.SharedCopy.Util.Source = 'return 9' return true")
    pulled = ctx.rbx("pl", timeout=120)
    restored = pulled.get("readOnlyLinksRestored") or []
    ctx.expect(any(entry.get("id") == "shared" or "shared" in json.dumps(entry) for entry in restored), f"pull restore: {pulled.json}")
    stored = ctx.rbx("bg", RS, "SharedCopy.Util", "Source", timeout=60).json
    ctx.expect("return 9" not in str(stored), f"store source restored: {str(stored)[:80]}")
    empties = [p for p in (ctx.root / "src").rglob("*.luau") if p.stat().st_size == 0]
    ctx.expect(not empties, f"empty scripts written: {empties}")


def pull_keeps_writable_links(ctx: Context):
    """LINK-031: a pull leaves a writable package copy at Studio's version."""
    _package_copy(ctx, "--writable")
    ctx.open_studio()
    ctx.rbx("ps", RS_STORE, "--yes", timeout=120)
    ctx.luau("game.ReplicatedStorage.SharedCopy.Util.Source = 'return 9' return true")
    pulled = ctx.rbx("pl", timeout=120)
    ctx.expect(not any("shared" in json.dumps(entry) for entry in pulled.get("readOnlyLinksRestored") or []), f"writable link restored: {pulled.json}")
    ctx.expect_in("return 9", str(ctx.rbx("bg", RS, "SharedCopy.Util", "Source", timeout=60).json), "store has Studio's text")


def invalid_manifest(ctx: Context):
    """LINK-032: lks refuses duplicate link ids and an empty manifest."""
    _env(ctx)
    ctx.write("links/A.luau", "return 1\n")
    link = {"id": "a", "readOnly": True, "source": {"type": "local", "path": "links/A.luau"}}
    targets = [{"service": RS, "path": [RS, name]} for name in ("A", "B")]
    ctx.write("renium-link.json", json.dumps({"version": 1, "links": [dict(link, targets=[target]) for target in targets], "broken": []}))
    duplicate = ctx.rbx("lks", ok=False, timeout=60)
    ctx.expect_in('duplicate renium-link id "a"', duplicate.error, "duplicate ids")
    ctx.write("renium-link.json", "")
    empty = ctx.rbx("lks", ok=False, timeout=60)
    ctx.expect_in("Link manifest is empty", empty.error, "empty manifest")


def target_path_cannot_escape(ctx: Context):
    """LINK-033: a hand-written target path with '..' is rejected (a warning; --strict fails) and nothing is
    written outside src."""
    _env(ctx)
    ctx.write("links/A.luau", "return 1\n")
    ctx.write("renium-link.json", json.dumps({"version": 1, "links": [{"id": "a", "readOnly": True, "source": {"type": "local", "path": "links/A.luau"}, "targets": [{"service": RS, "path": [RS, "..", "..", "x"]}]}], "broken": []}))
    before = sorted(p.name for p in ctx.root.iterdir() if p.name != ".renium")
    applied = ctx.rbx("lk", timeout=60)
    ctx.expect(applied.get("processedTargets") == 0 and "filesystem-safe" in str(applied.get("warnings")), f"lk: {applied.json}")
    strict = ctx.rbx("lk", "--strict", ok=False, timeout=60)
    ctx.expect_in("--strict is set", strict.error, "--strict")
    ctx.expect(sorted(p.name for p in ctx.root.iterdir() if p.name != ".renium") == before and not (ctx.root.parent / "x").exists() and not (ctx.root.parent / "x.luau").exists(), "nothing written outside src")


def link_commands_from_a_subfolder(ctx: Context):
    """LINK-072 (D-L11): link and store commands run from a folder inside the project find the project above it."""
    _mirror_link(ctx)
    inside = ctx.root / "src" / "ReplicatedStorage"
    status = ctx.rbx("lks", cwd=inside, ok=None, timeout=60)
    ctx.expect(status.ok and status.get("linkCount") == 1, f"lks from src/ReplicatedStorage: {status.error[:300]}")
    found = ctx.rbx("f", RS, "-n", "Logger", "--all", cwd=inside, ok=None, timeout=60)
    ctx.expect(found.ok and found.json, f"f from src/ReplicatedStorage: {found.error[:300]}")


# ---------------------------------------------------------------- C. lkd


def delete_package_actions(ctx: Context):
    """LINK-034/036: lkd refuses to delete a used package by default, refuses an unknown action, id or a
    non-package source, and unlink-uses keeps the instances as ordinary files."""
    _package_copy(ctx)
    refused = ctx.rbx("lkd", "--id", "shared", ok=False, timeout=60)
    ctx.expect_in("Package shared has 2 active use(s). Choose delete-uses or unlink-uses.", refused.error, "delete-unused refusal")
    unknown = ctx.rbx("lkd", "--id", "nope", ok=False, timeout=60)
    ctx.expect_in("No link package with id nope.", unknown.error, "unknown package")
    nuke = ctx.rbx("lkd", "--id", "shared", "--action", "nuke", ok=False, timeout=60)
    ctx.expect_in('Invalid --action "nuke"', nuke.error, "unknown action")
    unlinked = ctx.rbx("lkd", "--id", "shared", "--action", "unlink-uses", timeout=60)
    ctx.expect(unlinked.get("activeUses") == 2 and len(unlinked.get("unlinkedTargets") or []) == 2 and unlinked.get("lockRemoved") is True, f"unlink-uses: {unlinked.json}")
    ctx.expect(sorted(Path(p).parent.name for p in unlinked.get("externalizedSourcePaths") or []) == ["Shared", "SharedCopy"], f"externalized: {unlinked.json}")
    ctx.expect((ctx.root / UTIL).is_file() and (ctx.root / "src/ReplicatedStorage/SharedCopy/Util.luau").is_file(), "Util.luau externalized")
    ctx.expect(not (ctx.root / "links" / "shared.renium").exists(), "package file gone")
    _mirror_link(ctx)
    local = ctx.rbx("lkd", "--id", "logger", ok=False, timeout=60)
    ctx.expect_in("Link logger source is not a renium package", local.error, "mirror link refusal")


def delete_uses_removes_everything(ctx: Context):
    """LINK-035: delete-uses removes both uses, the package file and the manifest."""
    _package_copy(ctx)
    deleted = ctx.rbx("lkd", "--id", "shared", "--action", "delete-uses", timeout=60)
    ctx.expect(deleted.get("manifestRemoved") is True and deleted.get("activeUses") == 2 and len(deleted.get("deletedTargets") or []) == 2 and deleted.get("deletedPackage") and deleted.get("services") == [RS], f"delete-uses: {deleted.json}")
    ctx.expect(not (ctx.root / "links" / "shared.renium").exists() and not (ctx.root / "renium-link.json").exists(), "package file and manifest gone")
    ctx.expect(not _found(ctx, RS, "-n", "Shared") and not _found(ctx, RS, "-n", "SharedCopy"), "store lacks both roots")


# ---------------------------------------------------------------- D. shared links


def _experience(ctx: Context, size: str = "small", places=("lobby", "race", "menu"), folder: str = "exp") -> Path:
    """L6: an experience folder whose places are fresh fixture copies; menu lacks ReplicatedStorage.Shared.
    The scratch copy's own project file goes away: an experience is never inside another project, and
    commands run from a folder below a project fail (D-L11)."""
    _env(ctx)
    (ctx.root / "renium.project.jsonc").unlink(missing_ok=True)
    exp = ctx.root / folder
    for alias in places:
        shutil.copytree(FIXTURES[size], exp / "places" / alias, ignore=shutil.ignore_patterns(".renium", "*.lock", "*.rbxl.lock"))
    layout = {"version": 2, "gameId": 0, "startPlace": "lobby", "placeOrder": [], "places": {alias: {"placeId": 0, "name": alias.title(), "root": f"places/{alias}"} for alias in places}}
    (exp / "renium.experience.json").write_text(json.dumps(layout), encoding="utf-8")
    if "menu" in places:
        _at(ctx, "menu", "rm", RS, "-i", "Shared", folder=folder)
    return exp


def _in_experience(run):
    """Experience rows delete the scratch copy's project file (see _experience); put it back when the row
    ends, so the session's own Studio stays bound to its project."""
    @functools.wraps(run)
    def wrapper(ctx: Context):
        project = ctx.root / "renium.project.jsonc"
        saved = project.read_bytes() if project.is_file() else None
        try:
            run(ctx)
        finally:
            if saved is not None and not project.exists():
                project.write_bytes(saved)
    return wrapper


def _place(ctx: Context, alias: str, folder: str = "exp") -> Path:
    return ctx.root / folder / "places" / alias


def _at(ctx: Context, alias: str, *args: str, folder: str = "exp", **kwargs):
    """Runs rbx in one place of the experience; inside an experience the place selector is its alias."""
    kwargs.setdefault("timeout", 120)
    return ctx.rbx("--place", alias, *args, cwd=_place(ctx, alias, folder), **kwargs)


def _share(ctx: Context, alias: str = "lobby", *extra: str, path: str = SHARED, folder: str = "exp", **kwargs):
    return _at(ctx, alias, "lkp", "--share", *extra, "--service", RS, "--path", path, folder=folder, **kwargs)


def _open_place(ctx: Context, alias: str, folder: str = "exp"):
    """Opens one experience place in a scratch Studio and checks the connected Studio carries that file."""
    root = _place(ctx, alias, folder)
    place = sorted(p.name for p in root.glob("*.rbxl"))[0]
    _at(ctx, alias, "so", place, folder=folder, timeout=240)
    expected = str((root / place).resolve()).lower()
    listing = _at(ctx, alias, "cs", folder=folder, ok=None, timeout=60)
    files = [str(Path(c.get("localFile")).resolve()).lower() for c in listing.get("clients") or [] if c.get("localFile")]
    ctx.expect(expected in files, f"no connected Studio carries {expected}: {files}")


def _close_place(ctx: Context, alias: str, folder: str = "exp"):
    _at(ctx, alias, "sx", "--terminate", folder=folder, ok=None, timeout=120)
    time.sleep(1.5)


def _util(ctx: Context, alias: str) -> Path:
    return _place(ctx, alias) / UTIL


@_in_experience
def share_from_the_source_place(ctx: Context):
    """LINK-037/041/042/043: lkp --share packs the subtree at the experience root and links the places that
    have it read-only; lks/lk --experience report and repair staleness and drift; consumers cannot repack."""
    exp = _experience(ctx)
    layout = json.loads((exp / "renium.experience.json").read_text(encoding="utf-8"))
    shared = _share(ctx)
    ctx.expect(shared.get("source") == "lobby" and shared.get("linked") == ["race"] and shared.get("skipped") == [{"place": "menu", "reason": "no ReplicatedStorage.Shared"}], f"share: {shared.json}")
    ctx.expect(Path(shared.get("package", "")).resolve() == (exp / "links" / "shared.renium").resolve(), f"package path: {shared.get('package')}")
    recorded = json.loads((exp / "renium.experience.json").read_text(encoding="utf-8"))
    records = recorded.pop("sharedLinks", None)
    ctx.expect(recorded == layout and [(r.get("id"), r.get("source")) for r in records or []] == [("shared", "lobby")], f"experience file: {records}")
    race = _manifest(ctx, _place(ctx, "race"))
    ctx.expect([(link["id"], link["readOnly"]) for link in race["links"]] == [("shared", True)], f"race manifest: {race}")
    ctx.expect(not os.access(_util(ctx, "race"), os.W_OK) and os.access(_util(ctx, "lobby"), os.W_OK), "race copy read-only, lobby editable")
    status = lambda: (ctx.rbx("lks", "--experience", cwd=exp, timeout=60).get("shared") or [{}])[0]
    first = status()
    ctx.expect(first.get("stale") is False and first.get("places") == {"race": "ok"}, f"lks --experience: {first}")
    _util(ctx, "lobby").write_text("return { version = 2 }\n", encoding="utf-8")
    stale = status()
    ctx.expect(stale.get("stale") is True and stale.get("places") == {"race": "ok"}, f"after the source edit: {stale}")
    _writable(_util(ctx, "race"))
    _util(ctx, "race").write_text("return { version = 99 }\n", encoding="utf-8")
    drift = status()
    ctx.expect(drift.get("places") == {"race": "drift"} and "race" in (drift.get("drift") or {}), f"after the copy edit: {drift}")
    applied = ctx.rbx("lk", "--experience", cwd=exp, timeout=120)
    ctx.expect(applied.ok and not any(entry.get("error") for entry in applied.get("shared") or []), f"lk --experience: {applied.json}")
    ctx.expect(_util(ctx, "race").read_text(encoding="utf-8") == _util(ctx, "lobby").read_text(encoding="utf-8") and not os.access(_util(ctx, "race"), os.W_OK), "race copy follows lobby, read-only")
    final = status()
    ctx.expect(final.get("stale") is False and final.get("places") == {"race": "ok"}, f"after lk --experience: {final}")
    repack = _at(ctx, "race", "lkp", "--id", "shared", "--service", RS, "--path", SHARED, ok=False)
    ctx.expect_in("Link shared follows a package shared from another place of the experience", repack.error, "consumer repack refusal")


@_in_experience
def share_moves_stray_files(ctx: Context):
    """LINK-038: a consumer file the package does not own moves to .renium/link-replaced/<id>/."""
    _experience(ctx)
    stray = _place(ctx, "race") / "src/ReplicatedStorage/Shared/Stray.luau"
    stray.write_text("return 'stray'\n", encoding="utf-8")
    shared = _share(ctx)
    ctx.expect(shared.get("replaced") == {"race": 1}, f"share: {shared.json}")
    moved = list((_place(ctx, "race") / ".renium" / "link-replaced" / "shared").rglob("Stray.luau"))
    ctx.expect(not stray.exists() and len(moved) == 1, f"stray file moved: {moved}")


@_in_experience
def share_all_places(ctx: Context):
    """LINK-039: --all-places creates the subtree in places that lack it."""
    _experience(ctx)
    shared = _share(ctx, "lobby", "--all-places")
    ctx.expect(sorted(shared.get("linked") or []) == ["menu", "race"], f"share --all-places: {shared.json}")
    ctx.expect(_util(ctx, "menu").is_file(), "menu has Shared/Util")


@_in_experience
def share_refusals(ctx: Context):
    """LINK-040: --share needs an experience, refuses --writable, an overlap with a link of the place and a
    second id for a subtree already shared."""
    _env(ctx)
    plain = ctx.rbx("lkp", "--share", "--service", RS, "--path", SHARED, ok=False, timeout=60)
    ctx.expect_in("--share needs a multi-place experience", plain.error, "no experience")
    _experience(ctx)
    writable = _share(ctx, "lobby", "--writable", ok=False)
    ctx.expect_in("drop --writable", writable.error, "--writable")
    _share(ctx, "lobby", "--id", "x")
    second = _share(ctx, "lobby", "--id", "y", ok=False)
    ctx.expect_in("ReplicatedStorage.Shared is already shared as x", second.error, "second id")
    _experience(ctx, places=("lobby", "race"), folder="exp2")
    _at(ctx, "lobby", "lkp", "--link-folder", "links", "--id", "shared", "--service", RS, "--path", SHARED, folder="exp2")
    overlap = _share(ctx, "lobby", folder="exp2", ok=False)
    ctx.expect_in('overlaps link "shared" in this place', overlap.error, "overlap with a local link")


@_in_experience
def live_sync_propagates_from_the_source(ctx: Context):
    """LINK-044: Live Sync in the source place repacks a shared link and applies it to the other places.
    The session's own Studio closes first: one experience place is open at a time."""
    ctx.close_studio()
    _experience(ctx)
    _share(ctx)
    _open_place(ctx, "lobby")
    try:
        _at(ctx, "lobby", "lon", "--wait", "60", timeout=200)
        log = LOCAL / "logs" / "daemon.log"
        offset = log.stat().st_size if log.is_file() else 0
        _util(ctx, "lobby").write_text("return { version = 3 }\n", encoding="utf-8")
        _poll(ctx, lambda: "version = 3" in _util(ctx, "race").read_text(encoding="utf-8"), 60, "race never received the repack")
        status = _at(ctx, "lobby", "lst", ok=None)
        ctx.expect(not status.pointer("/daemon/error") and not status.get("error"), f"lst error: {status.json}")
        if log.is_file():
            with log.open("rb") as handle:
                handle.seek(offset)
                tail = handle.read().decode("utf-8", "replace")
            ctx.expect_in("shared links: shared repacked from lobby, applied to race", tail, "daemon log")
    finally:
        _close_place(ctx, "lobby")


@_in_experience
def consumer_push_guard_and_pull(ctx: Context):
    """LINK-045/046: in a consumer place an unchanged copy pushes, an edited copy is refused, and a pull
    restores a copy Studio changed."""
    ctx.close_studio()
    _experience(ctx)
    _share(ctx)
    ctx.rbx("lk", "--experience", cwd=ctx.root / "exp", timeout=120)
    _open_place(ctx, "race")
    try:
        pushed = _at(ctx, "race", "ps", UTIL, "--yes", timeout=180)
        ctx.expect(pushed.ok, f"unchanged copy push: {pushed.json}")
        copy = _util(ctx, "race")
        original = copy.read_text(encoding="utf-8")
        _writable(copy)
        copy.write_text("return 'hack'\n", encoding="utf-8")
        refused = _at(ctx, "race", "ps", UTIL, "--yes", ok=False, timeout=180)
        ctx.expect_in('Cannot edit read-only link "shared"', refused.error, "edited copy refused")
        ctx.expect_in("Apply the link again or use --override-packages", refused.error, "refusal names the way out")
        copy.write_text(original, encoding="utf-8")
        os.chmod(copy, stat.S_IREAD)
        _at(ctx, "race", "l", "game.ReplicatedStorage.Shared.Util.Source = 'return 7' return true")
        pulled = _at(ctx, "race", "pl", timeout=180)
        ctx.expect(any("shared" in json.dumps(entry) for entry in pulled.get("readOnlyLinksRestored") or []), f"consumer pull: {pulled.json}")
        ctx.expect(copy.read_text(encoding="utf-8") == _util(ctx, "lobby").read_text(encoding="utf-8") and not os.access(copy, os.W_OK), "race copy back to the package, read-only")
    finally:
        _close_place(ctx, "race")


@_in_experience
def shared_root_with_a_package_link(ctx: Context):
    """LINK-047 (heavy): a PackageLink right under a shared root is stripped from the package and the copies."""
    _experience(ctx, "heavy", ("lobby", "race"))
    shared = _share(ctx, path=ICON, timeout=240)
    ctx.expect(shared.get("strippedPackageLinks") == ["ReplicatedStorage.TopbarPlus.Icon.PackageLink"] and "upl" in str(shared.get("next")), f"share: {shared.json}")
    ctx.expect(not _at(ctx, "race", "f", RS, "-c", "PackageLink", "--all").json, "race copy has no PackageLink")
    ctx.expect(_at(ctx, "lobby", "f", RS, "-c", "PackageLink", "--all").json, "lobby keeps its PackageLink")


@_in_experience
def consumer_keeps_its_package_link(ctx: Context):
    """LINK-048 (heavy, D-L2): the consumer place ends without the PackageLink: Live Sync starts clean,
    lks --experience reports it ok and upl unlinks the Studio copy."""
    ctx.close_studio()
    _experience(ctx, "heavy", ("lobby", "race"))
    _share(ctx, path=ICON, timeout=240)
    _open_place(ctx, "race")
    try:
        _pair_heavy(lambda *args, **kwargs: _at(ctx, "race", *args, **kwargs))
        started = _at(ctx, "race", "lon", "--wait", "90", ok=None, timeout=300)
        status = (ctx.rbx("lks", "--experience", cwd=ctx.root / "exp", timeout=60).get("shared") or [{}])[0]
        unlinked = _at(ctx, "race", "upl", RS, "-i", "TopbarPlus.Icon", ok=None, timeout=180)
        ctx.expect(started.ok, f"race lon: {started.error[:300]}")
        ctx.expect(status.get("places") == {"race": "ok"}, f"lks --experience after lon: {status}")
        ctx.expect(unlinked.ok, f"race upl: {unlinked.error[:300]}")
    finally:
        _close_place(ctx, "race")


# ---------------------------------------------------------------- E. rbx wally

WALLY_TOML = """[package]
name = "scenario/fixture"
version = "0.1.0"
realm = "shared"
registry = "https://github.com/UpliftGames/wally-index"

[dependencies]
Promise = "evaera/promise@4.0.0"
"""


def _wally(ctx: Context, full: bool = False, server: bool = False) -> Path:
    """L5: wally.toml plus a package template; full copies the template's Packages into the project."""
    _env(ctx)
    ctx.write("wally.toml", WALLY_TOML)
    for realm in ("Packages", "ServerPackages"):
        ctx.write(f"wally-template/{realm}/Promise.lua", 'return require(script.Parent._Index["evaera_promise@4.0.0"]["promise"])\n')
        ctx.write(f"wally-template/{realm}/_Index/evaera_promise@4.0.0/promise/init.lua", "return {}\n")
    template = ctx.root / "wally-template"
    if full:
        shutil.copytree(template / "Packages", ctx.root / "Packages")
    if server:
        shutil.copytree(template / "ServerPackages", ctx.root / "ServerPackages")
    return template


def _fake_wally(ctx: Context, template: Path) -> str:
    """The fake installer copies the template's Packages into its working directory and exits FAKE_RC."""
    if os.name == "nt":
        path = ctx.root / "fakewally.cmd"
        path.write_text(f'@echo off\r\nxcopy /E /I /Y "{template / "Packages"}" Packages >nul\r\nexit /b %FAKE_RC%\r\n', encoding="utf-8")
    else:
        path = ctx.root / "fakewally"
        path.write_text(f'#!/bin/sh\ncp -R "{template / "Packages"}" Packages\nexit ${{FAKE_RC:-0}}\n', encoding="utf-8")
        os.chmod(path, 0o755)
    return str(path)


def wally_refusals(ctx: Context):
    """LINK-050/051: wally needs a manifest, known realms and, with --skip-install, an installed Packages folder."""
    _env(ctx)
    missing = ctx.rbx("wally", ok=False, timeout=60)
    ctx.expect_in("No Wally manifest found at", missing.error, "no manifest")
    _wally(ctx)
    bogus = ctx.rbx("wally", "--realms", "shared,bogus", ok=False, timeout=60)
    ctx.expect_in("Unknown Wally realm 'bogus'", bogus.error, "bad realm")
    empty = ctx.rbx("wally", "--realms", ",", ok=False, timeout=60)
    ctx.expect_in("No Wally realms selected", empty.error, "no realm")
    skip = ctx.rbx("wally", "--skip-install", ok=False, timeout=60)
    ctx.expect_in("Wally packages directory was not found at", skip.error, "no Packages")
    ctx.expect_in("Run without --skip-install", skip.error, "no Packages hint")


def wally_import_offline(ctx: Context):
    """LINK-052/053/056: --skip-install imports the installed Packages, a rerun skips a fresh realm and
    --force reimports; a server realm is added and removed with its folder."""
    _wally(ctx, full=True)
    imported = ctx.rbx("wally", "--skip-install", "--details", timeout=120)
    realm = (imported.get("realms") or [{}])[0]
    ctx.expect(imported.get("appliedRealms") == 1 and (imported.get("importedInstanceCount") or 0) >= 2 and realm.get("service") == RS and realm.get("targetName") == "Packages" and realm.get("skipped") is False and imported.pointer("/wallyInstall/skipped") is True, f"wally: {imported.json}")
    ctx.expect([entry.get("c") for entry in _found(ctx, RS, "-n", "Promise")] == ["ModuleScript"], "store has Packages.Promise")
    store = _digest(ctx.root / RS_STORE)
    rerun = ctx.rbx("wally", "--skip-install", timeout=120)
    ctx.expect(rerun.get("skippedRealms") == 1 and rerun.get("appliedRealms") == 0 and _digest(ctx.root / RS_STORE) == store, f"rerun: {rerun.json}")
    forced = ctx.rbx("wally", "--skip-install", "--force", timeout=120)
    ctx.expect(forced.get("appliedRealms") == 1, f"--force: {forced.json}")
    shutil.copytree(ctx.root / "wally-template" / "ServerPackages", ctx.root / "ServerPackages")
    added = ctx.rbx("wally", "--skip-install", timeout=120)
    ctx.expect(any(r.get("service") == "ServerStorage" and r.get("skipped") is False for r in added.get("realms") or []), f"server realm: {added.json}")
    ctx.expect(any(entry.get("path") == "ServerStorage.ServerPackages.Promise" for entry in _found(ctx, "ServerStorage", "-n", "Promise")), "ServerStorage.ServerPackages imported")
    shutil.rmtree(ctx.root / "ServerPackages")
    removed = ctx.rbx("wally", "--skip-install", timeout=120)
    ctx.expect(removed.get("removedTargetCount") == 1 and any(r.get("removed") is True for r in removed.get("realms") or []), f"server realm removed: {removed.json}")
    ctx.expect(not _found(ctx, "ServerStorage", "-n", "ServerPackages"), "ServerPackages gone from the store")


def wally_fake_installer(ctx: Context):
    """LINK-054: wally runs the given installer, reports its failure exit code and a missing installer."""
    template = _wally(ctx)
    fake = _fake_wally(ctx, template)
    try:
        os.environ["FAKE_RC"] = "0"
        installed = ctx.rbx("wally", "--wally-path", fake, timeout=120)
        ctx.expect(installed.pointer("/wallyInstall/status") == 0 and installed.pointer("/wallyInstall/command") == fake and installed.get("appliedRealms") == 1, f"fake install: {installed.json}")
        ctx.expect(_found(ctx, RS, "-n", "Promise"), "Packages imported")
        os.environ["FAKE_RC"] = "3"
        failed = ctx.rbx("wally", "--wally-path", fake, "--force", ok=False, timeout=120)
        ctx.expect_in("wally install failed with exit code 3", failed.error, "installer failure")
    finally:
        os.environ.pop("FAKE_RC", None)
    absent = ctx.rbx("wally", "--wally-path", "nowally-xyz", "--force", ok=False, timeout=120)
    ctx.expect_in("Failed to launch wally install using command", absent.error, "missing installer (D-L10 on Windows)")


def wally_overlap_with_a_link(ctx: Context):
    """LINK-055: a Wally realm target that overlaps a link is refused before anything is written."""
    _wally(ctx, full=True)
    _pack_shared(ctx)
    store = _digest(ctx.root / RS_STORE)
    overlap = ctx.rbx("wally", "--skip-install", "--target-name", "Shared", ok=False, timeout=120)
    ctx.expect_in("Wally shared realm target ReplicatedStorage.Shared overlaps link shared target", overlap.error, "overlap refusal")
    ctx.expect(_digest(ctx.root / RS_STORE) == store and not (ctx.root / "src/ReplicatedStorage/Shared").exists(), "nothing written")


def wally_packages_reach_studio(ctx: Context):
    """LINK-057: imported Wally packages reach Studio with a store push."""
    _wally(ctx, full=True)
    ctx.rbx("wally", "--skip-install", timeout=120)
    ctx.open_studio()
    ctx.rbx("ps", RS_STORE, "--yes", timeout=180)
    ctx.expect(ctx.luau("return game.ReplicatedStorage.Packages.Promise.ClassName") == "ModuleScript", "Packages.Promise in Studio")


# ---------------------------------------------------------------- F. Roblox packages


def package_argument_bounds(ctx: Context):
    """LINK-060/065: package timeouts are bounded before anything runs, and no command creates a PackageLink."""
    _env(ctx)
    for command, timeout in (("pd", "0"), ("pp", "601"), ("pu", "nan")):
        bounded = ctx.rbx(command, "Workspace.Props", "--timeout", timeout, ok=False, timeout=60)
        ctx.expect_in("Package timeout must be >0 and <=600s", bounded.error, f"{command} --timeout {timeout}")
    created = ctx.rbx("ba", "Workspace", "-n", "L", "-c", "PackageLink", ok=False, timeout=60)
    ctx.expect_in("PackageLink instances can only be created by Roblox package workflows", created.error, "ba PackageLink")


def package_commands_refuse_non_packages(ctx: Context):
    """LINK-061/062/063/067: pd and pp refuse a folder that is not a package, malformed targets and a pid that
    is not Studio; upl on a folder without a PackageLink reports it already unlinked (files-only: refused)."""
    _env(ctx)
    ctx.close_studio()
    closed = ctx.rbx("upl", "Workspace", "-i", "Props", ok=False, timeout=60)
    ctx.expect_in("Workspace.Props has no direct PackageLink child", closed.error, "files-only upl")
    ctx.open_studio()
    children = ctx.luau("return #workspace.Props:GetChildren()")
    for command in ("pd", "pp"):
        refused = ctx.rbx(command, "Workspace.Props", ok=False, timeout=120)
        ctx.expect_in("Target is not a Roblox package root", refused.error, f"{command} on a folder")
    empty = ctx.rbx("pd", "Workspace..Props", ok=False, timeout=120)
    ctx.expect_in("Package target contains an empty path segment", empty.error, "empty segment")
    ords = ctx.rbx("pd", "Workspace.Props", "--ords", "0", ok=False, timeout=120)
    ctx.expect_in("Package ordinals must be positive integers", ords.error, "ordinal 0")
    started = time.monotonic()
    other = ctx.rbx("pd", "Workspace.Props", "--pid", str(os.getpid()), "--timeout", "5", ok=False, timeout=120)
    ctx.expect(time.monotonic() - started < 20, f"--pid not Studio took {time.monotonic() - started:.1f}s: {other.error[:200]}")
    ctx.expect(ctx.luau("return #workspace.Props:GetChildren()") == children, "Studio unchanged")
    unlinked = ctx.rbx("upl", "Workspace", "-i", "Props", timeout=120)
    ctx.expect(unlinked.get("studio") == "already unlinked" and unlinked.get("files") == "already unlinked", f"upl with Studio open: {unlinked.json}")


HEAVY_DIVERGED = "src/ServerStorage/BatchTest/Config.luau"


def _pair_heavy(run):
    """H10 pairing: the heavy fixture's files carry an edit of BatchTest/Config.luau that its place lacks;
    pushing that one file leaves Live Sync's first comparison nothing unrelated to review."""
    run("ps", HEAVY_DIVERGED, "--yes", timeout=180)


ICON_LINK = 'local l = game.ReplicatedStorage.TopbarPlus.Icon:FindFirstChildOfClass("PackageLink") return l and l.VersionNumber or "none"'


def foreign_package_refused_then_unlinked(ctx: Context):
    """LINK-064/068 (heavy): pd and pp on a package this account does not own are refused with its creator;
    upl with Studio open removes the link in Studio and in the files, and Live Sync then starts clean."""
    _env(ctx)
    ctx.open_studio(wait=240)
    _pair_heavy(ctx.rbx)
    version = ctx.luau(ICON_LINK)
    ctx.expect(version != "none", "the place carries the TopbarPlus.Icon PackageLink")
    for command in ("pd", "pp"):
        refused = ctx.rbx(command, "ReplicatedStorage.TopbarPlus.Icon", ok=False, timeout=180)
        ctx.expect_in("The change touches a Roblox package this account does not own", refused.error, f"{command} refusal")
        ctx.expect_in("by group HD Admin", refused.error, f"{command} names the creator")
    ctx.expect(ctx.luau(ICON_LINK) == version, "VersionNumber unchanged")
    unlinked = ctx.rbx("upl", RS, "-i", "TopbarPlus.Icon", timeout=180)
    ctx.expect(len(unlinked.get("removedPackageLinks") or []) == 1, f"upl: {unlinked.json}")
    ctx.expect(ctx.luau(ICON_LINK) == "none", "Studio link removed")
    started = ctx.rbx("lon", "--wait", "90", ok=None, timeout=300)
    ctx.expect(started.ok and "PackageLink" not in started.error, f"lon after upl: {started.error[:300]}")


def unlink_under_live_sync(ctx: Context):
    """LINK-069 (heavy): upl under Live Sync unlinks in Studio and Live Sync pulls the result into the files."""
    _env(ctx)
    ctx.open_studio(wait=240)
    _pair_heavy(ctx.rbx)
    ctx.rbx("lon", "--wait", "90", timeout=300)
    unlinked = ctx.rbx("upl", RS, "-i", "TopbarPlus.Icon", timeout=180)
    ctx.expect(unlinked.get("files") == "Live Sync pulls the unlinked package into the project", f"upl: {unlinked.json}")
    settled = ctx.rbx("lst", "--wait", "60", timeout=150)
    ctx.expect(settled.pointer("/daemon/pendingCount", 0) == 0, f"pending after upl: {settled.json}")
    _poll(ctx, lambda: not _found(ctx, RS, "-c", "PackageLink"), 60, "the store still holds the PackageLink")


def files_only_unlink_then_reconcile(ctx: Context):
    """LINK-070 (heavy): after one pairing (H10), a files-only upl leaves Studio's link; lon reports the
    store that omits the PackageLink instead of applying it; upl with Studio open then settles."""
    _env(ctx)
    ctx.open_studio(wait=240)
    _pair_heavy(ctx.rbx)
    ctx.rbx("lon", "--wait", "90", timeout=300)
    ctx.rbx("lof", timeout=120)
    ctx.close_studio(save=True)
    files = ctx.rbx("upl", RS, "-i", "TopbarPlus.Icon", timeout=120)
    ctx.expect(len(files.get("removedPackageLinks") or []) == 1, f"files-only upl: {files.json}")
    ctx.open_studio(wait=240)
    ctx.expect(ctx.luau(ICON_LINK) != "none", "Studio keeps its link")
    started = ctx.rbx("lon", "--wait", "90", ok=None, timeout=300)
    details = ctx.rbx("lst", "--details", ok=None, timeout=120)
    text = " ".join([started.error, details.error, details.stdout])
    ctx.expect_in("omits PackageLink PackageLink; ordinary sync preserves package relationships", text, "lon reports the store that dropped the PackageLink")
    ctx.expect(started.get("ok") is False or details.get("ok") is False, f"lon needs a decision: {started.json}")
    ctx.expect(ctx.luau(ICON_LINK) != "none", "Studio link intact")
    ctx.rbx("upl", RS, "-i", "TopbarPlus.Icon", ok=None, timeout=180)
    settled = ctx.rbx("lst", "--wait", "60", ok=None, timeout=150)
    ctx.expect(settled.ok and settled.pointer("/daemon/pendingCount", 0) == 0 and ctx.luau(ICON_LINK) == "none", f"after upl with Studio open: {settled.error[:300] or settled.json}")


def live_sync_package_link_guard(ctx: Context):
    """LINK-071 (heavy): a store that drops the PackageLink arriving under Live Sync is refused; Studio keeps it."""
    _env(ctx)
    ctx.close_studio()
    other = ctx.root / "other"
    shutil.copytree(FIXTURES["heavy"], other, ignore=shutil.ignore_patterns(".renium", "*.lock", "*.rbxl.lock"))
    ctx.rbx("upl", RS, "-i", "TopbarPlus.Icon", cwd=other, timeout=120)
    ctx.open_studio(wait=240)
    _pair_heavy(ctx.rbx)
    ctx.rbx("lon", "--wait", "90", timeout=300)
    shutil.copyfile(other / RS_STORE, ctx.root / RS_STORE)
    error = _poll(ctx, lambda: (lambda status: str(status.pointer("/daemon/error") or status.get("error") or ""))(ctx.rbx("lst", ok=None, timeout=60)), 60, "Live Sync never refused the PackageLink change", interval=2)
    ctx.expect_in("changes a PackageLink directly; use the package workflow instead", error, "guard message")
    ctx.expect(ctx.luau(ICON_LINK) != "none", "Studio link intact")


def foreign_package_store_edits_refused(ctx: Context):
    """LINK-066 (heavy, D-L4): store edits that would remove or change a PackageLink are refused, a model
    holding one cannot be imported, and upl names the instance that has the direct link. Studio stays
    closed: with Studio open upl asks Studio first."""
    _env(ctx)
    ctx.close_studio()
    removed = ctx.rbx("rm", RS, "-i", "TopbarPlus", ok=False, timeout=120)
    ctx.expect_in("Package-bearing instances cannot be removed; use upl", removed.error, "rm refusal")
    edited = ctx.rbx("bs", RS, "TopbarPlus.Icon.PackageLink", "-p", "Name", "--str", "X", ok=False, timeout=120)
    ctx.expect_in("PackageLink instances cannot be edited; use upl", edited.error, "bs refusal")
    parent = ctx.rbx("upl", RS, "-i", "TopbarPlus", ok=False, timeout=120)
    ctx.expect_in("ReplicatedStorage.TopbarPlus has no direct PackageLink child", parent.error, "upl on the package's parent")
    ctx.rbx("mep", RS, "-i", "TopbarPlus.Icon", "-o", "icon.rbxm", timeout=120)
    parent = _found(ctx, "Workspace", "-c", "Folder")[0]["id"]
    imported = ctx.rbx("mip", "Workspace", "-I", parent, "-m", str(ctx.root / "icon.rbxm"), ok=False, timeout=120)
    ctx.expect_in("Models containing PackageLink instances cannot be imported", imported.error, "mip refusal")


SCENARIOS = [
    Scenario("LINK-001", "pack a subtree into the project", "small", pack_a_subtree_into_the_project, needs_studio=False),
    Scenario("LINK-002", "pack into the global library", "small", global_library_pack, needs_studio=False),
    Scenario("LINK-004", "pack inside or around a link", "small", pack_inside_or_around_a_link, needs_studio=False),
    Scenario("LINK-005", "pack refusals", "small", pack_refusals, needs_studio=False),
    Scenario("LINK-007", "packed source survives a push", "small", packed_source_survives_a_push),
    Scenario("LINK-010", "file mirror link lifecycle", "small", file_mirror_link_lifecycle, needs_studio=False),
    Scenario("LINK-011", "link add refusals", "small", link_add_refusals, needs_studio=False),
    Scenario("LINK-014", "status reports links and drift", "small", status_reports_links_and_drift, needs_studio=False),
    Scenario("LINK-015", "mirror push fans out and edits revert", "small", mirror_push_fans_out_and_edits_revert),
    Scenario("LINK-017", "package at a second path", "small", package_at_a_second_path, needs_studio=False),
    Scenario("LINK-018", "edits inside a read-only package refused", "small", read_only_package_edits_refused, needs_studio=False),
    Scenario("LINK-020", "writable package keeps edits", "small", writable_package_keeps_edits, needs_studio=False),
    Scenario("LINK-021", "force one target", "small", force_target_recreates_one_target, needs_studio=False),
    Scenario("LINK-022", "unowned file under a target", "small", unowned_file_under_a_target, needs_studio=False),
    Scenario("LINK-023", "break and remove targets", "small", break_and_remove_targets, needs_studio=False),
    Scenario("LINK-026", "git source offline", "small", git_source_offline, needs_studio=False),
    Scenario("LINK-028", "wally link source", "small", wally_link_source, needs_studio=False),
    Scenario("LINK-029", "lk under Live Sync", "small", apply_under_live_sync),
    Scenario("LINK-030", "pull restores a package Studio changed", "small", pull_restores_a_package_studio_changed),
    Scenario("LINK-031", "pull keeps writable links", "small", pull_keeps_writable_links),
    Scenario("LINK-032", "invalid manifest", "small", invalid_manifest, needs_studio=False),
    Scenario("LINK-033", "target path cannot escape", "small", target_path_cannot_escape, needs_studio=False),
    Scenario("LINK-034", "delete package actions", "small", delete_package_actions, needs_studio=False),
    Scenario("LINK-035", "delete-uses removes everything", "small", delete_uses_removes_everything, needs_studio=False),
    Scenario("LINK-037", "share from the source place", "small", share_from_the_source_place, needs_studio=False),
    Scenario("LINK-038", "share moves stray files", "small", share_moves_stray_files, needs_studio=False),
    Scenario("LINK-039", "share to all places", "small", share_all_places, needs_studio=False),
    Scenario("LINK-040", "share refusals", "small", share_refusals, needs_studio=False),
    Scenario("LINK-044", "Live Sync propagates a shared link", "small", live_sync_propagates_from_the_source),
    Scenario("LINK-045", "consumer push guard and pull", "small", consumer_push_guard_and_pull),
    Scenario("LINK-047", "shared root with a PackageLink", "heavy", shared_root_with_a_package_link, needs_studio=False),
    Scenario("LINK-048", "consumer keeps its PackageLink", "heavy", consumer_keeps_its_package_link),
    Scenario("LINK-050", "wally refusals", "small", wally_refusals, needs_studio=False),
    Scenario("LINK-052", "wally import offline", "small", wally_import_offline, needs_studio=False),
    Scenario("LINK-054", "wally fake installer", "small", wally_fake_installer, needs_studio=False),
    Scenario("LINK-055", "wally overlap with a link", "small", wally_overlap_with_a_link, needs_studio=False),
    Scenario("LINK-057", "wally packages reach Studio", "small", wally_packages_reach_studio),
    Scenario("LINK-060", "package argument bounds", "small", package_argument_bounds, needs_studio=False),
    Scenario("LINK-061", "package commands refuse non-packages", "small", package_commands_refuse_non_packages),
    Scenario("LINK-064", "foreign package refused, then unlinked", "heavy", foreign_package_refused_then_unlinked),
    Scenario("LINK-066", "foreign package store edits refused", "heavy", foreign_package_store_edits_refused, needs_studio=False),
    Scenario("LINK-069", "upl under Live Sync", "heavy", unlink_under_live_sync),
    Scenario("LINK-070", "files-only upl, then reconcile", "heavy", files_only_unlink_then_reconcile),
    Scenario("LINK-071", "Live Sync PackageLink guard", "heavy", live_sync_package_link_guard),
    Scenario("LINK-072", "link commands from a subfolder", "small", link_commands_from_a_subfolder, needs_studio=False),
    Scenario("LINK-073", "missing link package stops the comparison", "small", missing_link_package_stops_the_comparison),
]


def _leaves_files_writable(run):
    """Mirrors and shared copies are read-only files; make every file under the scratch copy writable
    again when a row ends, so the session runner can restore the copy from the fixture."""
    @functools.wraps(run)
    def wrapper(ctx: Context):
        try:
            run(ctx)
        finally:
            for path in ctx.root.rglob("*"):
                try:
                    if path.is_file() and not os.access(path, os.W_OK):
                        _writable(path)
                except OSError:
                    pass
    return wrapper


for _scenario in SCENARIOS:
    _scenario.run = _leaves_files_writable(_scenario.run)
