"""Saved-data commands used live against a connected Studio (matrix/lifecycle.md, DATA rows):
direct editor mutations, script text edits, script search, model round trips, store edits
followed by a push, and the Studio review of direct mutations."""

from __future__ import annotations

import json
import os
import sys
import subprocess
import time
from contextlib import contextmanager
from pathlib import Path

from areas.lifecycle import _env, _failure, _matches, _run, slotted
from run import RBX, Context, Scenario

MAIN = "src/ServerScriptService/Main.server.luau"
WORKSPACE = "instances/Workspace.renium"
RS_STORE = "instances/ReplicatedStorage.renium"
SIGN = ("-s", "Workspace", "-p", '["Props","Sign"]')
CRATE_2 = ("-s", "Workspace", "-c", "Part", "-p", '["Props","Crate"]', "-o", "[1,2]")
TECHNOLOGIES = ("Voxel", "ShadowMap", "Future", "Compatibility")


def _live(ctx: Context):
    ctx.open_studio()
    ctx.rbx("lon", "--wait", "60", timeout=200)


def _settled(ctx: Context, wait: int = 30):
    status = ctx.rbx("lst", "--wait", str(wait), timeout=wait + 60)
    ctx.expect(status.pointer("/daemon/pendingCount", 0) == 0 and not status.pointer("/daemon/resolutionRequired") and not status.pointer("/daemon/error"), f"after settle: {status.json}")
    return status


def _sign(ctx: Context):
    return ctx.luau("local s = workspace.Props:FindFirstChild('Sign') return s and s.Value")


def _stored(ctx: Context, target: str = "Props.Sign", prop: str = "Value"):
    return ctx.rbx("bg", "Workspace", target, prop, timeout=60).json


def _found(ctx: Context, *args: str) -> list:
    return _matches(ctx.rbx("f", *args, timeout=60))


def _mark_first_crate(ctx: Context):
    ctx.luau("for _, c in workspace.Props:GetChildren() do if c.Name == 'Crate' then c:SetAttribute('First', true) break end end return true")


def _documented_fields(ctx: Context, result, label: str):
    """D2: publishing.md says prop/del/me answer with persistence like every Edit-window change."""
    ctx.expect("persistence" in (result.json or {}), f"{label} answers without persistence (D2): {result.json}")


# ---------------------------------------------------------------- J. prop, del, me


def prop_yes_edits_studio_only(ctx: Context):
    """DATA-001 (D2): prop --yes writes Studio and leaves the store until a pull."""
    ctx.open_studio()
    changed = ctx.rbx("prop", *SIGN, "-c", "StringValue", "-n", "Value", "-j", '"hi"', "--yes", timeout=60)
    ctx.expect(changed.get("changed") is True, f"prop: {changed.json}")
    ctx.expect(_sign(ctx) == "hi", "Studio value")
    before = _stored(ctx)
    ctx.expect(before != "hi", f"store changed before a pull: {before}")
    ctx.rbx("pl", timeout=180)
    ctx.expect(_stored(ctx) == "hi", "store after the pull")
    _documented_fields(ctx, changed, "prop --yes")


def prop_without_yes_applies_one_change(ctx: Context):
    """DATA-002: prop without --yes applies a single change at once (one change is below the threshold)."""
    ctx.open_studio()
    started = time.monotonic()
    changed = ctx.rbx("prop", *SIGN, "-n", "Value", "-j", '"one"', timeout=60)
    ctx.expect(changed.get("changed") is True and time.monotonic() - started < 30, f"single prop not held: {changed.json}")
    ctx.expect(_sign(ctx) == "one", "Studio value")


def prop_by_settings_id(ctx: Context):
    """DATA-003 (D5): prop addresses an instance by its settings id."""
    ctx.open_studio()
    by_id = ctx.rbx("prop", "-s", "Workspace", "-i", "editor:4", "-p", "[]", "-n", "Value", "-j", '"id"', "--yes", ok=None, timeout=60)
    ctx.expect(by_id.ok, f"prop -i editor:4 -p [] (D5): {by_id.error[:300]}")
    ctx.expect(_sign(ctx) == "id", "Studio value by id")


def prop_on_a_duplicate_by_ordinal(ctx: Context):
    """DATA-004: prop with ordinals edits the second of two same-named Crates only."""
    ctx.open_studio()
    _mark_first_crate(ctx)
    ctx.rbx("prop", *CRATE_2, "-n", "Transparency", "-j", "0.5", "--yes", timeout=60)
    values = ctx.luau("local t = {} for _, c in workspace.Props:GetChildren() do if c.Name == 'Crate' then t[#t + 1] = (c:GetAttribute('First') and 'first:' or 'second:') .. c.Transparency end end return table.concat(t, ',')")
    ctx.expect(sorted(str(values).split(",")) == ["first:0", "second:0.5"], f"crates: {values}")


def prop_attribute_set_and_remove(ctx: Context):
    """DATA-005: prop -S attribute sets an attribute and a null value removes it."""
    ctx.open_studio()
    added = ctx.rbx("prop", *SIGN, "-S", "attribute", "-n", "Tag", "-j", '"x"', "--yes", timeout=60)
    ctx.expect(added.get("changed") is True, f"prop attribute: {added.json}")
    ctx.expect(ctx.luau("return workspace.Props.Sign:GetAttribute('Tag')") == "x", "attribute set")
    ctx.rbx("prop", *SIGN, "-S", "attribute", "-n", "Tag", "-j", "null", "--yes", timeout=60)
    ctx.expect(ctx.luau("return workspace.Props.Sign:GetAttribute('Tag') == nil") is True, "attribute removed")


def prop_source_from_a_file(ctx: Context):
    """DATA-006: prop --source-file writes a script Source; --source-file needs --property Source."""
    ctx.open_studio()
    ctx.write("new.luau", 'print("from file")\n')
    ctx.rbx("prop", "-s", "ServerScriptService", "-c", "Script", "-p", '["Main"]', "-n", "Source", "--source-file", "new.luau", "--yes", timeout=60)
    ctx.expect(ctx.luau("return game.ServerScriptService.Main.Source") == ctx.read("new.luau"), "Main.Source from the file")
    refused = ctx.rbx("prop", "-s", "ServerScriptService", "-c", "Script", "-p", '["Main"]', "-n", "Name", "--source-file", "new.luau", "--yes", ok=False, timeout=60)
    ctx.expect_in("--source-file requires --property Source", refused.error, "source-file with Name")


def prop_refuses_game_settings_fields(ctx: Context):
    """DATA-007: prop refuses a field Roblox Game Settings manages and leaves Studio alone."""
    ctx.open_studio()
    before = ctx.luau("return game.Players.MaxPlayers")
    refused = ctx.rbx("prop", "-s", "Players", "-c", "Players", "-p", '["Players"]', "-n", "MaxPlayers", "-j", "10", "--yes", ok=False, timeout=60)
    ctx.expect_in("Players.MaxPlayers is managed through Roblox Game Settings", refused.error, "Game Settings field")
    ctx.expect(ctx.luau("return game.Players.MaxPlayers") == before, "Studio unchanged")


def _technology(ctx: Context) -> tuple[str, str]:
    """Lighting.Technology is not readable from plugin Luau; the pulled store reads it."""
    current = _pulled_technology(ctx)
    wanted = next(name for name in TECHNOLOGIES if name != current)
    return current, json.dumps({"_type": "EnumItem", "enumType": "Enum.Technology", "name": wanted})


