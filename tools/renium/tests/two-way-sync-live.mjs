// Opt-in release test. Mutates only an unpublished ReniumNetworkTest fixture.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {spawnSync} from 'node:child_process';
import {performance} from 'node:perf_hooks';

const [cli, root, consent] = process.argv.slice(2);
assert(cli && root && consent === '--allow-play');
const project = path.join(root, 'renium.project.jsonc');
const place = 'ReniumNetworkTest.rbxl';
const timings = [];
let replacementRetries = 0;
function run(args, input, expectedFailure = false) {
  const start = performance.now();
  const child = spawnSync(cli, ['--project', project, '--place', place, ...args], {
    cwd: root, input, encoding: 'utf8', timeout: 20_000, windowsHide: true,
    maxBuffer: 4 * 1024 * 1024,
  });
  assert.ifError(child.error);
  timings.push({command: args[0], ms: Math.round((performance.now() - start) * 100) / 100});
  if (expectedFailure) {
    assert.notEqual(child.status, 0, child.stdout);
    return child.stderr + child.stdout;
  }
  assert.equal(child.status, 0, `${args.join(' ')}: ${child.stderr}${child.stdout}`);
  const result = JSON.parse(child.stdout);
  assert.notEqual(result.ok, false, JSON.stringify(result));
  return result;
}
const live = code => run(['l', code]).results[0];
const saved = (service, segments, fields) => run(['bb', service, '-J', '-'], JSON.stringify({
  ops: [{type: 'instance', path: [service, ...segments], fields}],
})).rs[0];
function settled() {
  const result = run(['lst', '--wait', '10']);
  assert.equal(result.pendingChanges, 0, JSON.stringify(result));
  assert.equal(result.daemon?.pendingCount, 0, JSON.stringify(result));
  assert.equal(result.daemon?.resolutionRequired, false, JSON.stringify(result));
  assert(!result.daemon?.error, JSON.stringify(result));
  return result;
}
const initial = run(['status']);
assert.equal(initial.playState, 'stopped');
assert.equal(initial.clients.length, 1);
assert.equal(initial.clients[0].placeId, 0);
assert.equal(initial.clients[0].placeName, place);
assert.equal(initial.clients[0].bridgeBuildUnix, 1788765703, 'Expected installed 0.3.5 plugin');
const editRuntime = initial.selected;
run(['lof']);
if (live('return workspace:FindFirstChild("ReniumFinalSync") == nil')) {
  const fixture = path.join(path.dirname(fileURLToPath(import.meta.url)), 'fixtures/network-simulation/sync-probe.luau');
  run(['l', '-'], fs.readFileSync(fixture, 'utf8'));
}
run(['pl', '-r', root]);
const original = saved('Workspace', ['ReniumFinalSync'], 'lookup,attr:Revision');
assert.equal(original.attrs.Revision, 0);
const source = path.join(root, 'src/ReplicatedStorage/ReniumFinalSource.luau');
assert.equal(fs.readFileSync(source, 'utf8'), 'return { revision = 0 }\n');
const targetIdentity = live('return workspace.ReniumFinalSync.Target:GetDebugId()');
run(['ps', '--verify']);
assert.equal(live('return workspace.ReniumFinalSync.Target:GetDebugId()'), targetIdentity);
fs.writeFileSync(source, 'return { revision = 1 }\n');
run(['bs', 'Workspace', '-i', original.id, '-p', 'Revision', '--scope', 'attribute', '--num', '1']);
run(['ps', '--verify']);
assert.deepEqual(live('local r=workspace.ReniumFinalSync; return {r:GetAttribute("Revision"),game.ReplicatedStorage.ReniumFinalSource.Source,r.Pointer.Value==r.Target,r.Target:GetDebugId()}'),
  [1, 'return { revision = 1 }\n', true, targetIdentity]);
run(['lon']);
settled();

