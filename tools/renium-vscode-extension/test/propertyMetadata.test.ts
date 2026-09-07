import assert from "node:assert/strict";
import { test } from "node:test";
import { normalizeApiDump } from "../src/propertyMetadata";
import fs from "node:fs";
import path from "node:path";
import Module from "node:module";

test("shared API normalization preserves generator/runtime contracts and reference/enum types", () => {
  const source = {
    Classes: [{ Name: "Model", Superclass: "Instance", Tags: ["Browsable"], Members: [
      { Name: "PrimaryPart", MemberType: "Property", ValueType: { Name: "BasePart", Category: "Class" }, Scriptability: "ReadWrite", Security: { Write: "None" } },
      { Name: "Mode", MemberType: "Property", ValueType: { Name: "Enum.Test", Category: "Enum" } },
      { Name: "Enabled", MemberType: "Property", ValueType: { Name: "bool", Category: "Primitive" } },
      { Name: "GetChildren", MemberType: "Function" },
    ] }],
    Enums: [{ Name: "Test", Items: [{ Name: "One", Value: "1" }, { Name: "Invalid", Value: "bad" }] }],
  };
  const generated = normalizeApiDump(source) as { Classes: Record<string, { Properties: Record<string, Record<string, unknown>> }>; Enums: unknown };
  const runtime = normalizeApiDump(source, true) as typeof generated;
  assert.deepEqual(generated.Classes.Model.Properties.PrimaryPart.DataType, { Value: "Ref" });
  assert.deepEqual(generated.Classes.Model.Properties.Mode.DataType, { Enum: "Test" });
  assert.deepEqual(generated.Classes.Model.Properties.Enabled.DataType, { Value: "Bool" });
  assert.equal(generated.Classes.Model.Properties.GetChildren, undefined);
  assert.deepEqual(generated.Enums, { Test: { items: { One: 1 } } });
  assert.equal(generated.Classes.Model.Properties.PrimaryPart.Scriptability, "ReadWrite");
  assert.equal(runtime.Classes.Model.Properties.PrimaryPart.Scriptability, undefined);
  assert.deepEqual(runtime.Classes.Model.Properties.PrimaryPart.DataType, generated.Classes.Model.Properties.PrimaryPart.DataType);
});

test("metadata is read once across projects and invalidates on replacement", () => {
  let revision = 1;
  let metadataReads = 0;
  const apiReads = new Map<string, number>();
  const loader = Module as unknown as { _load(request: string, parent: NodeModule | null, isMain: boolean): unknown };
  const original = loader._load;
  const isMetadata = (name: string) => name.endsWith("roblox-properties.generated.json");
  const isApi = (name: string) => name.endsWith(`${path.sep}API-Dump.json`);
  const filesystem = {
    ...fs,
    existsSync: (name: string) => isMetadata(name) || isApi(name),
    statSync: (name: string) => ({ size: 100, mtimeMs: isMetadata(name) ? revision : 1 }),
    readFileSync: (name: string) => {
      if (isMetadata(name)) {
        metadataReads += 1;
        return JSON.stringify({ classes: { Fixture: { Value: { type: "String", visible: true, displayName: `Value ${revision}` } } } });
      }
      assert.ok(isApi(name));
      apiReads.set(name, (apiReads.get(name) ?? 0) + 1);
      return JSON.stringify({ Classes: {}, Enums: {} });
    },
  };
  loader._load = (request, parent, isMain) => request === "fs" ? filesystem
    : request === "vscode" ? {} : original.call(loader, request, parent, isMain);
  let properties: typeof import("../src/explorerProperties");
  try {
    properties = require("../src/explorerProperties");
  } finally {
    loader._load = original;
  }
  const node = { className: "Fixture", properties: {} } as never;
  const rows = (project: string) => properties.propertyRowsForNode(node, { projectRoot: path.resolve(project) } as never);
  for (const project of ["project-a", "project-b", "project-a"]) {
    assert.equal(rows(project)[0].displayName, "Value 1");
  }
  assert.equal(metadataReads, 1);
  assert.deepEqual([...apiReads.values()], [1, 1]);
  revision = 2;
  assert.equal(rows("project-a")[0].displayName, "Value 2");
  assert.equal(rows("project-b")[0].displayName, "Value 2");
  assert.equal(metadataReads, 2);
});