def _pulled_technology(ctx: Context) -> str:
    ctx.rbx("pl", timeout=180)
    return (ctx.rbx("bg", "Lighting", "Lighting", "Technology", timeout=60).json or {}).get("name")


LIGHTING = ("-s", "Lighting", "-c", "Lighting", "-p", '["Lighting"]', "-n", "Technology")


def protected_field_without_yes(ctx: Context):
    """DATA-008 (D2, D7): prop of a field without a plugin API and without --yes stays below the review
    threshold, so it applies through the saved-field path and says so."""
    ctx.open_studio()
    current, value = _technology(ctx)
    applied = ctx.rbx("prop", *LIGHTING, "-j", value, ok=None, timeout=300)
    ctx.studio_open = True
    if sys.platform == "darwin" and not applied.ok:
        # Without a native saved-field writer (older Studio layouts) the
        # refusal names the store push; with one, macOS answers like Windows.
        ctx.expect("native property writer could not set it" in applied.error and "rbx ps instances/Lighting.renium -u --yes" in applied.error, f"macOS saved-field refusal must name the store push: {applied.json}")
        return
    ctx.expect(applied.get("changed") is True and applied.get("savedFieldsApplied") == 1, f"a protected prop without --yes neither applied nor named the field (D2/D7): {applied.json}")
    ctx.expect(_pulled_technology(ctx) == json.loads(value)["name"] != current, "Technology changed in the saved place")


def protected_field_with_yes(ctx: Context):
    """DATA-009 (D2): prop --yes of a field without a plugin API applies it (native writer or an offline reopen)."""
    ctx.open_studio()
    current, value = _technology(ctx)
    applied = ctx.rbx("prop", *LIGHTING, "-j", value, "--yes", ok=None, timeout=300)
    ctx.studio_open = True
    if sys.platform == "darwin" and not applied.ok:
        ctx.expect("no plugin API" in applied.error, f"macOS has no saved-field writer; the refusal must say so: {applied.json}")
        return
    ctx.expect(applied.ok, f"prop --yes: {applied.json}")
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline and not ctx.rbx("l", "return 1", ok=None, timeout=60).ok:
        time.sleep(2)
    ctx.guard_binding()
    name = _pulled_technology(ctx)
    ctx.expect(name == json.loads(value)["name"], f"prop --yes of a protected field changed nothing (D7): Technology {name} (was {current}); prop answered {applied.json}")
    _documented_fields(ctx, applied, "protected prop --yes")


def prop_under_live_sync(ctx: Context):
    """DATA-010 (D6): prop under Live Sync settles with no conflict and the pull brings the value to the store."""
    _live(ctx)
    ctx.rbx("prop", *SIGN, "-c", "StringValue", "-n", "Value", "-j", '"hi"', "--yes", timeout=60)
    _settled(ctx)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline and _stored(ctx) != "hi":
        ctx.rbx("lst", "--wait", "10", ok=None, timeout=60)
    ctx.expect(_stored(ctx) == "hi", f"Live Sync settled without pulling the prop value into the store (D6): {_stored(ctx)!r}")


def _link_copy(ctx: Context) -> dict:
    """SYNC H9 (links.py L2 form): the Shared subtree packed as a read-only link and applied at
    ReplicatedStorage.SharedCopy, then pushed to Studio."""
    env = _env(ctx, RENIUM_GLOBAL_PACKAGES_DIR=ctx.root / "global-packages", RENIUM_PLUGIN_HOME=ctx.root / "plugin-home")
    _run(ctx, "lkp", "--link-folder", "links", "--id", "shared", "--service", "ReplicatedStorage", "--path", '["ReplicatedStorage","Shared"]', env=env, timeout=60)
    _run(ctx, "lka", "--id", "shared", "--service", "ReplicatedStorage", "--path", '["ReplicatedStorage","SharedCopy"]', env=env, timeout=60)
    _run(ctx, "lk", env=env, timeout=60)
    ctx.open_studio()
    _run(ctx, "ps", RS_STORE, "src/ReplicatedStorage", "--yes", env=env, timeout=180)
    ctx.expect(ctx.luau("return game.ReplicatedStorage:FindFirstChild('SharedCopy') ~= nil") is True, "link copy in Studio")
    return env


def read_only_link_refuses_direct_edits(ctx: Context):
    """DATA-011: prop and del inside a read-only link copy are refused; --override-packages applies."""
    env = _link_copy(ctx)
    path = ("-s", "ReplicatedStorage", "-p", '["SharedCopy","Util"]')
    refused = _run(ctx, "prop", *path, "-S", "attribute", "-n", "Mark", "-j", '"x"', "--yes", env=env, ok=False, timeout=60)
    ctx.expect_in('Cannot edit read-only link "shared"', refused.error, "prop refusal")
    deleted = _run(ctx, "del", *path, env=env, ok=False, timeout=60)
    ctx.expect_in('Cannot edit read-only link "shared"', deleted.error, "del refusal")
    ctx.expect(ctx.luau("local u = game.ReplicatedStorage.SharedCopy:FindFirstChild('Util') return u ~= nil and u:GetAttribute('Mark') == nil") is True, "Studio unchanged")
    _run(ctx, "prop", *path, "-S", "attribute", "-n", "Mark", "-j", '"x"', "--yes", "--override-packages", env=env, timeout=60)
    ctx.expect(ctx.luau("return game.ReplicatedStorage.SharedCopy.Util:GetAttribute('Mark')") == "x", "override applied")


def del_then_pull(ctx: Context):
    """DATA-012: del removes an instance in Studio (the class is optional, D14 fixed); the store keeps it until a pull."""
    ctx.open_studio()
    deleted = ctx.rbx("del", *SIGN, timeout=60)
    ctx.expect(deleted.get("changed") is True, f"del: {deleted.json}")
    ctx.expect(ctx.luau("return workspace.Props:FindFirstChild('Sign') == nil") is True, "Sign removed in Studio")
    ctx.expect(len(_found(ctx, "Workspace", "-n", "Sign")) == 1, "store keeps Sign before the pull")
    ctx.rbx("pl", timeout=180)
    ctx.expect(len(_found(ctx, "Workspace", "-n", "Sign")) == 0, "store drops Sign after the pull")
    _documented_fields(ctx, deleted, "del")


def del_refuses_a_service_root(ctx: Context):
    """DATA-013: del refuses to delete a service root, by name or by an empty path."""
    ctx.open_studio()
    for path in ('["Workspace"]', "[]"):
        refused = ctx.rbx("del", "-s", "Workspace", "-p", path, ok=False, timeout=60)
        ctx.expect_in("Refusing to delete a service root", refused.error, f"del -p {path}")
    ctx.expect(ctx.luau("return workspace:FindFirstChild('Props') ~= nil") is True, "Workspace untouched")


def del_one_duplicate(ctx: Context):
    """DATA-014: del with ordinals removes the second Crate and keeps the first."""
    ctx.open_studio()
    _mark_first_crate(ctx)
    deleted = ctx.rbx("del", *CRATE_2, timeout=60)
    ctx.expect(deleted.get("changed") is True, f"del: {deleted.json}")
    left = ctx.luau("local t = {} for _, c in workspace.Props:GetChildren() do if c.Name == 'Crate' then t[#t + 1] = c:GetAttribute('First') == true end end return #t == 1 and t[1]")
    ctx.expect(left is True, "only the first Crate is left")


