// Opt-in live integration test. Cycles only the explicitly selected place's
// Play session. No sync, package actions, publication, or filesystem edits.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { performance } from "node:perf_hooks";

const [cli, project, place, rawCycles, consent] = process.argv.slice(2);
const cycles = Number(rawCycles);
assert(consent === "--allow-play-cycling", "Explicit --allow-play-cycling is required");
assert(cli && project && /^\d+:\d+$/.test(place), "Supply CLI, project, and gameId:placeId");
assert(Number.isInteger(cycles) && cycles > 0 && cycles <= 100, "Cycles must be 1..100");
const placeId = Number(place.split(":")[1]);
const timings = [];

async function command(args) {
  const started = performance.now();
  return await new Promise((resolve, reject) => {
    const child = spawn(cli, ["--place", place, ...args], {
      cwd: project, windowsHide: true, stdio: ["ignore", "pipe", "pipe"],
    });
    let output = "";
    let error = "";
    const timeout = setTimeout(() => {
      child.kill();
      reject(new Error(`${args[0]} exceeded 20 seconds; no further commands will run`));
    }, 20_000);
    child.stdout.on("data", chunk => { output += chunk; });
    child.stderr.on("data", chunk => { error += chunk; });
    child.on("error", failure => { clearTimeout(timeout); reject(failure); });
    child.on("exit", code => {
      clearTimeout(timeout);
      if (code !== 0) return reject(new Error(`${args[0]} exited ${code}: ${error || output}`));
      try {
        const result = JSON.parse(output.trim());
        timings.push({ command: args[0], ms: performance.now() - started });
        resolve(result);
      } catch (failure) { reject(failure); }
    });
  });
}

function entries(result) {
  return Object.fromEntries(result.results[0].entries.map(({ key, value }) => [key, value]));
}

const initial = await command(["status"]);
assert(initial.selected, "No selected edit runtime");
const edit = initial.selected;
let previousNonce = initial.studioState?.launchNonce;
const probe = "return {placeId=game.PlaceId,player=game.Players.LocalPlayer.Name,running=game:GetService('RunService'):IsRunning(),client=game:GetService('RunService'):IsClient()}";

for (let cycle = 1; cycle <= cycles; cycle++) {
  const stopped = await command(["play", "-x"]);
  assert.notEqual(stopped.ok, false);
  const start = await command(["play", "-s"]);
  assert.equal(start.ok, true);
  assert.equal(start.editRuntimeId, edit);
  assert(start.launchNonce && start.launchNonce !== previousNonce, "Start reused a previous session");
  previousNonce = start.launchNonce;
  const clients = start.clients.filter(client => client.role === "play-client");
  assert.equal(clients.length, 1, "Ordinary Play must expose exactly one client");
  const player = clients[0].playerName;
  const verifyClient = async selector => {
    const result = await command(["lc", probe, selector]);
    assert.equal(result.ok, true);
    assert.equal(result.context, "client");
    assert.deepEqual(entries(result), { placeId, player, running: true, client: true });
  };
  // No artificial settle delay. Include overlapping read-only requests while
  // the second channel may still be completing its handshake.
  await verifyClient("1");
  await verifyClient(player);
  await Promise.all(["1", player, "1", player].map(verifyClient));
  const status = await command(["status"]);
  assert.equal(status.selected, edit);
  assert.equal(status.playState, "running");
  assert.equal(status.clients.filter(client => client.role === "play-client").length, 1);
  for (const client of status.clients.filter(client => client.role.startsWith("play-"))) {
    assert.equal(client.launchNonce, start.launchNonce, "Inventory retained an older play session");
  }
  console.log(JSON.stringify({ cycle, launchNonce: start.launchNonce, client: clients[0].runtimeId, ok: true }));
}
const summary = Object.fromEntries([...new Set(timings.map(row => row.command))].map(command => {
  const samples = timings.filter(row => row.command === command).map(row => row.ms).sort((a, b) => a - b);
  return [command, { count: samples.length, medianMs: Math.round(samples[Math.floor(samples.length / 2)]), maxMs: Math.round(samples.at(-1)) }];
}));
console.log(JSON.stringify({ ok: true, cycles, summary, leftRunning: true }));
