// Opt-in installed-build test; requires an unpublished geometry fixture.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import {spawnSync} from 'node:child_process';
const [cli,root,probe,place='ReniumPropertyPackageTest.rbxl']=process.argv.slice(2);
const timings=[];
function run(args,input){
  const started=performance.now();
  const r=spawnSync(cli,['--place',place,...args],{cwd:root,input,encoding:'utf8',windowsHide:true,timeout:20000,maxBuffer:4*1024*1024});
  timings.push({op:args[0],ms:Math.round(performance.now()-started)});
  assert.ifError(r.error);assert.equal(r.status,0,r.stderr||r.stdout);return JSON.parse(r.stdout);
}
const live=code=>run(['l',code]).results[0];
const s=run(['status']);assert.equal(s.playState,'stopped');assert.equal(s.clients.length,1);assert.equal(s.clients[0].placeId,0);
run(['lof']);
const identity=run(['l','-'],fs.readFileSync(probe,'utf8')).results[0];
const target='Workspace.ReniumSerializerProbe.Mesh';
const geometryFields='prop:CollisionFidelity,prop:PhysicalConfigData,prop:UnscaledCofm,prop:UnscaledVolInertiaDiags,prop:UnscaledVolInertiaOffDiags,prop:UnscaledVolume,prop:Size,prop:Archivable';
const savedGeometry=()=>run(['bb','Workspace','-J','-'],JSON.stringify({ops:[{type:'instance',path:['Workspace','ReniumSerializerProbe','Mesh'],fields:geometryFields}]})).rs[0].props;
const hull=run(['access','write',target,'CollisionFidelity','Hull']);
assert.equal(hull.value,'Hull',JSON.stringify(hull));
run(['pl','-r',root]);
const saved=run(['bb','Workspace','-J','-'],JSON.stringify({ops:[{type:'instance',path:['Workspace','ReniumSerializerProbe','Mesh'],fields:'prop:CollisionFidelity,prop:Archivable,prop:Size'},{type:'instance',path:['Workspace','ReniumSerializerProbe'],fields:'prop:WorldPivot'}]})).rs;
assert.equal(saved[0].props.CollisionFidelity.name,'Hull',JSON.stringify(saved));
assert.equal(saved[0].props.Archivable,false);
assert(saved[1].props.WorldPivot,JSON.stringify(saved));
run(['access','write',target,'CollisionFidelity','Box']);
run(['ps','--verify']);
assert.equal(run(['access','read',target,'CollisionFidelity']).value,'Hull');
assert.deepEqual(live('local r=workspace.ReniumSerializerProbe; return {r.Mesh:GetDebugId(),r.Mesh.Archivable,r.Pointer.Value==r.Mesh,r.WorldPivot.Position.X,r.Mesh.Size.X}'),[identity,false,true,31,3]);
run(['lon']);
for(let cycle=0;cycle<3;cycle++){
  run(['access','write',target,'CollisionFidelity','Box']);
  const pulled=run(['lst','--wait','10']);
  assert.equal(pulled.pendingChanges,0,JSON.stringify(pulled));
  const mesh=run(['bb','Workspace','-J','-'],JSON.stringify({ops:[{type:'instance',path:['Workspace','ReniumSerializerProbe','Mesh'],fields:'lookup,prop:CollisionFidelity'}]})).rs[0];
  // The exporter elides the new-MeshPart default (Box). Prove that the saved
  // state restores Box as well, instead of assuming every property is explicit.
  assert(!mesh.props?.CollisionFidelity || mesh.props.CollisionFidelity.name==='Box',JSON.stringify(mesh));
  run(['lof']);
  run(['access','write',target,'CollisionFidelity','Hull']);
  run(['ps','--verify']);
  assert.equal(run(['access','read',target,'CollisionFidelity']).value,'Box');
  run(['lon']);
  run(['bs','Workspace','-i',mesh.id,'-p','CollisionFidelity','-j',JSON.stringify(saved[0].props.CollisionFidelity)]);
  const pushed=run(['lst','--wait','10']);
  assert.equal(pushed.pendingChanges,0,JSON.stringify(pushed));
  assert.equal(pushed.daemon.pendingCount,0,JSON.stringify(pushed));
  assert.equal(run(['access','read',target,'CollisionFidelity']).value,'Hull');
  const acceptedGeometry=savedGeometry();
  run(['lof']);
  run(['pl','-r',root]);
  assert.deepEqual(savedGeometry(),acceptedGeometry,'Live Sync must save the complete cooked geometry, not just CollisionFidelity');
  run(['lon']);
}
run(['lof']);
live('workspace.ReniumSerializerProbe:Destroy(); return true');
run(['pl','-r',root]);
console.log(JSON.stringify({passed:true,platform:process.platform,collisionFidelityRestored:true,nonArchivablePreserved:true,pivotAndReferencePreserved:true,identityPreserved:true,totalMs:timings.reduce((n,t)=>n+t.ms,0),slowCommands:timings.filter(t=>t.ms>1000)}));
