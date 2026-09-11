// Opt-in. Requires an already-running, disposable two-client test place with
// ReniumNetworkEcho (RemoteFunction); see fixtures/network-simulation.
// No Play starts, sync or publication here.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { performance } from 'node:perf_hooks';
const [cli, project, place, consent] = process.argv.slice(2);
assert(cli && project && place && consent === '--allow-network-changes');
const timings = [];
function settingsEqual(actual, expected) {
  assert.deepEqual(Object.keys(actual).sort(), Object.keys(expected).sort());
  for (const key of Object.keys(expected)) assert(Math.abs(actual[key] - expected[key]) < 0.00001, `${key}: ${actual[key]} != ${expected[key]}`);
}
function run(args, success = true) {
  const start = performance.now();
  const child = spawnSync(cli, ['--project', project, '--place', place, '--output-mode', 'json', ...args], {
    encoding: 'utf8', timeout: 20_000, windowsHide: true,
  });
  assert(!child.error, `${args[0]}: ${child.error}`);
  assert.equal(child.status === 0, success, `${args.join(' ')}: ${child.stderr}${child.stdout}`);
  timings.push({ command: args[0], ms: performance.now() - start });
  return !success && !child.stdout.trim() ? { error: child.stderr } : JSON.parse(child.stdout);
}
const show = player => run(['net', 'show', '-p', String(player)]);
const baseline = [show(1), show(2)];
assert.notEqual(baseline[0].pid, baseline[1].pid);
assert.notEqual(baseline[0].runtimeId, baseline[1].runtimeId);
function set(player, args) {
  const result = run(['net', 'set', '-p', String(player), ...args]);
  assert.equal(result.runtimeId, baseline[player - 1].runtimeId);
  assert.equal(result.pid, baseline[player - 1].pid);
  return result;
}
function rtt(player) {
  // Compare typical round trips; isolated startup stalls can dominate a mean
  // even when the simulated delay correctly applies to every later request.
  const code = 'local f=game.ReplicatedStorage:FindFirstChild("ReniumNetworkEcho"); assert(f); local samples={}; for i=1,9 do local start=os.clock(); assert(f:InvokeServer()); samples[i]=(os.clock()-start)*1000 end; table.sort(samples); return {samples[5],samples}';
  const [medianMs, samplesMs] = run(['lc', code, String(player)]).results[0];
  console.log(JSON.stringify({phase:'rtt', player, medianMs, samplesMs}));
  return medianMs;
}
try {
  for (const player of [1, 2]) run(['net', 'reset', '-p', String(player)]);
  const before = [rtt(1), rtt(2)];
  set(1, ['--in-delay', '50', '--out-delay', '50']);
  assert.deepEqual(show(2).settings, { inDelay:0,outDelay:0,inJitter:0,outJitter:0,inLoss:0,outLoss:0 });
  const after = [rtt(1), rtt(2)];
  assert(after[0] > before[0] + 50, `Network delay did not affect actual traffic: ${before} -> ${after}`);
  assert(after[1] < before[1] + 60, `Other client gained latency: ${before} -> ${after}`);
  console.log(JSON.stringify({ phase:'traffic', beforeMs:before, afterMs:after }));
  const presets = run(['net', 'presets']).presets;
  for (const preset of presets) {
    settingsEqual(set(1, ['--preset', preset.name]).settings, preset.settings);
    assert.equal(show(2).settings.inDelay, 0);
  }
  const expected = [show(1).settings, show(2).settings];
  for (let index = 0; index < 40; index++) {
    const player = index % 2 + 1;
    const delay = index * 7 % 101;
    const result = set(player, ['--in-delay', String(delay)]);
    expected[player - 1] = { ...expected[player - 1], inDelay:delay };
    assert.deepEqual(result.settings, expected[player - 1]);
  }
  for (const player of [1, 2]) assert.deepEqual(show(player).settings, expected[player - 1]);
  run(['net', 'set', '-p', '1', '--in-delay', '20', '--out-loss', '1'], false);
  assert.deepEqual(show(1).settings, expected[0]);
  run(['net', 'set', '--in-delay', '20'], false);
  console.log(JSON.stringify({phase:'rapid', changes:40, presets:presets.length, clients:baseline.map(({pid,runtimeId})=>({pid,runtimeId})), timings}));
} finally {
  for (const player of [1, 2]) {
    const restored = run(['net', 'restore', '-p', String(player)]);
    assert.deepEqual(restored.settings, baseline[player - 1].settings);
  }
}
console.log('Network simulation live checks passed; both clients restored. Play remains running.');
