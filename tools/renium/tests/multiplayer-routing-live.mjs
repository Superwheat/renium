// Opt-in, disposable local fixture. Verifies separate-process ownership and cleanup.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { performance } from 'node:perf_hooks';

const [cli, project, place, consent] = process.argv.slice(2);
assert(cli && project && place && consent === '--allow-play-cycling');
const timings = [];
function run(args) {
  const start = performance.now();
  const child = spawnSync(cli, ['--project', project, '--place', place, ...args], {
    encoding: 'utf8', timeout: 20_000, windowsHide: true,
  });
  if (child.error || child.status !== 0) {
    console.error(JSON.stringify({args, status: child.status, error: String(child.error ?? ''), stdout: child.stdout, stderr: child.stderr}));
  }
  assert(!child.error, `${args[0]}: ${child.error}`);
  assert.equal(child.status, 0, `${args.join(' ')}: ${child.stderr}${child.stdout}`);
  const result = JSON.parse(child.stdout);
  assert.notEqual(result.ok, false, JSON.stringify(result));
  timings.push({command: args[0], ms: performance.now() - start});
  return result;
}
const initial = run(['status']);
assert.equal(initial.playState, 'stopped');
assert.equal(initial.clients.length, 1);
assert.equal(initial.clients[0].placeId, 0, 'Only a disposable local fixture is supported');
const edit = initial.selected;
let previous = new Set();
let previousNonce;
for (let cycle = 0; cycle < 2; cycle++) {
  try {
    const start = run(['play', '-s', '--players', '2']);
    assert.equal(start.clients.length, 3);
    const nonce = start.clients[0].launchNonce;
    assert(nonce && nonce !== previousNonce);
    const status = run(['status']);
    assert.equal(status.selected, edit);
    assert.equal(status.playState, 'running');
    assert.equal(status.clients.length, 4);
    assert.equal(status.clients.filter(client => client.role === 'play-client').length, 2);
    assert.equal(status.clients.filter(client => client.role === 'play-server').length, 1);
    for (const client of start.clients) {
      assert.equal(client.launchEditRuntimeId, edit);
      assert.equal(client.launchNonce, nonce);
      assert(!previous.has(client.runtimeId), 'An old runtime survived the next launch');
    }
    assert.deepEqual(run(['l', 'return {game:GetService("RunService"):IsServer(), game:GetService("RunService"):IsRunning()}']).results[0], [true, true]);
    for (const player of [1, 2]) {
      const code = 'return {game:GetService("RunService"):IsClient(), game:GetService("RunService"):IsRunning(), game.Players.LocalPlayer.Name}';
      const expected = [true, true, `Player${player}`];
      assert.deepEqual(run(['lc', code, String(player)]).results[0], expected);
      assert.deepEqual(run(['lc', code, `Player${player}`]).results[0], expected);
    }
    previous = new Set(start.clients.map(client => client.runtimeId));
    previousNonce = nonce;
    console.log(JSON.stringify({cycle, nonce, runtimes: [...previous], passed: true}));
  } finally {
    run(['play', '-x']);
    const stopped = run(['status']);
    assert.equal(stopped.playState, 'stopped');
    assert.equal(stopped.selected, edit);
    assert.equal(stopped.clients.length, 1);
  }
}
console.log(JSON.stringify({passed: true, cycles: 2, timings}));
