// Opt-in disposable-place scale check. No rendering, network assets or Play required.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {spawnSync} from 'node:child_process';
import {performance} from 'node:perf_hooks';
const [cli, root, consent] = process.argv.slice(2);
assert(cli && root && consent === '--allow-large-fixture');
const timings = [];
function run(args) {
  const start = performance.now();
  const r = spawnSync(cli, ['--project', path.join(root,'renium.project.jsonc'), '--place','ReniumNetworkTest.rbxl',...args], {
    cwd:root,encoding:'utf8',windowsHide:true,timeout:20_000,maxBuffer:1024*1024,
  });
  assert.ifError(r.error);
  assert.equal(r.status,0,r.stderr || r.stdout);
  const result=JSON.parse(r.stdout);
  assert.notEqual(result.ok,false,JSON.stringify(result));
  const timing={command:args[0],ms:Math.round((performance.now()-start)*100)/100};
  timings.push(timing);
  console.log(JSON.stringify(timing));
  return result;
}
const initial=run(['status']);
assert.equal(initial.playState,'stopped');
assert.equal(initial.clients.length,1);
assert.equal(initial.clients[0].placeId,0);
assert.equal(initial.clients[0].placeName,'ReniumNetworkTest.rbxl');
run(['lof']);
assert.equal(run(['l', `assert(game.PlaceId==0 and game:GetService("RunService"):IsEdit()); assert(not workspace:FindFirstChild("ReniumScaleFixture")); local root=Instance.new("Folder"); root.Name="ReniumScaleFixture"; root.Parent=workspace; for bucket=1,1000 do local f=Instance.new("Folder"); f.Name=tostring(bucket); f.Parent=root; for index=1,114 do local v=Instance.new("StringValue"); v.Name=tostring(index); v.Value="scale-value"; v.Parent=f end; if bucket%20==0 then task.wait() end end; return #root:GetDescendants()`]).results[0],115000);
const source=path.join(root,'src/ReplicatedStorage/ReniumFinalSource.luau');
for (let sample=0;sample<3;sample++) {
  run(['pl','-r',root]);
  run(['ps','--verify']);
  const started=run(['lon']);
  assert.equal(started.pendingChanges,0);
  const revision=500+sample;
  fs.writeFileSync(source,`return { revision = ${revision} }\n`);
  const settled=run(['lst','--wait','10']);
  assert.equal(settled.pendingChanges,0,JSON.stringify(settled));
  assert.equal(settled.daemon.pendingCount,0,JSON.stringify(settled));
  assert.equal(settled.daemon.settled,true,JSON.stringify(settled));
  assert.equal(run(['l','return game.ReplicatedStorage.ReniumFinalSource.Source']).results[0],`return { revision = ${revision} }\n`);
  run(['l',`game.ReplicatedStorage.ReniumFinalSource.Source="return { revision = ${revision+10} }\\n"; return true`]);
  assert.equal(run(['lst','--wait','10']).pendingChanges,0);
  assert.equal(fs.readFileSync(source,'utf8'),`return { revision = ${revision+10} }\n`);
  run(['lof']);
}
assert.equal(run(['l','return #workspace.ReniumScaleFixture:GetDescendants()']).results[0],115000);
console.log(JSON.stringify({passed:true,scaleInstances:115001,platform:process.platform,timings}));