def del_under_live_sync(ctx: Context):
    """DATA-015 (D6): del under Live Sync pulls the removal into the store with a sync history entry."""
    _live(ctx)
    ctx.rbx("del", *SIGN, timeout=60)
    _settled(ctx)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline and _found(ctx, "Workspace", "-n", "Sign"):
        ctx.rbx("lst", "--wait", "10", ok=None, timeout=60)
    ctx.expect(not _found(ctx, "Workspace", "-n", "Sign"), "Live Sync settled without pulling the del into the store (D6)")
    saved = ctx.rbx("rev", "--path", WORKSPACE, "--print", json_mode=False, ok=None, timeout=60)
    ctx.expect(saved.code == 0 and "Sign" in saved.stdout, f"sync history keeps the store before the pull: exit {saved.code} {saved.stdout[:200]} {saved.stderr[:200]}")


def _main_source(ctx: Context, source: str):
    ctx.luau(f"game.ServerScriptService.Main.Source = {json.dumps(source)} return true")


def me_replaces_text(ctx: Context):
    """DATA-016: me on an instance path replaces the first match, -a every match, in Studio only."""
    ctx.open_studio()
    _main_source(ctx, "print(1) a a")
    file = ctx.read(MAIN)
    edited = ctx.rbx("me", "ServerScriptService.Main", "print(1)", "print(2)", timeout=60)
    ctx.expect(edited.get("edits") == 1 and edited.get("path") == "ServerScriptService.Main" and edited.get("persistence") == "unsaved", f"me: {edited.json}")
    ctx.rbx("me", "ServerScriptService.Main", "a", "b", "-a", timeout=60)
    ctx.expect(ctx.luau("return game.ServerScriptService.Main.Source") == "print(2) b b", "Source after me -a")
    ctx.expect(ctx.read(MAIN) == file, "file unchanged")


def me_failure_rolls_back(ctx: Context):
    """DATA-017: a missing OLD string fails the whole me and leaves Source alone; OLD needs a NEW."""
    ctx.open_studio()
    _main_source(ctx, "print(1) a a")
    missing = ctx.rbx("me", "ServerScriptService.Main", "print(1)", "x", "absent", "y", ok=False, timeout=60)
    ctx.expect_in("old_string was not found", missing.error, "missing OLD")
    ctx.expect(ctx.luau("return game.ServerScriptService.Main.Source") == "print(1) a a", "Source unchanged")
    odd = ctx.rbx("me", "ServerScriptService.Main", "a", "b", "c", ok=False, timeout=60)
    ctx.expect_in("Each OLD value needs a following NEW value", odd.error, "odd pairs")


def me_file_form_edits_file_and_studio(ctx: Context):
    """DATA-018: me FILE OLD NEW (the guide form) edits the file and the Studio script it maps to."""
    ctx.open_studio()
    edited = ctx.rbx("me", MAIN, 'print("main")', 'print("edited")', timeout=60)
    ctx.expect(edited.get("edits") == 1 and edited.get("path") == "ServerScriptService.Main", f"me FILE: {edited.json}")
    ctx.expect_in('print("edited")', ctx.read(MAIN), "file updated")
    ctx.expect_in('print("edited")', ctx.luau("return game.ServerScriptService.Main.Source"), "Studio updated")
    missing = ctx.rbx("me", MAIN, "nowhere", "x", ok=False, timeout=60)
    ctx.expect(missing.error, "a missing OLD string fails")


def me_creates_scripts(ctx: Context):
    """DATA-019: me with -c creates a script; without -c a new path is refused; an ambiguous segment asks for [n]."""
    ctx.open_studio()
    created = ctx.rbx("me", "ServerScriptService.Lib", "", "return 1", "-c", "ModuleScript", timeout=60)
    ctx.expect(created.get("edits") == 1 and created.get("className") == "ModuleScript", f"me -c: {created.json}")
    ctx.expect(ctx.luau("return game.ServerScriptService.Lib.Source") == "return 1", "Lib source")
    plain = ctx.rbx("me", "ServerScriptService.Lib2", "", "return 2", ok=False, timeout=60)
    ctx.expect_in("className must be Script, LocalScript, or ModuleScript", plain.error, "no -c")
    ambiguous = ctx.rbx("me", "Workspace.Props.Crate.X", "", "return 1", "-c", "Script", ok=False, timeout=60)
    ctx.expect_in("Path segment 'Crate' matched 2 children", ambiguous.error, "ambiguous segment")
    ctx.expect_in("[n]", ambiguous.error, "ordinal hint")


def me_under_live_sync(ctx: Context):
    """DATA-020: me on an instance path under Live Sync reaches the file."""
    _live(ctx)
    ctx.rbx("me", "ServerScriptService.Main", 'print("main")', 'print(2)', timeout=60)
    deadline = time.monotonic() + 40
    while time.monotonic() < deadline and "print(2)" not in ctx.read(MAIN):
        ctx.rbx("lst", "--wait", "10", ok=None, timeout=60)
    ctx.expect_in("print(2)", ctx.read(MAIN), "file after Live Sync")


# ---------------------------------------------------------------- K. script search and structural edits


def script_search_reads_files(ctx: Context):
    """DATA-021: ss/sg/sr read the files, not Studio: a Studio edit shows up only after a pull or Live Sync."""
    found = ctx.rbx("ss", "main", timeout=60)
    ctx.expect(found.get("totalFiles") == 1 and found.pointer("/results/0/path") == MAIN, f"ss: {found.json}")
    ctx.expect(ctx.rbx("sg", "print", timeout=60).get("totalMatches") == 2, "sg print")
    ctx.expect(ctx.rbx("sg", "-i", "PRINT", timeout=60).get("totalMatches") == 2, "sg -i")
    ctx.expect(ctx.rbx("sr", "src/Nowhere.luau", ok=False, timeout=60).error, "sr on a missing file fails")
    ctx.open_studio()
    _main_source(ctx, "-- marker42")
    ctx.expect(ctx.rbx("sg", "marker42", timeout=60).get("totalMatches") == 0, "sg before the pull")
    ctx.rbx("pl", timeout=180)
    hit = ctx.rbx("sg", "marker42", timeout=60)
    ctx.expect(hit.get("totalMatches") == 1 and hit.pointer("/results/0/hits/0/line") == 1, f"sg after the pull: {hit.json}")
    ctx.expect(ctx.rbx("ss", "marker42", timeout=60).pointer("/results/0/path") == MAIN, "ss after the pull")
    read = ctx.rbx("sr", MAIN, "--end-line", "1", timeout=60)
    ctx.expect(read.get("source") == "-- marker42", f"sr: {read.json}")
    ctx.rbx("lon", "--wait", "60", timeout=200)
    _main_source(ctx, "-- marker43")
    deadline = time.monotonic() + 40
    while time.monotonic() < deadline and ctx.rbx("sg", "marker43", timeout=60).get("totalMatches") != 1:
        ctx.rbx("lst", "--wait", "10", ok=None, timeout=60)
    ctx.expect(ctx.rbx("sg", "marker43", timeout=60).get("totalMatches") == 1, "sg under Live Sync")