// Burst replaces exercise watcher coalescing, not one command per source save.
for (let revision = 2; revision <= 101; revision++) {
  const text = `return { revision = ${revision} }\n`;
  if (revision % 2) {
    fs.writeFileSync(source + '.swap', text);
    const deadline = performance.now() + 500;
    for (;;) {
      try { fs.renameSync(source + '.swap', source); break; }
      catch (error) {
        if (process.platform !== 'win32' || !['EPERM', 'EBUSY', 'EACCES'].includes(error.code) || performance.now() >= deadline) throw error;
        replacementRetries++;
        Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 10);
      }
    }
  } else fs.writeFileSync(source, text);
}
for (let revision = 2; revision <= 21; revision++) {
  run(['bs', 'Workspace', '-i', original.id, '-p', 'Revision', '--scope', 'attribute', '--num', String(revision)]);
}
settled();
assert.deepEqual(live('return {workspace.ReniumFinalSync:GetAttribute("Revision"),game.ReplicatedStorage.ReniumFinalSource.Source}'), [21, 'return { revision = 101 }\n']);
live('local r=workspace.ReniumFinalSync; for i=1,100 do r:SetAttribute("Revision",1000+i); local f=Instance.new("Folder"); f.Name="Canceled"; f.Parent=r; f.Name="Renamed"; f:Destroy() end; game.ReplicatedStorage.ReniumFinalSource.Source="return { revision = 202 }\\n"; r.Target.Parent=game.ServerStorage; r.Pointer.Name="MovedPointer"; return true');
settled();
assert.equal(saved('Workspace', ['ReniumFinalSync'], 'attr:Revision').attrs.Revision, 1100);
assert.equal(fs.readFileSync(source, 'utf8'), 'return { revision = 202 }\n');
const movedTarget = saved('ServerStorage', ['Target'], 'lookup,attr:Marker');
assert.equal(movedTarget.attrs.Marker, 'keep');
const pointer = saved('Workspace', ['ReniumFinalSync', 'MovedPointer'], 'prop:Value');
assert.equal(pointer.props.Value.settingsId ?? `debug:${pointer.props.Value.debugId}`, movedTarget.id, JSON.stringify(pointer));
const duplicates = run(['bb', 'Workspace', '-J', '-'], JSON.stringify({ops: [{type:'search',q:'Duplicate',fields:'lookup,prop:Value',limit:5}]})).rs[0];
assert.deepEqual(duplicates.ns.filter(n => n.c === 'StringValue').map(n => n.props.Value).sort(), ['duplicate-1', 'duplicate-2']);
run(['mv', 'ServerStorage', '-i', movedTarget.id, '--to-service', 'Workspace', '-I', original.id]);
for (let index = 0; index < 10; index++) {
  const created = run(['ba', 'Workspace', '-I', original.id, '-n', `Transient${index}`, '-c', 'Folder']);
  assert(created.settingsId, JSON.stringify(created));
  run(['br', 'Workspace', '-i', created.settingsId]);
}
settled();
assert.equal(live('local r=workspace.ReniumFinalSync; return r.MovedPointer.Value==r.Target and #r:GetChildren()==4'), true);
// Cross-service moves currently recreate the engine object and rebind references.
// No-op pushes and unrelated edits must preserve that resulting object.
const movedIdentity = live('return workspace.ReniumFinalSync.Target:GetDebugId()');

// Explicitly test three real starts/stops; Play-only data must never be exported.
let playing = false;
try {
  for (let cycle = 1; cycle <= 3; cycle++) {
    playing = true;
    run(['play', '-s']);
    assert.equal(run(['status']).playState, 'running');
    const revision = 300 + cycle;
    fs.writeFileSync(source, `return { revision = ${revision} }\n`);
    assert.equal(live('assert(game:GetService("RunService"):IsRunning()); local f=Instance.new("Folder"); f.Name="ReniumPlayOnly"; f.Parent=workspace; return true'), true);
    assert.equal(run(['lst']).runtimeId, editRuntime);
    run(['play', '-x']);
    playing = false;
    settled();
    assert.equal(live('return game.ReplicatedStorage.ReniumFinalSource.Source'), `return { revision = ${revision} }\n`);
    assert.equal(live('return workspace:FindFirstChild("ReniumPlayOnly")==nil'), true);
    const playOnly = run(['bb', 'Workspace', '-J', '-'], JSON.stringify({ops:[{type:'search',q:'ReniumPlayOnly',fields:'lookup',limit:2}]})).rs[0];
    assert.deepEqual(playOnly.m, []);
    console.log(JSON.stringify({cycle, deferredSourceApplied: revision, editRuntime}));
  }
} finally {
  if (playing) run(['play', '-x']);
}
for (let cycle = 1; cycle <= 10; cycle++) {
  run(['lof']);
  const editor = `return { revision = ${400 + cycle * 2} }\n`;
  const studio = `return { revision = ${401 + cycle * 2} }\n`;
  fs.writeFileSync(source, editor);
  live(`game.ReplicatedStorage.ReniumFinalSource.Source=${JSON.stringify(studio)}; return true`);
  const conflict = run(['lon'], undefined, true);
  assert.match(conflict, /changed on both sides/);
  assert.equal(fs.readFileSync(source, 'utf8'), editor);
  assert.equal(live('return game.ReplicatedStorage.ReniumFinalSource.Source'), studio);
  if (cycle % 2 === 0) {
    const stopped = run(['lof']);
    assert.equal(stopped.daemon.running, false);
    assert.match(stopped.daemon.previousError, /changed on both sides/);
  }
  run(['lon', '--prefer', 'editor']);
  settled();
  assert.equal(live('return game.ReplicatedStorage.ReniumFinalSource.Source'), editor);
}
run(['lof']);
run(['pl', '-r', root]);
run(['ps', '--verify']);
assert.equal(live('local r=workspace.ReniumFinalSync; return r.MovedPointer.Value==r.Target and r.Target:GetDebugId()'), movedIdentity);
console.log(JSON.stringify({passed:true, platform:process.platform, crossServiceObjectRecreated:movedIdentity !== targetIdentity, replacementRetries, timings}));
