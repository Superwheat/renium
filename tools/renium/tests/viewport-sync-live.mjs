// Opt-in, destructive only within the existing disposable local sync fixture.
// No Play, package publishing, save-to-place, or user input/focus automation.
import assert from 'node:assert/strict';
import path from 'node:path';
import {spawnSync} from 'node:child_process';
import {performance} from 'node:perf_hooks';

const [cli, root] = process.argv.slice(2);
assert(cli && root);
const target = 'ReniumNetworkTest.rbxl';
let commands = 0;
function run(args, input) {
  const started = performance.now();
  const child = spawnSync(cli, ['--project', path.join(root, 'renium.project.jsonc'), '--place', target, ...args], {
    cwd: root, input, encoding: 'utf8', windowsHide: true, timeout: 20_000, maxBuffer: 4 * 1024 * 1024,
  });
  commands++;
  assert.ifError(child.error);
  assert.equal(child.status, 0, `${args.join(' ')}: ${child.stderr}${child.stdout}`);
  const value = JSON.parse(child.stdout);
  assert.notEqual(value.ok, false, child.stdout);
  return {value, ms: Math.round(performance.now() - started)};
}
const live = code => run(['l', '-'], code).value.results[0];
const query = ops => run(['bb', 'Workspace', '-J', '-'], JSON.stringify({ops})).value.rs;
function reopen() {
  run(['sx', '--terminate']);
  run(['ro', path.join(root, target)]);
  assert.deepEqual(run(['l', '--wait-seconds', '18', 'return {game.PlaceId,game:GetService("RunService"):IsEdit(),workspace:FindFirstChild("ReniumScaleFixture")~=nil}']).value.results[0], [0, true, true]);
}
function pull() { run(['pl', '-r', root]); }
const status = run(['status']).value;
assert.equal(status.playState, 'stopped');
assert.equal(status.clients.length, 1);
assert.equal(status.clients[0].placeId, 0);
assert.equal(status.clients[0].placeName, target);
run(['lof']);
let expectedCameraCount;
for (let cycle = 0; cycle < 3; cycle++) {
  reopen();
  const nested = cycle % 2 === 1;
  const before = live(`
local c = assert(workspace.CurrentCamera)
local n = 0
for _,v in workspace:GetChildren() do if v:IsA("Camera") then n += 1 end end
assert(n >= 1, "Fresh file has no viewport")
${nested ? 'local h=Instance.new("Folder"); h.Name="ReniumViewportHolder"; h.Parent=workspace; c.Parent=h' : ''}
c.Name="Camera"
local child=Instance.new("StringValue"); child.Name="ReniumViewportChild"; child.Value="before"; child.Parent=c
for i,name in {"Camera", "CurrentCamera"} do local v=Instance.new("Camera"); v.Name=name; v.FieldOfView=40+i; v.Parent=workspace end
local p=Instance.new("ObjectValue"); p.Name="ReniumViewportPointer"; p.Value=c; p.Parent=game.ServerStorage
return {c:GetDebugId(), c.FieldOfView, n}
`);
  pull();
  const viewportPath = nested ? ['Workspace', 'ReniumViewportHolder', 'Camera'] : ['Workspace', 'Camera'];
  const [service, child, cameras] = query([
    {type: 'instance', path: ['Workspace'], fields: 'lookup,prop:CurrentCamera'},
    {type: 'instance', path: [...viewportPath, 'ReniumViewportChild'], fields: 'lookup,prop:Value'},
    {type: 'search', q: 'Camera', limit: 10, fields: 'lookup,prop:FieldOfView'},
  ]);
  const viewportId = service.props?.CurrentCamera?.settingsId;
  assert(viewportId, 'Pull must persist a stable viewport reference, not just a live debug path');
  const extra = cameras.ns.filter(v => v.c === 'Camera' && v.id !== viewportId);
  // macOS can add a second camera before the plugin even starts (captured by
  // the startup trace). Treat that observed object as ordinary syncable data;
  // assert that Renium adds none of its own instead of deleting by name.
  assert.equal(extra.length, before[2] + 1);
  for (const camera of extra) run(['bs', 'Workspace', '-i', camera.id, '-p', 'FieldOfView', '--num', '85']);
  run(['bs', 'Workspace', '-i', viewportId, '-p', 'FieldOfView', '--num', '25']);
  run(['bs', 'Workspace', '-i', child.id, '-p', 'Value', '--str', 'after']);
  const push = run(['ps', '--verify']);
  const verify = () => live(`
local c=workspace.CurrentCamera
assert(c:GetDebugId()==${JSON.stringify(before[0])} and c.FieldOfView==${before[1]}, "Viewport identity/properties overwritten")
assert(c.ReniumViewportChild.Value=="after", "Viewport child failed to sync")
assert(game.ServerStorage.ReniumViewportPointer.Value==c, "External reference lost")
local count=0
for _,v in workspace:GetDescendants() do if v:IsA("Camera") then count+=1; if v~=c then assert(math.abs(v.FieldOfView-85)<0.0001) end end end
assert(count==${before[2] + 2}, "Camera created/deleted unexpectedly")
return {count,c:GetDebugId()}
`);
  verify();
  // An explicitly selected whole store exercises native service replacement.
  const native = run(['ps', 'instances/Workspace.renium', '--verify', '--yes']);
  verify();
  if (cycle === 2) {
    run(['lon']);
    live('workspace.CurrentCamera.ReniumViewportChild.Value="live"; return true');
    const pulled = run(['lst', '--wait', '10']);
    assert.equal(pulled.value.daemon.settled, true, JSON.stringify(pulled));
    const [syncedChild] = query([{type: 'instance', path: [...viewportPath, 'ReniumViewportChild'], fields: 'lookup,prop:Value'}]);
    assert.equal(syncedChild.props.Value, 'live');
    run(['bs', 'Workspace', '-i', syncedChild.id, '-p', 'Value', '--str', 'after']);
    const pushed = run(['lst', '--wait', '10']);
    assert.equal(pushed.value.daemon.settled, true, JSON.stringify(pushed));
    assert.equal(live('return workspace.CurrentCamera.ReniumViewportChild.Value'), 'after');
    run(['lof']);
  }
  run(['br', 'Workspace', '-i', extra[0].id]);
  run(['ps', '--verify']);
  expectedCameraCount = before[2] + 1;
  assert.equal(live('local n=0; for _,v in workspace:GetDescendants() do if v:IsA("Camera") then n+=1 end end; return n'), expectedCameraCount);
  // Simulate a pre-role store: the next plain pull must persist metadata even
  // though every ordinary property already matches.
  run(['bs', 'Workspace', '-i', service.id, '-p', 'CurrentCamera', '--null']);
  pull();
  assert(query([{type: 'instance', path: ['Workspace'], fields: 'prop:CurrentCamera'}])[0].props?.CurrentCamera?.settingsId);
  console.log(JSON.stringify({cycle, nested, startupCameras: before[2], fullPushMs: push.ms, nativePushMs: native.ms, passed: true}));
}
// A fresh runtime must bind the saved role before push; don't warm it with pull.
reopen();
const fresh = live('return workspace.CurrentCamera:GetDebugId()');
run(['ps', '--verify']);
assert.equal(live('return workspace.CurrentCamera:GetDebugId()'), fresh);
assert.equal(live('local n=0; for _,v in workspace:GetDescendants() do if v:IsA("Camera") then n+=1 end end; return n'), expectedCameraCount);
reopen();
pull();
console.log(JSON.stringify({passed: true, platform: process.platform, commands}));