def store_edits_then_push(ctx: Context):
    """DATA-022: cr, cp, rn, mv and rm edit the stores and one push applies all of them."""
    ctx.open_studio()
    made = ctx.rbx("cr", "Workspace", "-c", "Folder", "-n", "Zone", timeout=60)
    zone = made.get("settingsId") or made.pointer("/created/0/settingsId")
    ctx.expect(zone, f"cr: {made.json}")
    ctx.expect(ctx.rbx("cp", "Workspace", "-i", "editor:4", "-I", zone, timeout=60).ok, "cp")
    ctx.expect(ctx.rbx("rn", "Workspace", "Label", "-i", zone, timeout=60).ok, "rn")
    ctx.expect(ctx.rbx("mv", "Workspace", "-i", "editor:2", "-I", zone, timeout=60).ok, "mv")
    ctx.expect(ctx.rbx("rm", "Workspace", "-i", "editor:3", timeout=60).ok, "rm")
    ctx.rbx("ps", WORKSPACE, "--yes", timeout=120)
    ctx.expect(ctx.luau("return workspace.Label:FindFirstChild('Sign') ~= nil and workspace.Label:FindFirstChild('Crate') ~= nil") is True, "copy and move applied")
    props = ctx.luau("local t = {} for _, c in workspace.Props:GetChildren() do t[#t + 1] = c.Name .. ':' .. c.ClassName end return table.concat(t, ' ')")
    ctx.expect(ctx.luau("return #workspace.Props:GetChildren()") == 1, f"one Crate removed, Sign original kept; Props holds [{props}]")


def create_and_targeted_push(ctx: Context):
    """DATA-023: cr adds a Part under Props in the store and a push of its id creates it in Studio (a push that
    writes no project file records no historyId)."""
    ctx.open_studio()
    made = ctx.rbx("cr", "Workspace", "-c", "Part", "-n", "New", "-I", "editor:1", timeout=60)
    new = made.get("settingsId")
    ctx.expect(new and made.get("changedPaths"), f"cr: {made.json}")
    pushed = ctx.rbx("ps", WORKSPACE, "-i", new, "--yes", timeout=120)
    ctx.expect(pushed.pointer("/instances/created") == 1 and pushed.get("verified") is True, f"push: {pushed.json}")
    ctx.expect(ctx.luau("local n = workspace.Props:FindFirstChild('New') return n and n.ClassName") == "Part", "Props.New is a Part")


def create_script_edit_push(ctx: Context):
    """DATA-024: cr of a ModuleScript writes its source file; an edit of the file and a push reach Studio."""
    ctx.open_studio()
    made = ctx.rbx("cr", "ServerScriptService", "-c", "ModuleScript", "-n", "Lib", timeout=60)
    source = ctx.root / "src" / "ServerScriptService" / "Lib.luau"
    ctx.expect(source.is_file(), f"cr wrote no source file: {made.json}")
    ctx.write("src/ServerScriptService/Lib.luau", "return 5\n")
    ctx.rbx("ps", "instances/ServerScriptService.renium", "src/ServerScriptService/Lib.luau", "--yes", timeout=120)
    ctx.expect(ctx.luau("return require(game.ServerScriptService.Lib)") == 5, "require(Lib)")


def clone_and_push(ctx: Context):
    """DATA-025: cp of the first Crate under Props pushes a third Part with a unique name."""
    ctx.open_studio()
    copied = ctx.rbx("cp", "Workspace", "-i", "Props.Crate[1]", "-I", "Props", timeout=60)
    new = copied.get("rootSettingsId")
    ctx.expect(new, f"cp: {copied.json}")
    ctx.rbx("ps", WORKSPACE, "-i", new, "--yes", timeout=120)
    names = ctx.luau("local t = {} for _, c in workspace.Props:GetChildren() do if c:IsA('Part') then t[#t + 1] = c.Name end end table.sort(t) return table.concat(t, ',')")
    parts = str(names).split(",")
    ctx.expect(len(parts) == 3 and len(set(parts) - {"Crate"}) == 1, f"parts under Props: {names}")


def move_across_services_and_push(ctx: Context):
    """DATA-026: mv of Sign into ReplicatedStorage and a push of both stores moves it in Studio with its Value."""
    ctx.open_studio()
    value = _sign(ctx)
    ctx.rbx("mv", "Workspace", "-i", "Props.Sign", "--to-service", "ReplicatedStorage", timeout=60)
    ctx.rbx("ps", WORKSPACE, RS_STORE, "--yes", timeout=120)
    ctx.expect(ctx.luau("local s = game.ReplicatedStorage:FindFirstChild('Sign') return s and s.Value") == value, "ReplicatedStorage.Sign keeps its Value")
    ctx.expect(ctx.luau("return workspace.Props:FindFirstChild('Sign') == nil") is True, "Props.Sign gone")


def move_a_script_and_push(ctx: Context):
    """DATA-027: mv of a ModuleScript across services moves its file and a push moves it in Studio."""
    ctx.open_studio()
    source = ctx.luau("return game.ReplicatedStorage.Shared.Util.Source")
    ctx.rbx("mv", "ReplicatedStorage", "-i", "Shared.Util", "--to-service", "ServerScriptService", timeout=60)
    ctx.expect((ctx.root / "src" / "ServerScriptService" / "Util.luau").is_file() and not (ctx.root / "src" / "ReplicatedStorage" / "Shared" / "Util.luau").exists(), "Util.luau moved")
    ctx.rbx("ps", RS_STORE, "instances/ServerScriptService.renium", "src", "--yes", timeout=180)
    ctx.expect(ctx.luau("local u = game.ServerScriptService:FindFirstChild('Util') return u and u.Source") == source, "ServerScriptService.Util source")
    ctx.expect(ctx.luau("return game.ReplicatedStorage.Shared:FindFirstChild('Util') == nil") is True, "old Util gone")


def rename_and_push(ctx: Context):
    """DATA-028: rn of the second Crate and a push rename it in Studio."""
    ctx.open_studio()
    ctx.rbx("rn", "Workspace", "Crate2", "-i", "Props.Crate[2]", timeout=60)
    ctx.rbx("ps", WORKSPACE, "--yes", timeout=120)
    names = ctx.luau("local t = {} for _, c in workspace.Props:GetChildren() do if c:IsA('Part') then t[#t + 1] = c.Name end end table.sort(t) return table.concat(t, ',')")
    ctx.expect(names == "Crate,Crate2", f"names: {names}")


def rename_a_script_and_push(ctx: Context):
    """DATA-029: rn of a LocalScript renames its file and a push renames it in Studio."""
    ctx.open_studio()
    ctx.rbx("rn", "StarterPlayer", "Client2", "-i", "StarterPlayerScripts.Client", timeout=60)
    folder = ctx.root / "src" / "StarterPlayer" / "StarterPlayerScripts"
    ctx.expect((folder / "Client2.client.luau").is_file() and not (folder / "Client.client.luau").exists(), f"files: {sorted(p.name for p in folder.iterdir())}")
    ctx.rbx("ps", "instances/StarterPlayer.renium", "src", "--yes", timeout=180)
    ctx.expect(ctx.luau("local s = game.StarterPlayer.StarterPlayerScripts return s:FindFirstChild('Client2') ~= nil and s:FindFirstChild('Client') == nil") is True, "Studio has Client2 only")


