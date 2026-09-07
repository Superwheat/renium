import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import { test } from "node:test";
import * as vm from "node:vm";
import ts from "typescript";

type Handler = (event: Record<string, unknown>) => void;
class Element {
  listeners = new Map<string, Set<Handler>>();
  children: Element[] = [];
  style: Record<string, string> = {};
  className = "";
  width = 100;
  addEventListener(type: string, handler: Handler): void {
    const handlers = this.listeners.get(type) ?? new Set();
    handlers.add(handler);
    this.listeners.set(type, handlers);
  }
  removeEventListener(type: string, handler: Handler): void { this.listeners.get(type)?.delete(handler); }
  dispatch(type: string, event: Record<string, unknown> = {}): void {
    for (const handler of [...this.listeners.get(type) ?? []]) { handler(event); }
  }
  count(): number { return [...this.listeners.values()].reduce((n, handlers) => n + handlers.size, 0); }
  appendChild(child: Element): void { this.children.push(child); }
  setAttribute(): void {}
  getBoundingClientRect(): { left: number; width: number } { return { left: 0, width: this.width }; }
  find(className: string): Element | undefined {
    return this.className === className ? this : this.children.map(child => child.find(className)).find(Boolean);
  }
}

function webview() {
  const source = fs.readFileSync(path.resolve(__dirname, "../../resources/properties.html"), "utf8").split("<script>")[1].split("</script>")[0];
  const tree = ts.createSourceFile("properties.js", source, ts.ScriptTarget.Latest, true, ts.ScriptKind.JS);
  const document = Object.assign(new Element(), { createElement: () => new Element() });
  const window = new Element();
  const timers = new Map<number, () => void>();
  let nextTimer = 0;
  const schedule = (callback: () => void) => { timers.set(++nextTimer, callback); return nextTimer; };
  const seeks: number[] = [];
  const context = vm.createContext({
    document, window, stopAudioScrub: null, stopNumberRepeat: null, audioScrubbing: false, audioPlaying: false,
    allProperties: [{ name: "TimeLength", value: 60 }], allTags: [], allAttributes: [], container: new Element(),
    captureCurrentEditorFocus: () => null, restoreEditorFocusOnNextRender: false, matchesFilter: () => false,
    keepEditorTextFocus: () => undefined, setSoundTimePosition: (value: number) => seeks.push(value),
    setTimeout: schedule, setInterval: schedule,
    clearTimeout: (id: number) => timers.delete(id), clearInterval: (id: number) => timers.delete(id),
  });
  for (const name of ["createAudioPreviewRow", "makeNumberStepperButton", "render"]) {
    const fn = tree.statements.find(node => ts.isFunctionDeclaration(node) && node.name?.text === name);
    assert.ok(fn, `Missing production function ${name}`);
    vm.runInContext(fn.getText(tree), context);
  }
  return { context, document, window, timers, seeks, run: (code: string) => vm.runInContext(code, context) };
}

test("Sound rows do not retain document listeners; drag handlers end on release, blur and rerender", () => {
  const view = webview();
  for (let i = 0; i < 50; i++) { view.run("createAudioPreviewRow({})"); }
  assert.equal(view.document.count(), 0);
  for (const end of ["release", "blur", "render"]) {
    const row = view.run("createAudioPreviewRow({})") as Element;
    row.find("audio-scrubber-track")!.dispatch("mousedown", { button: 0, clientX: 25 });
    assert.equal(view.document.count(), 2);
    view.document.dispatch("mousemove", { clientX: 50 });
    assert.equal(view.seeks.at(-1), 30);
    if (end === "release") { view.document.dispatch("mouseup"); }
    else if (end === "blur") { view.window.dispatch("blur"); }
    else { view.run("render()"); }
    assert.equal(view.document.count() + view.window.count(), 0);
    assert.equal(view.context.audioScrubbing, false);
    const count = view.seeks.length;
    view.document.dispatch("mousemove", { clientX: 90 });
    assert.equal(view.seeks.length, count);
  }
});

test("number repeat has one owner and releases timers on cancellation, blur and rerender", () => {
  const view = webview();
  let steps = 0;
  view.context.step = (direction: number) => { steps += direction; };
  for (const end of ["pointerup", "pointercancel", "blur", "render"]) {
    const button = view.run('makeNumberStepperButton({}, step, 1, "up", "Increase")') as Element;
    button.dispatch("pointerdown", { preventDefault() {} });
    assert.equal(view.timers.size, 1);
    const [delay, callback] = [...view.timers][0];
    view.timers.delete(delay);
    callback();
    [...view.timers.values()][0]();
    if (end === "blur") { view.window.dispatch("blur"); }
    else if (end === "render") { view.run("render()"); }
    else { view.document.dispatch(end); }
    assert.equal(view.timers.size + view.document.count() + view.window.count(), 0);
    button.dispatch("keydown", { key: "Enter", preventDefault() {} });
  }
  assert.equal(steps, 12);
});
