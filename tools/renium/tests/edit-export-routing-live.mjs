// Opt-in, disposable local fixture only. Never runs against a published place.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { performance } from 'node:perf_hooks';
import path from 'node:path';

const [cli, project, place, consent] = process.argv.slice(2);
assert(cli && project && place && consent === '--allow-play');
const timings = [];
function run(args, input) {
  const start = performance.now();
  const child = spawnSync(cli, ['--project', project, '--place', place, ...args], {
    encoding: 'utf8', timeout: 20_000, windowsHide: true, input, cwd: path.dirname(project),
  });
  assert(!child.error, `${args[0]}: ${child.error}`);
  assert.equal(child.status, 0, `${args[0]}: ${child.stderr}${child.stdout}`);
  const result = JSON.parse(child.stdout);
  assert.notEqual(result.ok, false, JSON.stringify(result));
  timings.push({command: args[0], ms: performance.now() - start});
  return result;
}
const initial = run(['status']);
assert.equal(initial.playState, 'stopped');
assert.equal(initial.clients.length, 1);
assert.equal(initial.clients[0].placeId, 0);
assert.equal(initial.clients[0].placeName, 'ReniumNetworkTest.rbxl');
const edit = initial.selected;
const check = run(['l', 'return workspace:FindFirstChild("ReniumRoutingProbe") == nil and workspace:FindFirstChild("ReniumPlayOnlyProbe") == nil']);
assert.equal(check.results[0], true, 'Fixture already has probe names; do not overwrite them');
let mayBePlaying = false;
try {
  for (let cycle = 1; cycle <= 3; cycle++) {
    const changed = run(['l', `assert(game:GetService("RunService"):IsEdit(),"Expected Edit before creating the sync probe"); local p=workspace:FindFirstChild("ReniumRoutingProbe"); if not p then p=Instance.new("Folder"); p.Name="ReniumRoutingProbe"; p.Parent=workspace end; p:SetAttribute("Revision",${cycle}); return p:GetAttribute("Revision")`]);
    assert.equal(changed.results[0], cycle);
    mayBePlaying = true;
    run(['play', '-s']);
    const playing = run(['status']);
    assert.equal(playing.playState, 'running');
    assert(playing.clients.some(client => client.role === 'play-server'));
    const server = run(['l', 'assert(game:GetService("RunService"):IsRunning(),"Expected the play server"); local p=Instance.new("Folder"); p.Name="ReniumPlayOnlyProbe"; p.Parent=workspace; return true']);
    assert.equal(server.results[0], true);
    const during = run(['lst']);
    assert.equal(during.runtimeId, edit);
    assert(!during.daemon?.error, JSON.stringify(during));
    run(['play', '-x']);
    mayBePlaying = false;
    const settled = run(['lst', '--wait', '10']);
    assert.equal(settled.pendingChanges, 0);
    assert(!settled.daemon?.error, JSON.stringify(settled));
    const saved = run(['bb', 'Workspace', '-J', '-'], JSON.stringify({ops: [
      {type:'instance', path:['Workspace','ReniumRoutingProbe'], fields:'lookup,attr:Revision'},
      {type:'search', q:'ReniumPlayOnlyProbe', fields:'lookup', limit:10},
    ]}));
    assert.equal(saved.rs[0].attrs?.Revision, cycle, JSON.stringify({saved,during,settled}));
    assert.deepEqual(saved.rs[1].m, [], 'Play-only data leaked into saved Edit state');
    console.log(JSON.stringify({cycle, edit, settled:true, savedRevision:cycle}));
  }
} finally {
  if (mayBePlaying) run(['play', '-x']);
  run(['l', 'local p=workspace:FindFirstChild("ReniumRoutingProbe"); if p then p:Destroy() end; return true']);
  const settled = run(['lst', '--wait', '10']);
  assert.equal(settled.pendingChanges, 0);
}
console.log(JSON.stringify({passed:true, cycles:3, timings}));