def remove_and_push(ctx: Context):
    """DATA-030: rm of Sign then a push with -u keeps it in Studio; a plain push removes it."""
    ctx.open_studio()
    ctx.rbx("rm", "Workspace", "-i", "Props.Sign", timeout=60)
    ctx.rbx("ps", WORKSPACE, "-u", "--yes", timeout=120)
    ctx.expect(_sign(ctx) is not None, "push -u kept Sign")
    ctx.rbx("ps", WORKSPACE, "--yes", timeout=120)
    ctx.expect(ctx.luau("return workspace.Props:FindFirstChild('Sign') == nil") is True, "plain push removed Sign")


def remove_keeping_descendants(ctx: Context):
    """DATA-031: rm -R of Props keeps its children under Workspace after a push."""
    ctx.open_studio()
    ctx.rbx("rm", "Workspace", "-i", "Props", "-R", timeout=60)
    ctx.rbx("ps", WORKSPACE, "--yes", timeout=120)
    state = ctx.luau("local n = 0 for _, c in workspace:GetChildren() do if c.Name == 'Crate' then n += 1 end end return n .. ',' .. tostring(workspace:FindFirstChild('Sign') ~= nil) .. ',' .. tostring(workspace:FindFirstChild('Props') ~= nil)")
    ctx.expect(state == "2,true,false", f"crates, sign, props: {state}")


def structural_edit_under_live_sync(ctx: Context):
    """DATA-032: a cr under Live Sync is pushed by the worker."""
    _live(ctx)
    ctx.rbx("cr", "Workspace", "-c", "Part", "-n", "New", "-I", "editor:1", timeout=60)
    deadline = time.monotonic() + 40
    while time.monotonic() < deadline and ctx.luau("return workspace.Props:FindFirstChild('New') ~= nil") is not True:
        ctx.rbx("lst", "--wait", "10", ok=None, timeout=60)
    ctx.expect(ctx.luau("return workspace.Props:FindFirstChild('New') ~= nil") is True, "worker pushed New")
    _settled(ctx)


def create_inside_a_read_only_link(ctx: Context):
    """DATA-033: cr inside a read-only link copy is refused naming the link; --override-packages writes the store."""
    env = _link_copy(ctx)
    store = ctx.root / RS_STORE
    before = store.read_bytes()
    refused = _run(ctx, "cr", "ReplicatedStorage", "-c", "Folder", "-n", "X", "-I", "SharedCopy", env=env, ok=False, timeout=60)
    ctx.expect_in('read-only link "shared"', refused.error, "cr refusal")
    ctx.expect(store.read_bytes() == before, "store unchanged")
    _run(ctx, "cr", "ReplicatedStorage", "-c", "Folder", "-n", "X", "-I", "SharedCopy", "--override-packages", env=env, timeout=60)
    ctx.expect(store.read_bytes() != before, "override wrote the store")


# ---------------------------------------------------------------- L. model round trips and bpack


def model_round_trip_through_studio(ctx: Context):
    """DATA-034: mep exports Shared, v lists it, mip imports it under Props and a push creates it in Studio with the
    same Util source; bem --format rbxmx writes XML that bim imports; a pull keeps both copies."""
    ctx.open_studio()
    original = ctx.luau("return game.ReplicatedStorage.Shared.Util.Source")
    ctx.rbx("mep", "ReplicatedStorage", "-i", "Shared", "-o", "s.rbxm", timeout=60)
    viewed = ctx.rbx("v", "s.rbxm", "--json", timeout=60)
    ctx.expect("Shared" in viewed.stdout and "Util" in viewed.stdout, f"v: {viewed.stdout[:300]}")
    imported = ctx.rbx("mip", "Workspace", "-I", "editor:1", "-m", "s.rbxm", timeout=60)
    root = imported.get("settingsId") or imported.pointer("/created/0/settingsId") or imported.pointer("/roots/0")
    ctx.expect(imported.ok, f"mip: {imported.json}")
    push = ["ps", WORKSPACE, "src", "--yes"] + (["-i", str(root)] if root else [])
    ctx.rbx(*push, timeout=180)
    ctx.expect(ctx.luau("local s = workspace.Props:FindFirstChild('Shared') return s and s.Util.Source") == original, "Studio copy's Util source")
    exported = ctx.rbx("bem", "-f", RS_STORE, "Shared", "-o", "s.rbxmx", "--format", "rbxmx", timeout=60)
    ctx.expect(exported.ok and ctx.read("s.rbxmx").lstrip().startswith("<roblox"), "rbxmx is XML")
    ctx.rbx("bim", "ServerStorage", "--model", "s.rbxmx", "--no-parent", timeout=60)
    ctx.rbx("ps", "instances/ServerStorage.renium", "src", "--yes", timeout=180)
    ctx.expect(ctx.luau("local s = game.ServerStorage:FindFirstChild('Shared') return s and s.Util.Source") == original, "rbxmx copy in Studio")
    ctx.rbx("pl", timeout=180)
    ctx.expect(len(_found(ctx, "Workspace", "-n", "Util")) == 1 and len(_found(ctx, "ServerStorage", "-n", "Util")) == 1, "pull keeps both copies")


def studio_model_to_files_and_back(ctx: Context):
    """DATA-035: a Studio-made model pulled, exported with mep, imported with bim and pushed matches the original."""
    ctx.open_studio()
    ctx.luau('local m = Instance.new("Model") m.Name = "Built" local p = Instance.new("Part") p.Size = Vector3.new(4, 1, 2) p.Parent = m local s = Instance.new("Script") s.Source = "return 7" s.Parent = m m.Parent = workspace return true')
    ctx.rbx("pl", timeout=180)
    ctx.rbx("mep", "Workspace", "-i", "Built", "-o", "b.rbxm", timeout=60)
    ctx.rbx("bim", "ServerStorage", "--model", "b.rbxm", "--no-parent", timeout=60)
    ctx.rbx("ps", "instances/ServerStorage.renium", "src", "--yes", timeout=180)
    same = ctx.luau("local a, b = workspace.Built, game.ServerStorage:FindFirstChild('Built') return b ~= nil and a.Part.Size == b.Part.Size and a.Script.Source == b.Script.Source")
    ctx.expect(same is True, "ServerStorage.Built matches Workspace.Built")


def import_under_live_sync(ctx: Context):
    """DATA-037: mip under Live Sync is pushed by the worker."""
    _live(ctx)
    ctx.rbx("mep", "ReplicatedStorage", "-i", "Shared", "-o", "s.rbxm", timeout=60)
    ctx.rbx("mip", "Workspace", "-I", "editor:1", "-m", "s.rbxm", timeout=60)
    ctx.rbx("lst", "--wait", "60", ok=None, timeout=200)
    deadline = time.monotonic() + 40
    while time.monotonic() < deadline and ctx.luau("return workspace.Props:FindFirstChild('Shared') ~= nil") is not True:
        ctx.rbx("lst", "--wait", "10", ok=None, timeout=60)
    ctx.expect(ctx.luau("return workspace.Props:FindFirstChild('Shared') ~= nil") is True, "worker pushed the import")
    _settled(ctx)


