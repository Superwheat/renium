// Opt-in lifecycle regression: only the unpublished ReniumNetworkTest fixture.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {spawnSync} from 'node:child_process';
import {performance} from 'node:perf_hooks';

const [cli, root, consent] = process.argv.slice(2);
assert(cli && root && consent === '--allow-disposable-source-edit');
const times = [];
let cycle = 0;
function run(args, input, expectedFailure = false) {
  const started = performance.now();
  const result = spawnSync(cli, ['--project', path.join(root, 'renium.project.jsonc'),
    '--place', 'ReniumNetworkTest.rbxl', ...args], {
    cwd: root, input, encoding: 'utf8', timeout: 20_000, windowsHide: true, maxBuffer: 4 * 1024 * 1024,
  });
  assert.ifError(result.error);
  times.push({cycle, command: args[0], ms: Math.round((performance.now() - started) * 100) / 100});
  const output = result.stderr + result.stdout;
  if (expectedFailure) {
    assert.notEqual(result.status, 0, output);
    assert.match(output, /changed on both sides/);
    return;
  }
  assert.equal(result.status, 0, `cycle ${cycle}, ${args.join(' ')}: ${output}`);
  const value = JSON.parse(result.stdout);
  assert.notEqual(value.ok, false, output);
  return value;
}
const live = code => run(['l', code]).results[0];
const status = run(['status']);
assert.equal(status.playState, 'stopped');
assert.equal(status.clients.length, 1);
assert.equal(status.clients[0].placeId, 0);
assert.equal(status.clients[0].placeName, 'ReniumNetworkTest.rbxl');
assert.equal(live('return game.ReplicatedStorage:FindFirstChild("ReniumFinalSource") ~= nil'), true);
run(['lof']);
run(['pl', '-r', root]);
const source = path.join(root, 'src/ReplicatedStorage/ReniumFinalSource.luau');
const original = fs.readFileSync(source, 'utf8');
try {
  for (cycle = 1; cycle <= 30; cycle++) {
    run(['lof']);
    const editor = `return { revision = ${8000 + cycle * 2} }\n`;
    const studio = `return { revision = ${8001 + cycle * 2} }\n`;
    fs.writeFileSync(source, editor);
    live(`game.ReplicatedStorage.ReniumFinalSource.Source=${JSON.stringify(studio)}; return true`);
    run(['lon'], undefined, true);
    if (cycle % 2 === 0) run(['lof']);
    run(['lon', '--prefer', 'editor']);
    const settled = run(['lst', '--wait', '10']);
    assert.equal(settled.daemon?.settled, true, JSON.stringify(settled));
    assert.equal(settled.pendingChanges, 0);
    assert.equal(settled.daemon?.pendingCount, 0);
    assert.equal(settled.daemon?.resolutionRequired, false);
    assert(!settled.daemon?.error, JSON.stringify(settled.daemon));
    assert.equal(live('return game.ReplicatedStorage.ReniumFinalSource.Source'), editor);
    assert.equal(fs.readFileSync(source, 'utf8'), editor);
    console.log(JSON.stringify({cycle, passed: true}));
  }
} finally {
  run(['lof']);
  fs.writeFileSync(source, original);
  live(`game.ReplicatedStorage.ReniumFinalSource.Source=${JSON.stringify(original)}; return true`);
  run(['pl', '-r', root]);
}
const waits = times.filter(t => t.command === 'lst').map(t => t.ms).sort((a, b) => a - b);
console.log(JSON.stringify({passed: true, platform: process.platform, cycles: 30,
  waitMedianMs: waits[Math.floor(waits.length / 2)], waitMaxMs: waits.at(-1)}));
