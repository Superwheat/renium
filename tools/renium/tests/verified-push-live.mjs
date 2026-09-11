// Opt-in installed-build regression for a disposable, unpublished Edit fixture.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {spawnSync} from 'node:child_process';
const [cli, root, place, consent] = process.argv.slice(2);
assert(cli && root && place && consent === '--allow-fixture-edit');
const timings = [];
const identityFailures = [];
function run(args, input) {
  const started = performance.now();
  const result = spawnSync(cli, ['--project', path.join(root, 'renium.project.jsonc'), '--place', place, ...args],
    {cwd: root, input, encoding: 'utf8', timeout: 20_000, windowsHide: true, maxBuffer: 4 * 1024 * 1024});
  timings.push({command: args[0], ms: Math.round(performance.now() - started)});
  assert.ifError(result.error);
  assert.equal(result.status, 0, `${args.join(' ')}: ${result.stderr}${result.stdout}`);
  return JSON.parse(result.stdout);
}
const live = code => run(['l', code]).results[0];
const push = () => run(['ps', '--verify']);
const status = run(['status']);
assert.equal(status.playState, 'stopped');
assert.equal(status.clients.length, 1);
assert.equal(status.clients[0].placeId, 0);
run(['lof']);
const originalStorageName = live('return game:GetService("ServerStorage").Name');
const prefix = 'local p=game:GetService("ServerStorage").ReniumPushProof; local v=p.Pointer.Value; ';
assert.equal(live('return game:GetService("ServerStorage"):FindFirstChild("ReniumPushProof")==nil and workspace.CurrentCamera:GetAttribute("ReniumPushProofTest")==nil'), true);
let expected = live(String.raw`local p=Instance.new("Folder"); p.Name="ReniumPushProof";
local a=Instance.new("Folder"); a.Name="A"; a.Parent=p;
local b=Instance.new("Folder"); b.Name="B"; b.Parent=p; b:AddTag("ReniumPushProofMember");
local v=Instance.new("NumberValue"); v.Name="Target"; v.Value=101; v:SetAttribute("Revision",1); v.Parent=a;
local pointer=Instance.new("ObjectValue"); pointer.Name="Pointer"; pointer.Value=v; pointer.Parent=p;
local code=Instance.new("ModuleScript"); code.Name="Code"; code.Source="return 1\n"; code.Parent=p;
p.Parent=game:GetService("ServerStorage"); return v:GetDebugId()`);
function check(value=101, source='return 1\n') {
  assert.deepEqual(live(prefix + 'return {v:GetDebugId(),v.Name,v.Value,v:GetAttribute("Revision"),v.Archivable,v.Parent==p.A,#v:GetTags(),p.Code.Source}'),
    [expected, 'Target', value, 1, true, true, 0, source]);
}
try {
  run(['pl', '-r', root]);
  push(); push(); check();
  // Native service lookup must survive editable display names. Keep the same
  // objects and references through both the renamed pull and subsequent push.
  live('game:GetService("ServerStorage").Name="ReniumPushProofStorage"; return true');
  run(['pl', '-r', root]);
  push(); check();
  assert.equal(live('return game:GetService("ServerStorage").Name'), 'ReniumPushProofStorage');
  live(`game:GetService("ServerStorage").Name=${JSON.stringify(originalStorageName)}; return true`);
  run(['pl', '-r', root]);
  for (const mutation of ['v.Value=202', 'v:SetAttribute("Revision",2)', 'v.Archivable=false',
    ...Array.from({length:100}, (_, index) => `v:AddTag("ReniumPushProofNew${index}")`),
    'v:AddTag("ReniumPushProofMember")', 'v.Name="Renamed"', 'v.Parent=p.B']) {
    console.log(JSON.stringify({checking: mutation}));
    live(prefix + mutation + '; return true');
    push();
    try { check(); } catch (error) {
      identityFailures.push({mutation, error: error.message});
      expected = live(prefix + 'return v:GetDebugId()');
      check(); // Continue only when the restored data is correct.
    }
    push();
  }
  // The active viewport stays unchanged by push, but a subsequent pull must
  // export its current attributes rather than reuse the pre-edit snapshot.
  live('workspace.CurrentCamera:SetAttribute("ReniumPushProofTest",1); return true');
  run(['pl', '-r', root]);
  push(); push();
  live('workspace.CurrentCamera:SetAttribute("ReniumPushProofTest",2); return true');
  push();
  assert.equal(live('return workspace.CurrentCamera:GetAttribute("ReniumPushProofTest")'), 2);
  run(['pl', '-r', root]);
  const cameraPath = live('return {"Workspace", workspace.CurrentCamera.Name}');
  const camera = run(['bb','Workspace','-J','-'], JSON.stringify({ops:[{type:'instance',path:cameraPath,fields:'attr:ReniumPushProofTest'}]})).rs[0];
  assert.equal(camera.attrs.ReniumPushProofTest, 2);
  live('workspace.CurrentCamera:SetAttribute("ReniumPushProofTest",nil); return true');
  run(['pl', '-r', root]);
  push(); push();
  const source = path.join(root, 'src/ServerStorage/ReniumPushProof/Code.luau');
  assert.equal(fs.readFileSync(source, 'utf8'), 'return 1\n');
  for (let revision=2; revision<=12; revision++) {
    fs.writeFileSync(source, `return ${revision}\n`);
    push(); check(101, `return ${revision}\n`); push();
  }
  const saved = run(['bb','ServerStorage','-J','-'], JSON.stringify({ops:[{type:'instance',path:['ServerStorage','ReniumPushProof','A','Target'],fields:'lookup'}]})).rs[0];
  for (let value=102; value<=112; value++) {
    run(['bs','ServerStorage','-i',saved.id,'-p','Value','-j',JSON.stringify(value)]);
    push(); check(value, 'return 12\n'); push();
  }
  const sync = run(['lst']);
  assert.equal(sync.daemon.running, false, JSON.stringify(sync));
  console.log(JSON.stringify({passed:identityFailures.length===0,identityFailures,commands:timings.length,timings}));
  assert.equal(identityFailures.length, 0, 'Push replaced an existing instance');
} finally {
  live(`game:GetService("ServerStorage").Name=${JSON.stringify(originalStorageName)}; return true`);
  live('local p=game:GetService("ServerStorage"):FindFirstChild("ReniumPushProof"); if p then p:Destroy() end; workspace.CurrentCamera:SetAttribute("ReniumPushProofTest",nil); return true');
  run(['pl','-r',root]);
}