def bpack_on_current_stores(ctx: Context):
    """DATA-038: bpack on current stores changes nothing."""
    packed = ctx.rbx("bpack", timeout=120)
    ctx.expect(packed.get("fileCount", 0) > 0 and packed.get("changedFiles") == 0 and packed.get("changedPaths") == [], f"bpack: {str(packed.json)[:300]}")


def bpack_under_live_sync(ctx: Context):
    """DATA-039: bpack under a settled Live Sync pushes nothing and leaves Studio alone."""
    _live(ctx)
    pushes = _settled(ctx).pointer("/daemon/pushes", 0)
    value = _sign(ctx)
    ctx.rbx("bpack", timeout=120)
    after = _settled(ctx)
    ctx.expect(after.pointer("/daemon/pushes", 0) == pushes, f"pushes {pushes} -> {after.pointer('/daemon/pushes')}")
    ctx.expect(_sign(ctx) == value, "Studio unchanged")


# ---------------------------------------------------------------- M. Studio review of direct mutations


def _luau_value(value) -> str:
    if value is None or (isinstance(value, dict) and value.get("_type") == "Nil"):
        return "nil"
    return json.dumps(value)


def _restore_settings(ctx: Context, restore: str):
    """Puts the stored plugin settings back, retrying while Studio is busy and reopening it when it is gone."""
    deadline = time.monotonic() + 120
    answer = None
    while time.monotonic() < deadline:
        answer = ctx.rbx("l", restore, ok=None, timeout=60)
        if answer.ok:
            return
        if "busy" not in answer.error:
            ctx.rbx("ro", ok=None, timeout=200)
            ctx.studio_open = True
        time.sleep(2)
    raise _failure(ctx, f"could not restore the plugin settings (Renium_changesThreshold may still be 0): {answer.error[:300] if answer else ''}")


@contextmanager
def _review_threshold_zero(ctx: Context, **settings):
    """H4: the project's liveSync.changesThreshold at 0 holds every reviewed edit (the daemon forwards it, and it
    beats the Studio's own setting), so a row needs no Studio restart. Extra Renium_ plugin settings (displayPrompts)
    still go through the plugin for one Studio restart, as the plugin reads them when it starts."""
    ctx.rbx("cfg", "set", "liveSync.changesThreshold", "0", timeout=60)
    ctx.open_studio()
    restore = None
    if settings:
        names = {f"Renium_{key}": value for key, value in settings.items()}
        previous = {name: ctx.luau(f"return plugin:GetSetting({json.dumps(name)})") for name in names}
        restore = " ".join(f"plugin:SetSetting({json.dumps(name)}, {_luau_value(value)})" for name, value in previous.items()) + " return true"
        try:
            ctx.luau(" ".join(f"plugin:SetSetting({json.dumps(name)}, {_luau_value(value)})" for name, value in names.items()) + " return true")
            ctx.close_studio(save=True)
            ctx.reopen_studio()
        finally:
            _restore_settings(ctx, restore)
    try:
        yield
    finally:
        ctx.rbx("cfg", "set", "liveSync.changesThreshold", "5", ok=None, timeout=60)
        if settings:
            ctx.close_studio()


def _detached(ctx: Context, name: str, *args: str, env: dict | None = None) -> tuple[subprocess.Popen, Path, Path]:
    out, err = ctx.root / f"{name}.out", ctx.root / f"{name}.err"
    process = subprocess.Popen([RBX, "--output-mode", "json"] + list(args), cwd=ctx.root, env=env or _env(ctx), stdin=subprocess.DEVNULL, stdout=out.open("w", encoding="utf-8"), stderr=err.open("w", encoding="utf-8"))
    return process, out, err


def _finished(ctx: Context, process: subprocess.Popen, out: Path, err: Path, timeout: float = 150) -> dict:
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        process.kill()
        raise _failure(ctx, f"{process.args} did not finish within {timeout}s")
    stdout, stderr = out.read_text(encoding="utf-8", errors="replace"), err.read_text(encoding="utf-8", errors="replace")
    ctx.log.append(f"$ (detached) {' '.join(process.args[3:])}  [exit {process.returncode}]\n{stdout.strip()[:1200]}\n{stderr.strip()[:600]}")
    lines = [line for line in stdout.strip().splitlines() + stderr.strip().splitlines() if line.strip().startswith("{")]
    return {"code": process.returncode, "json": json.loads(lines[-1]) if lines else None, "stdout": stdout, "stderr": stderr}


def _await_review(ctx: Context, decision: str, seconds: float) -> bool:
    """H13: decides the held review from a second process; False when none appeared in time."""
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        answer = ctx.rbx("rv", decision, ok=None, timeout=60)
        if answer.ok:
            return True
        ctx.expect("awaiting a decision" in answer.error, f"rv {decision}: {answer.error[:300]}")
        time.sleep(0.5)
    return False


def _expect_held(ctx: Context, answer, what: str):
    """The result of an edit over the threshold: nothing applied, the change list and the next step."""
    ctx.expect(answer.get("heldForReview") is True and answer.get("changed") is False, f"{what} over the threshold: {answer.json}")
    ctx.expect(isinstance(answer.get("review"), list) and len(answer.get("review")) >= 1, f"{what} carries its change list: {answer.json}")
    ctx.expect("--yes" in str(answer.get("next")), f"{what} names the next step: {answer.json}")


def held_del_applied_by_rv(ctx: Context):
    """DATA-040, DATA-048: a del over the threshold applies nothing and answers heldForReview with its change list;
    the same del with --yes removes the instance."""
    with _review_threshold_zero(ctx):
        held = ctx.rbx("del", *SIGN, timeout=60)
        _expect_held(ctx, held, "del")
        ctx.expect(_sign(ctx) is not None, "Sign kept while the del is held")
        applied = ctx.rbx("del", *SIGN, "--yes", timeout=60)
        ctx.expect(applied.get("changed") is True, f"del --yes: {applied.json}")
        ctx.expect(ctx.luau("return workspace.Props:FindFirstChild('Sign') == nil") is True, "Sign gone")


def held_del_skipped_by_rv(ctx: Context):
    """DATA-041: a held del names the instance it would remove and leaves no review open in Studio."""
    with _review_threshold_zero(ctx):
        held = ctx.rbx("del", *SIGN, timeout=60)
        _expect_held(ctx, held, "del")
        ctx.expect("Sign" in json.dumps(held.get("review")), f"the change list names Sign: {held.json}")
        ctx.expect(held.get("reviewThreshold") == 0, f"the result carries the threshold: {held.json}")
        answer = ctx.rbx("rv", "skip", ok=False, timeout=60)
        ctx.expect_in("No review is awaiting a decision", answer.error, "rv after a held del")
        ctx.expect(_sign(ctx) is not None, "Sign kept")


def unanswered_review(ctx: Context):
    """DATA-042: a held del returns at once instead of waiting for a decision nobody gives."""
    with _review_threshold_zero(ctx):
        started = time.monotonic()
        held = ctx.rbx("del", *SIGN, timeout=60)
        waited = time.monotonic() - started
        _expect_held(ctx, held, "del")
        ctx.expect(waited < 15, f"del returned after {waited:.0f}s")
        ctx.expect(_sign(ctx) is not None, "Sign kept")


