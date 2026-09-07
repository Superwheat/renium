// Never publishes. Use only the disposable local package fixture and discard it afterward.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {spawnSync} from 'node:child_process';
const [cli,root,consent]=process.argv.slice(2);
assert(cli && root && consent==='--allow-disposable-package-edit');
function run(args,input) {
  const r=spawnSync(cli,['--project',path.join(root,'renium.project.jsonc'),'--place','ReniumPropertyPackageTest.rbxl',...args],{cwd:root,encoding:'utf8',input,windowsHide:true,timeout:20_000});
  assert.ifError(r.error);
  assert.equal(r.status,0,r.stderr||r.stdout);
  const result=JSON.parse(r.stdout);
  assert.notEqual(result.ok,false,JSON.stringify(result));
  return result;
}
function packageState() {
  const request=run(['access','--wait-seconds','12','read','ReplicatedStorage.ReniumAccessPackage.PackageLink','ModifiedState']);
  assert.equal(request.status,'approval-required');
  return {id:request.intent.instanceId,value:run(['access','approve',request.requestId]).value};
}
const status=run(['status']);
assert.equal(status.playState,'stopped');
assert.equal(status.clients.length,1);
assert.equal(status.clients[0].placeId,0);
assert.equal(status.clients[0].placeName,'ReniumPropertyPackageTest.rbxl');
run(['access','mode','ask']);
const before=packageState();
assert.equal(before.value,'-1','Fixture must start Up To Date');
run(['pl','-r',root]);
run(['ps','--verify']);
assert.deepEqual(packageState(),before,'No-op full push must leave PackageLink untouched');
run(['lon']);
const source=path.join(root,'src/ReplicatedStorage/ReniumAccessPackage/Types.luau');
assert(fs.existsSync(source));
fs.appendFileSync(source,'\n-- Renium 0.3.5 disposable sync verification\n');
const settled=run(['lst','--wait','10']);
assert.equal(settled.pendingChanges,0,JSON.stringify(settled));
assert.equal(settled.daemon.pendingCount,0,JSON.stringify(settled));
assert(settled.daemon.autoDesyncedPackages.includes('ReplicatedStorage.ReniumAccessPackage'),JSON.stringify(settled));
const after=packageState();
assert.equal(after.id,before.id);
assert.equal(after.value,'1');
assert.equal(run(['l','return string.find(game.ReplicatedStorage.ReniumAccessPackage.Types.Source,"Renium 0.3.5 disposable sync verification",1,true)~=nil']).results[0],true);
run(['lof']);
console.log(JSON.stringify({passed:true,platform:process.platform,packageLinkPreserved:true,noOpClean:true,normalSourceEditAutoDesynced:true}));
