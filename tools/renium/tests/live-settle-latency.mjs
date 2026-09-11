import assert from 'node:assert/strict';
import path from 'node:path';
import {execFile} from 'node:child_process';
// Opt-in installed-build regression: simultaneous status reads must not undo
// a newer acknowledgment or keep restarting an already quiet sync wait.
const [cli,fixture,consent,place='ReniumPropertyPackageTest.rbxl']=process.argv.slice(2);
assert(cli && fixture && consent==='--allow-disposable-edit');
let cycle=0;
async function run(args,input){
  const start=performance.now();
  const {stdout,stderr}=await new Promise((resolve,reject)=>{
    const child=execFile(cli,[
      '--project',path.join(fixture,'renium.project.jsonc'),'--place',place,...args,
    ],{cwd:fixture,timeout:20000,windowsHide:true,maxBuffer:4*1024*1024},(error,stdout,stderr)=>error?reject(error):resolve({stdout,stderr}));
    child.stdin.end(input);
  });
  const value=JSON.parse(stdout);
  if(args[0]==='lst' && args.includes('--wait'))
    console.log(JSON.stringify({cycle,ms:performance.now()-start,value,stderr}));
  assert.notEqual(value.ok,false,stdout);
  return value;
}
const live=async code=>(await run(['l',code])).results[0];
const saved=async()=> (await run(['bb','ReplicatedStorage','-J','-'],JSON.stringify({ops:[{type:'instance',path:['ReplicatedStorage','ReniumSettleLatencyProbe'],fields:'lookup,attr:Revision'}]}))).rs[0];
const status=await run(['status']);
assert.equal(status.playState,'stopped');assert.equal(status.clients.length,1);assert.equal(status.clients[0].placeId,0);
assert.equal(status.clients[0].placeName,place);
await run(['lof']);
const identity=await live('assert(game.PlaceId==0 and game:GetService("RunService"):IsEdit()); assert(not game.ReplicatedStorage:FindFirstChild("ReniumSettleLatencyProbe")); local f=Instance.new("Folder"); f.Name="ReniumSettleLatencyProbe"; f:SetAttribute("Revision",0); f.Parent=game.ReplicatedStorage; return f:GetDebugId()');
await run(['pl','-r',fixture]);
const id=(await saved()).id;
await run(['lon']);
const waits=[];
async function settle(){
  const started=performance.now();
  const results=await Promise.all([run(['lst','--wait','10']),run(['lst']),run(['lst'])]);
  const duration=performance.now()-started;waits.push(duration);
  assert.equal(results[0].daemon.settled,true,JSON.stringify(results[0]));
  assert(duration<5000,`cycle ${cycle}: sync wait took ${duration.toFixed(1)}ms`);
  assert.equal(results[0].pendingChanges,0,JSON.stringify(results[0]));
  assert.equal(results[0].daemon.pendingCount,0,JSON.stringify(results[0]));
}
for(cycle=1;cycle<=100;cycle++){
  await live(`game.ReplicatedStorage.ReniumSettleLatencyProbe:SetAttribute("Revision",${cycle*2-1}); return true`);
  await settle();
  assert.equal((await saved()).attrs.Revision,cycle*2-1);
  await run(['bs','ReplicatedStorage','-i',id,'-p','Revision','--scope','attribute','--num',String(cycle*2)]);
  await settle();
  assert.deepEqual(await live('local f=game.ReplicatedStorage.ReniumSettleLatencyProbe; return {f:GetAttribute("Revision"),f:GetDebugId()}'),[cycle*2,identity]);
  if(cycle%10===0)console.log(JSON.stringify({cycle,maxWaitMs:Math.max(...waits)}));
}
await run(['lof']);
await live('local f=game.ReplicatedStorage.ReniumSettleLatencyProbe; assert(f:IsA("Folder") and f:GetAttribute("Revision")==200); f:Destroy(); return true');
await run(['pl','-r',fixture]);
waits.sort((a,b)=>a-b);
console.log(JSON.stringify({passed:true,cycles:100,waits:waits.length,p50:waits[100],p95:waits[190],max:waits.at(-1)}));