def prop_review_above_threshold(ctx: Context):
    """DATA-043, DATA-044, LIFE-075 (yes part): prop --yes and cfg yes:true apply at once; prop without --yes above
    the threshold applies nothing and answers heldForReview."""
    with _review_threshold_zero(ctx):
        approved = ctx.rbx("prop", *SIGN, "-n", "Value", "-j", '"yes"', "--yes", timeout=60)
        ctx.expect(approved.get("changed") is True and _sign(ctx) == "yes", f"prop --yes: {approved.json}")
        ctx.write(".renium/config.json", '{"schemaVersion":1,"yes":true}\n')
        configured = ctx.rbx("prop", *SIGN, "-n", "Value", "-j", '"cfg"', timeout=60)
        ctx.expect(configured.get("changed") is True and _sign(ctx) == "cfg", f"prop with cfg yes: {configured.json}")
        (ctx.root / ".renium" / "config.json").unlink()
        ctx.rbx("cfg", "set", "liveSync.changesThreshold", "0", timeout=60)
        held = ctx.rbx("prop", *SIGN, "-n", "Value", "-j", '"held"', timeout=60)
        _expect_held(ctx, held, "prop")
        ctx.expect(_sign(ctx) == "cfg", "Value unchanged while the prop is held")


def del_yes_skips_the_review(ctx: Context):
    """DATA-045 (D3): `rbx --yes del` and cfg yes:true apply a del without a review."""
    with _review_threshold_zero(ctx):
        process, out, err = _detached(ctx, "del-yes", "--yes", "del", *SIGN)
        held = _await_review(ctx, "apply", 15)
        result = _finished(ctx, process, out, err)
        ctx.expect(not held, f"rbx --yes del still opened a review (D3): {result['json']}")
        ctx.expect((result["json"] or {}).get("changed") is True, f"--yes del: {result['json']}")


def review_with_nothing_held(ctx: Context):
    """DATA-046: rv with no held review says so."""
    ctx.open_studio()
    for args in (("rv", "apply"), ("rv", "skip", "-i", "bogus")):
        answer = ctx.rbx(*args, ok=False, timeout=60)
        ctx.expect_in("No review is awaiting a decision", answer.error, " ".join(args))


def display_prompts_never(ctx: Context):
    """DATA-047: with Renium_displayPrompts "never" a del above the threshold applies without a hold."""
    with _review_threshold_zero(ctx, displayPrompts="never"):
        started = time.monotonic()
        deleted = ctx.rbx("del", *SIGN, timeout=120)
        ctx.expect(deleted.get("changed") is True and time.monotonic() - started < 30, f"del: {deleted.json}")
        ctx.expect(ctx.luau("return workspace.Props:FindFirstChild('Sign') == nil") is True, "Sign gone")


def saved_field_review_at_threshold_zero(ctx: Context):
    """DATA-051: prop of a field without a plugin API and without --yes at threshold 0 applies nothing and answers
    heldForReview; with --yes the native writer sets it."""
    with _review_threshold_zero(ctx):
        current, value = _technology(ctx)
        held = ctx.rbx("prop", *LIGHTING, "-j", value, timeout=120)
        _expect_held(ctx, held, "saved-field prop")
        ctx.expect(_pulled_technology(ctx) == current, "Technology unchanged while the prop is held")
        applied = ctx.rbx("prop", *LIGHTING, "-j", value, "--yes", ok=None, timeout=180)
        if sys.platform == "darwin" and not applied.ok:
            ctx.expect("native property writer could not set it" in applied.error + json.dumps(applied.json), f"macOS saved-field refusal: {applied.error[:300]}")
            return
        ctx.expect(applied.get("changed") is True and applied.get("savedFieldsApplied") == 1, f"saved-field prop --yes: {applied.json}")
        ctx.expect(_pulled_technology(ctx) == json.loads(value)["name"], "Technology written with --yes")


def saved_field_on_duplicate_sibling(ctx: Context):
    """DATA-052: prop --yes of a saved field on the second of two same-named siblings under a same-named ancestor
    writes that sibling through the native writer with the ordinals the command carries. MeshPart.MeshId is saved
    and readable but not writable from plugin Luau."""
    ctx.open_studio()
    ctx.luau("for _ = 1, 2 do local set = Instance.new('Folder') set.Name = 'Set' set.Parent = workspace for _ = 1, 2 do local mesh = Instance.new('MeshPart') mesh.Name = 'Player' mesh.Anchored = true mesh.Parent = set end end return true")
    mesh_id = "rbxassetid://6715531731"
    target = ("-s", "Workspace", "-c", "MeshPart", "-p", '["Set","Player"]', "-o", "[2,2]", "-n", "MeshId")
    applied = ctx.rbx("prop", *target, "-j", json.dumps(mesh_id), "--yes", ok=None, timeout=300)
    ctx.studio_open = True
    ctx.expect(applied.get("changed") is True, f"saved field on a same-named sibling (ords [2,2]) did not apply: {applied.json}")
    ctx.rbx("pl", timeout=180)
    values = {f"Set[{s}].Player[{p}]": ctx.rbx("bg", "Workspace", f"Set[{s}].Player[{p}]", "MeshId", ok=None, timeout=60).json for s in (1, 2) for p in (1, 2)}
    ctx.expect(values["Set[2].Player[2]"] == mesh_id and all(value != mesh_id for path, value in values.items() if path != "Set[2].Player[2]"), f"MeshId per sibling after the pull: {values}")


def prop_hold_notice_reaches_the_caller(ctx: Context):
    """DATA-053: a held prop tells its caller in the result itself, and no transport flag reaches a parser."""
    with _review_threshold_zero(ctx):
        held = ctx.rbx("prop", *SIGN, "-n", "Value", "-j", '"noticed"', timeout=60)
        _expect_held(ctx, held, "prop")
        ctx.expect(_sign(ctx) != "noticed", "Value unchanged while the prop is held")
        ctx.expect("--notices" not in held.stdout + held.stderr, f"a transport flag reached a parser: {held.stderr[:300]!r}")


def me_has_no_review(ctx: Context):
    """DATA-049: me applies at once with the threshold at 0."""
    with _review_threshold_zero(ctx):
        started = time.monotonic()
        edited = ctx.rbx("me", "ServerScriptService.Main", 'print("main")', 'print(3)', timeout=120)
        ctx.expect(edited.get("edits") == 1 and time.monotonic() - started < 30, f"me: {edited.json}")


def no_review_during_play(ctx: Context):
    """DATA-050 (D8): during Play a del of the Edit window is refused at once and names `rbx play -x`;
    after the session stops the same del applies."""
    with _review_threshold_zero(ctx):
        ctx.rbx("play", "-s", timeout=300)
        try:
            started = time.monotonic()
            refused = ctx.rbx("del", *SIGN, "--yes", ok=False, timeout=120)
            ctx.expect("play -x" in refused.error and time.monotonic() - started < 30, f"del during Play was not refused promptly (D8): {refused.json} {refused.error[:160]} after {time.monotonic() - started:.0f}s")
            ctx.expect(ctx.luau("return workspace.Props:FindFirstChild('Sign') ~= nil") is True, "Sign kept while Play runs")
        finally:
            ctx.rbx("play", "-x", ok=None, timeout=120)
        deleted = ctx.rbx("del", *SIGN, "--yes", timeout=120)
        ctx.expect(deleted.get("changed") is True, f"del after the session stopped: {deleted.json}")
        ctx.expect(ctx.luau("return workspace.Props:FindFirstChild('Sign') == nil") is True, "Sign gone in Edit")


