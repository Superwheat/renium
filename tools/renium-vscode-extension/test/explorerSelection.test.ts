import assert from "node:assert/strict";
import Module from "node:module";
import { test } from "node:test";
import * as vm from "node:vm";
import ts from "typescript";
import { parseExplorerMessage } from "../src/explorerMessages";
import { fileExplorerWebviewHtml } from "../src/fileExplorerWebview";
import { isImportStageName } from "../src/serviceDefaults";

test("only reserved import staging roots are excluded, not similarly named user data", () => {
  for (const name of [".replicatedstorage.25104-5.renium-import", ".serverstorage.25104-4.renium-import", ".workspace.25104-6.renium-import"]) {
    assert.equal(isImportStageName(name), true);
  }
  for (const name of ["Workspace", ".custom", "user.renium-import", ".foo.invalid.renium-import", ".foo.1-.renium-import"]) {
    assert.equal(isImportStageName(name), false);
  }
});

const loader = Module as unknown as { _load(request: string, parent: NodeModule | null, isMain: boolean): unknown };
const original = loader._load;
loader._load = (request, parent, isMain) => request === "vscode" ? {} : original.call(loader, request, parent, isMain);
const { FileExplorerViewProvider } = require("../src/fileExplorerView");
const { FilePropertiesViewProvider } = require("../src/filePropertiesView");
loader._load = original;

test("clearing Explorer search delivers a valid request and reloads normal rows with or without a selected match", async () => {
  const html = fileExplorerWebviewHtml({ assetBase: "", classNames: [], availableIconNames: new Set(), initialRows: "[]", maxStoreDroppedBytes: 1024 });
  const source = html.split("<script>")[1].split("</script>")[0];
  const tree = ts.createSourceFile("explorer.js", source, ts.ScriptTarget.Latest, true, ts.ScriptKind.JS);
  let input: ts.ExpressionStatement | undefined;
  function visit(node: ts.Node): void {
    if (ts.isExpressionStatement(node) && ts.isCallExpression(node.expression)
      && node.expression.expression.getText(tree) === "search.addEventListener"
      && node.expression.arguments[0]?.getText(tree) === "'input'") { input = node; }
    ts.forEachChild(node, visit);
  }
  visit(tree);
  assert.ok(input, "Production Explorer search input handler is missing");
  for (const revealId of [null, "Workspace/Match"]) {
    let handler!: () => void;
    const messages: unknown[] = [];
    const context = vm.createContext({
      search: { value: "", addEventListener: (_type: string, callback: () => void) => { handler = callback; } },
      filter: "mesh", searchRevision: 1, searchRevealId: revealId, prefetchTimer: null, searchDebounce: 1,
      document: { activeElement: null }, tree: { scrollTop: 88 },
      clearTimeout() {}, resetRowCache() {}, hideSearchSuggestions() {}, renderFlatRows() {}, updateSearchMeta() {},
      visibleCount: () => 120,
      vscode: { postMessage: (message: unknown) => messages.push(JSON.parse(JSON.stringify(message))) },
    });
    vm.runInContext(input.getText(tree), context);
    handler();
    assert.equal(messages.length, 1);
    const message = parseExplorerMessage(messages[0]);
    assert.ok(message, "The host must accept the message replacing Loading with normal rows");
    assert.equal(message.type, "clearSearch");
    const rows: unknown[][] = [];
    let cleared = false;
    const view = Object.assign(Object.create(FileExplorerViewProvider.prototype), {
      searchGeneration: 0, currentMode: "search",
      backend: { clearSearch: async () => { cleared = true; }, revealNode: async () => ({ rowIndex: 100 }) },
      requestRows: async (...args: unknown[]) => { rows.push(args); },
    });
    await view.onMessage(message);
    assert.equal(cleared, true);
    assert.equal(context.filter, "");
    assert.deepEqual(rows, [[revealId ? 40 : 0, 120, "normal", !!revealId]]);
  }
});

test("Explorer message boundary rejects malformed payloads while retaining optional defaults", () => {
  for (const input of [null, [], {}, { type: "constructor" }, { type: "getRows", start: "0" },
    { type: "getRows", count: Infinity }, { type: "getRows", mode: "invalid" },
    { type: "restoreHistoryGroup", historyIds: [1] }, { type: "moveInstance", targetId: {} },
    { type: "storeSelect", node: { properties: [] } }]) {
    assert.equal(parseExplorerMessage(input), undefined);
  }
  for (const input of [{ type: "getRows" }, { type: "getRows", start: 0, count: 120, mode: "search" },
    { type: "createInstance", nodeId: "parent", className: "Folder" }, { type: "storeSelect", node: {} }]) {
    assert.equal(parseExplorerMessage(input), input);
  }
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

test("late Explorer selection cannot replace the latest selection or revive a cleared/project-switched one", async () => {
  for (const outcome of ["second", "clear", "project", "stale-error"]) {
    const first = deferred<unknown>();
    const second = deferred<unknown>();
    const shown: string[] = [];
    const view = Object.assign(Object.create(FileExplorerViewProvider.prototype), {
      selectionSerial: 0, projectGeneration: 0, propertyOnlyStaleServices: new Set(),
      serviceFromNodeId: () => "Workspace", nodeFromBackend: (node: unknown) => node,
      backend: { selectDetails: (id: string) => id === "a" ? first.promise : second.promise },
      model: { rememberNode: (node: unknown) => node },
      propertiesProvider: { show: async (node: { id: string }) => { shown.push(node.id); } },
      actions: {},
    });
    const pending = view.selectNode("a");
    if (outcome === "clear") { view.clearSelection(); }
    else if (outcome === "project") { view.projectGeneration += 1; }
    else {
      const selected = view.selectNode("b");
      second.resolve({ details: { id: "b" } });
      await selected;
    }
    if (outcome === "stale-error") { first.reject(new Error("old request failed")); }
    else { first.resolve({ details: { id: "a" } }); }
    await pending;
    const newer = outcome === "second" || outcome === "stale-error";
    assert.deepEqual(shown, newer ? ["b"] : []);
    assert.equal(view.selectedId, newer ? "b" : undefined);
  }
});

test("Properties drops stale detail loads and refreshes after a newer selection", async () => {
  for (const refresh of [false, true]) {
    const first = deferred<unknown>();
    const second = deferred<unknown>();
    const shown: string[] = [];
    const a = { treeId: "a", service: "Workspace" };
    const b = { treeId: "b", service: "Workspace" };
    const view = Object.assign(Object.create(FilePropertiesViewProvider.prototype), {
      projectGeneration: 0, selectionRevision: 0, currentNode: a,
      model: {
        getNode: () => a,
        loadDetails: (node: { treeId: string }) => node.treeId === "a" ? first.promise : second.promise,
      },
      pushCurrent: () => { shown.push(view.currentNode.treeId); },
    });
    const old = refresh ? view.refreshCurrent() : view.show(a);
    const latest = view.show(b);
    second.resolve(b);
    await latest;
    first.resolve(a);
    await old;
    assert.equal(view.currentNode.treeId, "b");
    assert.deepEqual(shown, ["b"]);
  }
});
