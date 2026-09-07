import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../..');
const binary = path.resolve(process.argv[2] ?? path.join(repository, 'tools/renium/target/debug', process.platform === 'win32' ? 'renium.exe' : 'renium'));
const root = path.join(repository, 'audit', `plugin-host-smoke-${process.pid}`);
fs.mkdirSync(root);
const env = { ...process.env, RENIUM_PLUGIN_HOME: path.join(root, 'registry'), RENIUM_PLUGIN_CHILD: '1' };
delete env.RENIUM_PLACE;
delete env.PLACE;
delete env.RENIUM_RESOURCE_LEASE;
function run(args, ok = true, cwd = root) {
  const result = spawnSync(binary, ['--output-mode', 'json', ...args], { cwd, env, encoding: 'utf8', timeout: 20_000, windowsHide: true });
  assert.equal(result.error, undefined);
  assert.equal(result.status === 0, ok, `${args.join(' ')}\n${result.stdout}\n${result.stderr}`);
  return result;
}
function json(args, cwd) { return JSON.parse(run(args, true, cwd).stdout); }
let complete = false;
try {
  const starter = path.join(root, 'example-workflow');
  assert.equal(json(['plugin', 'process', String(process.pid)]).alive, true);
  assert.equal(json(['plugin', 'new', 'example-workflow']).compiled, false);
  run(['plugin', 'new', 'example-workflow'], false);
  assert.equal(json(['plugin', 'check', starter]).valid, true);
  run(['plugin', 'install', starter], false); // No auto-build on install.
  const build = spawnSync('cargo', ['build', '--release'], { cwd: starter, env, encoding: 'utf8', timeout: 180_000, windowsHide: true });
  assert.equal(build.status, 0, build.stderr);
  json(['plugin', 'install', starter]);
  assert.equal(json(['example-workflow', 'hello', '--name', 'Author']).message, 'Hello, Author!');
  assert.equal(json(['example-workflow', 'hello']).message, 'Hello, World!');
  assert.match(run(['example-workflow', '--help']).stdout, /hello/);
  assert.match(run(['example-workflow', 'hello', '--help']).stdout, /--name/);
  run(['example-workflow', 'hello', '--no-such-flag'], false);
  assert.equal(json(['plugin', 'list']).plugins.length, 1);
  assert.match(json(['plugin', 'info', 'example-workflow']).guide, /without|offline/);
  const manifest = path.join(starter, 'renium-plugin.json');
  fs.appendFileSync(manifest, '\n');
  run(['example-workflow', 'hello'], false); // Changed metadata needs reinstall.
  json(['plugin', 'install', starter, '--dev']);
  assert.equal(json(['example-workflow', 'hello']).message, 'Hello, World!');
  json(['plugin', 'remove', 'example-workflow']);
  run(['example-workflow', 'hello'], false);
  assert.equal(json(['plugin', 'list']).plugins.length, 0);
  assert.ok(fs.existsSync(starter));

  const source = path.join(root, 'project');
  fs.mkdirSync(path.join(source, 'src', 'ServerScriptService'), { recursive: true });
  fs.writeFileSync(path.join(source, 'renium.project.jsonc'), JSON.stringify({ schemaVersion: 1, sourceRoot: 'src' }));
  fs.writeFileSync(path.join(source, 'src', 'ServerScriptService', 'Probe.server.luau'), '-- offline snapshot fixture\n');
  const target = path.join(root, 'snapshot');
  run(['--project', path.join(source, 'renium.project.jsonc'), 'plugin', 'snapshot', path.join(source, 'src', 'recursive-copy')], false);
  const copied = json(['--project', path.join(source, 'renium.project.jsonc'), 'plugin', 'snapshot', target]);
  assert.ok(copied.project);
  assert.equal(fs.readFileSync(path.join(target, 'src', 'ServerScriptService', 'Probe.server.luau'), 'utf8'), '-- offline snapshot fixture\n');
  assert.ok(!fs.existsSync(path.join(target, '.renium', 'live-sync-enabled.json')));
  run(['--project', path.join(source, 'renium.project.jsonc'), 'plugin', 'snapshot', target], false);
  complete = true;
  console.log('Plugin host smoke passed: starter build, install, execution, help, defaults, invalid arguments, changed manifest, dev mode, removal and isolated snapshot. No Studio or sandbox plugin was started.');
} finally {
  // This test created this exact directory; never follow a replacement/junction.
  if (complete && fs.realpathSync(root) === root && path.dirname(root) === path.join(repository, 'audit')) fs.rmSync(root, { recursive: true });
  else console.log(`Preserved test artifacts: ${root}`);
}