def first_store_keeps_a_new_script(ctx: Context):
    """DATA-054: a service without a store is pulled by the first command that needs it, and that pull keeps a
    script file Studio has no script for: `ba --push` adopts it and Studio gets its text. A file Studio does
    have a script for takes Studio's copy. `rbx ck` without git checks the whole project."""
    ctx.open_studio()
    ctx.luau("local s = Instance.new('LocalScript') s.Name = 'Known' s.Source = '-- studio' s.Parent = game.StarterGui return true")
    (ctx.root / "instances" / "StarterGui.renium").unlink()
    ctx.write("src/StarterGui/Fresh.client.luau", "print('fresh')\n")
    ctx.write("src/StarterGui/Known.client.luau", "print('local')\n")
    added = ctx.rbx("ba", "StarterGui", "-n", "Fresh", "-c", "LocalScript", "--push", timeout=120)
    ctx.expect(added.get("adoptedSource") == "src/StarterGui/Fresh.client.luau", f"ba adopts the kept file: {added.json}")
    ctx.expect_in("kept 1 script file", added.stderr, "the pull says what it kept")
    ctx.expect(ctx.read("src/StarterGui/Fresh.client.luau") == "print('fresh')\n", "the new script file keeps its text")
    ctx.expect(ctx.luau("return game.StarterGui.Fresh.Source") == "print('fresh')\n", "Studio has the new script's text")
    ctx.expect(ctx.read("src/StarterGui/Known.client.luau").strip() == "-- studio", "a script Studio has takes Studio's copy")
    checked = ctx.rbx("ck", timeout=60)
    ctx.expect(checked.ok and "checked every script" in str(checked.get("note")), f"ck without git: {checked.json}")


SCENARIOS = [
    Scenario("DATA-001", "prop --yes edits Studio only", "small", slotted(prop_yes_edits_studio_only)),
    Scenario("DATA-002", "prop without --yes applies one change", "small", slotted(prop_without_yes_applies_one_change)),
    Scenario("DATA-003", "prop by settings id", "small", slotted(prop_by_settings_id)),
    Scenario("DATA-004", "prop on a duplicate by ordinal", "small", slotted(prop_on_a_duplicate_by_ordinal)),
    Scenario("DATA-005", "prop attribute set and remove", "small", slotted(prop_attribute_set_and_remove)),
    Scenario("DATA-006", "prop Source from a file", "small", slotted(prop_source_from_a_file)),
    Scenario("DATA-007", "prop refuses Game Settings fields", "small", slotted(prop_refuses_game_settings_fields)),
    Scenario("DATA-008", "protected field without --yes", "small", slotted(protected_field_without_yes)),
    Scenario("DATA-009", "protected field with --yes", "small", slotted(protected_field_with_yes)),
    Scenario("DATA-010", "prop under Live Sync", "small", slotted(prop_under_live_sync)),
    Scenario("DATA-011", "read-only link refuses direct edits", "small", slotted(read_only_link_refuses_direct_edits)),
    Scenario("DATA-012", "del then pull", "small", slotted(del_then_pull)),
    Scenario("DATA-013", "del refuses a service root", "small", slotted(del_refuses_a_service_root)),
    Scenario("DATA-014", "del one duplicate", "small", slotted(del_one_duplicate)),
    Scenario("DATA-015", "del under Live Sync", "small", slotted(del_under_live_sync)),
    Scenario("DATA-016", "me replaces text", "small", slotted(me_replaces_text)),
    Scenario("DATA-017", "me failure rolls back", "small", slotted(me_failure_rolls_back)),
    Scenario("DATA-018", "me FILE form edits the file and Studio", "small", slotted(me_file_form_edits_file_and_studio)),
    Scenario("DATA-019", "me creates scripts", "small", slotted(me_creates_scripts)),
    Scenario("DATA-020", "me under Live Sync", "small", slotted(me_under_live_sync)),
    Scenario("DATA-021", "script search reads files", "small", slotted(script_search_reads_files)),
    Scenario("DATA-022", "store edits then one push", "small", slotted(store_edits_then_push)),
    Scenario("DATA-023", "cr and a targeted push", "small", slotted(create_and_targeted_push)),
    Scenario("DATA-024", "cr script, edit, push", "small", slotted(create_script_edit_push)),
    Scenario("DATA-025", "cp and push", "small", slotted(clone_and_push)),
    Scenario("DATA-026", "mv across services and push", "small", slotted(move_across_services_and_push)),
    Scenario("DATA-027", "mv a script and push", "small", slotted(move_a_script_and_push)),
    Scenario("DATA-028", "rn and push", "small", slotted(rename_and_push)),
    Scenario("DATA-029", "rn a script and push", "small", slotted(rename_a_script_and_push)),
    Scenario("DATA-030", "rm and push, -u keeps", "small", slotted(remove_and_push)),
    Scenario("DATA-031", "rm -R keeps descendants", "small", slotted(remove_keeping_descendants)),
    Scenario("DATA-032", "structural edit under Live Sync", "small", slotted(structural_edit_under_live_sync)),
    Scenario("DATA-033", "cr inside a read-only link", "small", slotted(create_inside_a_read_only_link)),
    Scenario("DATA-034", "model round trip through Studio", "small", slotted(model_round_trip_through_studio)),
    Scenario("DATA-035", "Studio model to files and back", "small", slotted(studio_model_to_files_and_back)),
    Scenario("DATA-037", "import under Live Sync", "small", slotted(import_under_live_sync)),
    Scenario("DATA-038", "bpack on current stores", "small", bpack_on_current_stores, needs_studio=False),
    Scenario("DATA-039", "bpack under Live Sync", "small", slotted(bpack_under_live_sync)),
    Scenario("DATA-040", "held del applies with --yes", "small", slotted(held_del_applied_by_rv)),
    Scenario("DATA-041", "held del names its change", "small", slotted(held_del_skipped_by_rv)),
    Scenario("DATA-042", "held del returns at once", "small", slotted(unanswered_review)),
    Scenario("DATA-043", "prop held above the threshold", "small", slotted(prop_review_above_threshold)),
    Scenario("DATA-045", "--yes del skips the review", "small", slotted(del_yes_skips_the_review)),
    Scenario("DATA-046", "rv with nothing held", "small", slotted(review_with_nothing_held)),
    Scenario("DATA-047", "displayPrompts never", "small", slotted(display_prompts_never), exclusive=True),
    Scenario("DATA-049", "me has no review", "small", slotted(me_has_no_review)),
    Scenario("DATA-050", "no review during Play", "small", slotted(no_review_during_play)),
    Scenario("DATA-051", "saved field held at threshold 0", "small", slotted(saved_field_review_at_threshold_zero)),
    Scenario("DATA-052", "saved field on a same-named sibling", "small", slotted(saved_field_on_duplicate_sibling)),
    Scenario("DATA-053", "held prop result reaches the caller", "small", slotted(prop_hold_notice_reaches_the_caller)),
    Scenario("DATA-054", "a first store keeps a new script", "small", slotted(first_store_keeps_a_new_script)),
]
