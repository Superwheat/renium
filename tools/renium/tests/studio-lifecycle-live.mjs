// Opt-in regression for closing a runtime and immediately reopening its local file.
import assert from 'node:assert/strict';
import path from 'node:path';
import {spawnSync} from 'node:child_process';

const [cli, root, consent] = process.argv.slice(2);
assert(cli && path.isAbsolute(root) && consent === '--allow-disposable-restart');
const place = 'ReniumPropertyPackageTest.rbxl';
const timings = [];
function run(args) {
  const started = performance.now();
  const result = spawnSync(cli, ['--place', place, ...args], {
    cwd: root, encoding: 'utf8', windowsHide: true, timeout: 20_000,
  });
  timings.push({op: args[0], ms: Math.round(performance.now() - started)});
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr || result.stdout);
  return JSON.parse(result.stdout);
}
const initial = run(['status']);
assert.equal(initial.playState, 'stopped');
assert.equal(initial.clients.length, 1);
assert.equal(initial.clients[0].placeId, 0);
run(['lof']);
let previous = initial.selected;
for (let cycle = 0; cycle < 3; cycle++) {
  run(['sx', '--terminate']);
  run(['ro', path.join(root, place)]);
  // ro acknowledges process launch; a live command waits for the new bridge.
  assert.equal(run(['l', 'return game.PlaceId==0 and game:GetService("RunService"):IsEdit()']).results[0], true);
  const current = run(['status']);
  assert.equal(current.playState, 'stopped', JSON.stringify(current));
  assert.equal(current.clients.length, 1);
  assert.equal(current.clients[0].placeId, 0);
  assert.notEqual(current.selected, previous);
  previous = current.selected;
}
console.log(JSON.stringify({passed: true, platform: process.platform, cycles: 3, timings}));
