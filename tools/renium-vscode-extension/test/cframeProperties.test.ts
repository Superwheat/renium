import assert from "node:assert/strict";
import { test } from "node:test";
import Module from "node:module";
import path from "node:path";

const loader = Module as unknown as { _load(request: string, parent: NodeModule | null, isMain: boolean): unknown };
const original = loader._load;
loader._load = (request, parent, isMain) => request === "vscode" ? {} : original.call(loader, request, parent, isMain);
let properties: typeof import("../src/explorerProperties");
try { properties = require("../src/explorerProperties"); } finally { loader._load = original; }

type Frame = { _type: string; components: number[] };
type Display = { Position: { X: number; Y: number; Z: number }; Rotation: { X: number; Y: number; Z: number } };
function display(frame: Frame): Display {
  const node = { className: "Folder", properties: {}, attributes: { Transform: frame } } as never;
  return properties.verdePropertyRowsForNode(node, "", { projectRoot: path.resolve("../..") } as never).attributes[0].value as Display;
}
function matrix(x: number, y: number, z: number): number[] {
  const multiply = (a: number[], b: number[]) => a.map((_, i) =>
    [0, 1, 2].reduce((sum, k) => sum + a[Math.floor(i / 3) * 3 + k] * b[k * 3 + i % 3], 0));
  const [sx, cx, sy, cy, sz, cz] = [Math.sin(x), Math.cos(x), Math.sin(y), Math.cos(y), Math.sin(z), Math.cos(z)];
  return multiply(multiply([cy, 0, sy, 0, 1, 0, -sy, 0, cy], [1, 0, 0, 0, cx, -sx, 0, sx, cx]), [cz, -sz, 0, sz, cz, 0, 0, 0, 1]);
}

test("CFrame orientation edits use Roblox YXZ and preserve translation", () => {
  const identity: Frame = { _type: "CFrame", components: [12, -3, 45, 1, 0, 0, 0, 1, 0, 0, 0, 1] };
  for (const angles of [[0, 90, 0], [30, 45, 60], [-90, 12, 45], [90, -72, 13], [89.99, 80, 13], [-23, 179, -163]]) {
    const [X, Y, Z] = angles;
    const edited = properties.bytecodeValueFromVerde({ Rotation: { X, Y, Z } }, "CFrame", identity) as Frame;
    assert.deepEqual(edited.components.slice(0, 3), identity.components.slice(0, 3));
    const expected = matrix(...angles.map(v => v * Math.PI / 180) as [number, number, number]);
    edited.components.slice(3).forEach((v, i) => assert.ok(Math.abs(v - expected[i]) < 1e-12));
    const shown = display(edited);
    const decodedMatrix = matrix(...[shown.Rotation.X, shown.Rotation.Y, shown.Rotation.Z].map(v => v * Math.PI / 180) as [number, number, number]);
    decodedMatrix.forEach((v, i) => assert.ok(Math.abs(v - expected[i]) < 1e-9));
    const moved = properties.bytecodeValueFromVerde({ ...shown, Position: { X: 8, Y: 9, Z: 10 } }, "CFrame", edited) as Frame;
    assert.deepEqual(moved.components, [8, 9, 10, ...edited.components.slice(3)]);
  }
  const rotated: Frame = { _type: "CFrame", components: [12, -3, 45, 0, 0, 1, 0, 1, 0, -1, 0, 0] };
  assert.equal(display(rotated).Rotation.Y, 90);
});

test("moving a CFrame preserves encoded nonfinite rotation components", () => {
  const components = [1, 2, 3, 1, { _type: "Float", value: "nan" }, 0, 0, 1, 0, 0, 0, 1];
  const current = { _type: "CFrame", components };
  const moved = properties.bytecodeValueFromVerde({ Position: { X: 4, Y: 5, Z: 6 } }, "CFrame", current) as { components: unknown[] };
  assert.deepEqual(moved.components, [4, 5, 6, ...components.slice(3)]);
  assert.deepEqual(current.components, components);
});
