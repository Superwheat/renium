import { iconAssetNameForClass } from "./fileExplorer";

export type ChangePreviewRow = {
  service: string;
  path: string;
  pathSegments: string[];
  pathOrdinals: number[];
  identity: string;
  leaf: string;
  className: string;
  icon: string;
  scope: string;
  property: string;
  status?: string;
  oldValue?: unknown;
  newValue?: unknown;
};

export function buildChangePreviewHtml(
  rows: ChangePreviewRow[],
  changeCount: number,
  threshold: number,
  assetBase: string,
  mode: "property" | "structural",
  iconNames: ReadonlySet<string>,
): string {
  const payload = JSON.stringify(rows).replace(/</g, "\\u003c");
  const instanceCount = new Set(rows.map((row) => `${row.service}\0${row.identity}`)).size;
  const services = [...new Set(rows.map((row) => row.service).filter((service) => service.length > 0))];
  const folderIcon = iconAssetNameForClass("Folder", iconNames);
  const serviceIcons = Object.fromEntries(
    services.map((service) => [service, iconAssetNameForClass(service, iconNames)]),
  );
  return `<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<style>
  :root {
    color-scheme: light dark;
    --ink: rgba(255,255,255,0.92);
    --ink-mid: rgba(255,255,255,0.60);
    --ink-dim: rgba(255,255,255,0.38);
    --surface: rgba(255,255,255,0.032);
    --surface-hover: rgba(255,255,255,0.055);
    --edge: rgba(255,255,255,0.085);
    --edge-soft: rgba(255,255,255,0.05);
    --amber: #e8b53f;
    --red: #f47f76;
    --green: #66c88e;
  }
  body.vscode-light, body.vscode-high-contrast-light {
    --ink: rgba(20,22,28,0.92);
    --ink-mid: rgba(20,22,28,0.62);
    --ink-dim: rgba(20,22,28,0.40);
    --surface: rgba(18,20,26,0.035);
    --surface-hover: rgba(18,20,26,0.06);
    --edge: rgba(18,20,26,0.12);
    --edge-soft: rgba(18,20,26,0.07);
    --amber: #b8860b;
    --red: #d0453a;
    --green: #1f8a4c;
  }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  ::-webkit-scrollbar { width: 9px; }
  ::-webkit-scrollbar-thumb { background: var(--edge); border-radius: 5px; border: 2px solid transparent; background-clip: padding-box; }
  ::-webkit-scrollbar-thumb:hover { background: var(--ink-dim); border: 2px solid transparent; background-clip: padding-box; }
  body {
    font-family: "Segoe UI Variable Text", "Inter", var(--vscode-font-family, "Segoe UI"), sans-serif;
    -webkit-font-smoothing: antialiased;
    font-size: 13px; line-height: 1.5;
    color: var(--ink);
    background: var(--vscode-editor-background, #17171a);
    display: flex; flex-direction: column; height: 100vh; overflow: hidden;
  }
  .header { padding: 26px 30px 20px; flex: none; }
  .kicker {
    display: flex; align-items: center; gap: 8px;
    font-size: 10px; font-weight: 700; letter-spacing: 0.16em; text-transform: uppercase;
    color: var(--ink-dim);
  }
  .kicker b { color: var(--ink-mid); font-weight: 700; }
  .pulse {
    width: 7px; height: 7px; border-radius: 50%;
    background: var(--amber);
    box-shadow: 0 0 0 0 color-mix(in srgb, var(--amber) 45%, transparent);
    animation: pulse 2.2s cubic-bezier(0.4, 0, 0.6, 1) infinite; flex: none;
  }
  @keyframes pulse {
    0% { box-shadow: 0 0 0 0 color-mix(in srgb, var(--amber) 45%, transparent); }
    70% { box-shadow: 0 0 0 7px transparent; }
    100% { box-shadow: 0 0 0 0 transparent; }
  }
  h1 { font-size: 19px; font-weight: 640; letter-spacing: -0.018em; margin-top: 10px; }
  .subtitle { margin-top: 4px; font-size: 12.5px; color: var(--ink-mid); max-width: 60ch; }
  .subtitle b { color: var(--ink); font-weight: 620; font-variant-numeric: tabular-nums; }
  .subtitle .threshold { color: var(--amber); font-weight: 620; font-variant-numeric: tabular-nums; }
  .toolbar { display: flex; align-items: center; gap: 10px; margin-top: 14px; }
  .filter {
    flex: none; width: 240px; font-family: inherit; font-size: 12px;
    color: var(--ink); background: var(--surface); border: 1px solid var(--edge);
    border-radius: 7px; padding: 5px 11px; outline: none;
    transition: border-color 0.12s ease, background 0.12s ease;
  }
  .filter:focus { border-color: var(--ink-dim); background: var(--surface-hover); }
  .filter::placeholder { color: var(--ink-dim); }
  .toolbar-hint { font-size: 11px; color: var(--ink-dim); }
  .list { flex: 1; overflow-y: auto; padding: 6px 22px 26px; position: relative; animation: rise 0.3s cubic-bezier(0.16, 1, 0.3, 1) both; }
  #sizer { position: relative; width: 100%; }
  #viewport { position: absolute; left: 0; right: 0; top: 0; }
  .row {
    display: flex; align-items: center; height: 26px; border-radius: 6px;
    padding-right: 10px; cursor: pointer; user-select: none; min-width: 0;
  }
  .row:hover { background: var(--surface-hover); }
  .twisty {
    width: 17px; flex: none; text-align: center; color: var(--ink-dim);
    font-size: 10px; line-height: 1; transition: transform 0.12s ease;
  }
  .twisty.open { transform: rotate(90deg); }
  .twisty.blank { visibility: hidden; }
  .icon {
    width: 16px; height: 16px; flex: none; margin-right: 6px;
    display: block; object-fit: contain; object-position: center center; image-rendering: pixelated;
  }
  .rname { font-size: 12.5px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .row.folder .rname { color: var(--ink-mid); }
  .rsep { color: var(--ink-dim); margin: 0 4px; font-size: 11px; }
  .count {
    margin-left: auto; flex: none; font-size: 10px; font-weight: 650;
    padding: 1px 8px; border-radius: 999px;
    background: var(--surface-hover); color: var(--ink-mid);
    font-variant-numeric: tabular-nums;
  }
  .prop-row {
    display: grid; grid-template-columns: minmax(120px, 190px) 1fr;
    gap: 16px; align-items: center; height: 26px; padding: 0 10px; border-radius: 6px;
  }
  .prop-row:hover { background: var(--surface-hover); }
  .prop-name-cell { display: flex; align-items: center; gap: 8px; min-width: 0; }
  .prop-name {
    font-family: "Cascadia Code", "JetBrains Mono", var(--vscode-editor-font-family, Consolas), monospace;
    font-size: 11.5px; font-weight: 450; color: var(--ink-mid);
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .scope-badge {
    font-size: 8px; font-weight: 750; text-transform: uppercase; letter-spacing: 0.1em;
    padding: 1px 6px; border-radius: 4px; flex: none;
    color: var(--ink-dim);
    background: var(--surface-hover);
  }
  .values { display: flex; align-items: center; gap: 8px; min-width: 0; font-variant-numeric: tabular-nums; }
  .val {
    font-family: "Cascadia Code", "JetBrains Mono", var(--vscode-editor-font-family, Consolas), monospace;
    font-size: 11.5px; padding: 2px 9px; border-radius: 6px;
    white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
  }
  .val.old {
    color: color-mix(in srgb, var(--red) 82%, var(--ink));
    background: color-mix(in srgb, var(--red) 9%, transparent);
    box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--red) 22%, transparent);
    text-decoration: line-through; text-decoration-thickness: 1px;
    text-decoration-color: color-mix(in srgb, var(--red) 55%, transparent);
    max-width: 42%; flex: none;
  }
  .val.new {
    color: color-mix(in srgb, var(--green) 85%, var(--ink));
    background: color-mix(in srgb, var(--green) 10%, transparent);
    box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--green) 24%, transparent);
  }
  .val.neutral {
    color: var(--ink-mid);
    background: var(--surface-hover);
    box-shadow: inset 0 0 0 1px var(--edge-soft);
  }
  .row.added .rname { color: color-mix(in srgb, var(--green) 70%, var(--ink)); }
  .row.removed .rname {
    color: color-mix(in srgb, var(--red) 70%, var(--ink));
    text-decoration: line-through;
    text-decoration-color: color-mix(in srgb, var(--red) 50%, transparent);
  }
  .row.removed .icon { opacity: 0.55; }
  .arrow { color: var(--ink-dim); flex: none; font-size: 11px; }
  .swatch { display: inline-block; width: 11px; height: 11px; border-radius: 3.5px; margin-right: 6px; vertical-align: -1px; box-shadow: inset 0 0 0 1px rgba(128,128,128,0.4); }
  .footer {
    flex: none; display: flex; align-items: center; gap: 18px;
    padding: 15px 30px; border-top: 1px solid var(--edge-soft);
    background: color-mix(in srgb, var(--vscode-editor-background, #17171a) 72%, transparent);
    backdrop-filter: blur(14px);
  }
  .countdown { font-size: 11.5px; color: var(--ink-dim); flex: 1; min-width: 0; }
  .countdown b { color: var(--ink-mid); font-weight: 620; font-variant-numeric: tabular-nums; }
  .countdown-bar { height: 2px; border-radius: 2px; background: var(--edge); margin-top: 8px; overflow: hidden; }
  .countdown-fill { height: 100%; width: 100%; background: var(--green); transition: width 1s linear, background 1s linear; border-radius: 2px; }
  button {
    font-family: inherit; font-size: 12.5px; font-weight: 590; letter-spacing: 0.005em;
    padding: 8px 18px; border-radius: 8px;
    border: 1px solid transparent; cursor: pointer; flex: none;
    transition: transform 0.1s ease, box-shadow 0.15s ease, background 0.15s ease, color 0.15s ease;
  }
  button:active { transform: translateY(1px) scale(0.98); }
  .apply { background: #2e9e5b; color: #fff; }
  .apply:hover { background: #35b268; }
  body.vscode-light .apply, body.vscode-high-contrast-light .apply { background: #1f8a4c; }
  body.vscode-light .apply:hover, body.vscode-high-contrast-light .apply:hover { background: #23994f; }
  .full { background: var(--surface-hover); color: var(--ink); border-color: var(--edge); }
  .full:hover { background: var(--edge); }
  .skip { background: transparent; font-weight: 480; color: var(--ink-dim); }
  .skip:hover { color: var(--red); }
  @keyframes rise { from { opacity: 0; transform: translateY(10px); } to { opacity: 1; transform: none; } }
</style>
</head>
<body>
  <div class="header">
    <div class="kicker"><div class="pulse"></div><span><b>Renium</b>&ensp;&middot;&ensp;Live sync paused</span></div>
    <h1>Studio changes awaiting review</h1>
    <div class="subtitle"><b>${changeCount}</b> change${changeCount === 1 ? "" : "s"} across <b>${instanceCount}</b> instance${instanceCount === 1 ? "" : "s"} in ${services.join(", ") || "your project"}. This batch is over your review threshold of <span class="threshold">${threshold}</span>.</div>
    <div class="toolbar">
      <input class="filter" id="filter" type="text" placeholder="Filter by name, class, or property" spellcheck="false">
      <span class="toolbar-hint" id="toolbar-hint"></span>
    </div>
  </div>
  <div class="list" id="list"><div id="sizer"><div id="viewport"></div></div></div>
  <div class="footer">
    <div class="countdown">
      <span id="count-label">Protected full import in <b id="secs">90</b>s &mdash; hover the list to pause</span>
      <div class="countdown-bar"><div class="countdown-fill" id="fill"></div></div>
    </div>
    <button class="skip" id="skip" title="Acknowledge without touching editor files">Skip batch</button>
    ${mode === "structural"
      ? '<button class="apply" id="full" title="Re-export and import everything that differs">Import</button>'
      : '<button class="full" id="full" title="Safest: re-export and import everything that differs">Full import</button>\n    <button class="apply" id="apply" title="Write exactly these changes to the editor files">Apply changes</button>'}
  </div>
<script>
  const vscode = acquireVsCodeApi();
  const DATA = ${payload};
  const ASSET = ${JSON.stringify(assetBase)};
  const SERVICE_ICONS = ${JSON.stringify(serviceIcons)};
  const FOLDER_ICON = ${JSON.stringify(folderIcon)};

  function esc(text) {
    return String(text).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
  }
  function fmtNum(n) {
    if (!isFinite(n)) return String(n);
    return Math.abs(n) >= 1e6 || (Math.abs(n) < 1e-4 && n !== 0) ? n.toExponential(3) : String(Math.round(n * 1000) / 1000);
  }
  function fmt(value) {
    if (value === undefined) return null;
    if (value === null) return '<i>nil</i>';
    const t = typeof value;
    if (t === "boolean") return String(value);
    if (t === "number") return esc(fmtNum(value));
    if (t === "string") return '"' + esc(value.length > 90 ? value.slice(0, 90) + "\\u2026" : value) + '"';
    if (t === "object") {
      const k = value._type;
      if (k === "Color3") {
        const r = Math.round((value.r ?? 0) * 255), g = Math.round((value.g ?? 0) * 255), b = Math.round((value.b ?? 0) * 255);
        return '<span class="swatch" style="background:rgb(' + r + "," + g + "," + b + ')"></span>' + r + ", " + g + ", " + b;
      }
      if (k === "Vector3") return esc(fmtNum(value.x ?? 0) + ", " + fmtNum(value.y ?? 0) + ", " + fmtNum(value.z ?? 0));
      if (k === "Vector2") return esc(fmtNum(value.x ?? 0) + ", " + fmtNum(value.y ?? 0));
      if (k === "EnumItem") return esc(String(value.value ?? value.name ?? "Enum"));
      if (k === "Float") return esc(String(value.value));
      if (k === "CFrame") return "CFrame (" + ((value.components || []).slice(0, 3).map(fmtNum).join(", ") || "\\u2026") + ", \\u2026)";
      if (k === "Ref" || value.Ref) return "\\u2192 " + esc(String((value.Ref || value).settingsId ?? (value.Ref || value).instanceId ?? "instance"));
      const json = JSON.stringify(value);
      return esc(json.length > 90 ? json.slice(0, 90) + "\\u2026" : json);
    }
    return esc(String(value));
  }

  const root = { children: new Map(), changes: null, icon: null, className: "", status: null };
  for (const row of DATA) {
    const segments = Array.isArray(row.pathSegments) && row.pathSegments.length > 0
      ? row.pathSegments
      : [row.leaf];
    const ordinals = Array.isArray(row.pathOrdinals) ? row.pathOrdinals : [];
    let node = root;
    for (let index = 0; index < segments.length; index += 1) {
      const segment = segments[index];
      const ordinal = Math.max(1, Number(ordinals[index]) || 1);
      const segmentKey = JSON.stringify([segment, ordinal]);
      if (!node.children.has(segmentKey)) {
        node.children.set(segmentKey, {
          name: segment,
          ordinal,
          children: new Map(),
          changes: null,
          icon: null,
          className: "",
          status: null,
        });
      }
      node = node.children.get(segmentKey);
    }
    if (row.scope === "__status") {
      node.status = row.status;
      node.icon = node.icon || row.icon;
      node.className = node.className || row.className;
      if (!node.changes) node.changes = [];
      continue;
    }
    if (!node.changes) {
      node.changes = [];
      node.icon = row.icon;
      node.className = row.className;
    }
    node.changes.push(row);
  }

  const list = document.getElementById("list");
  const sizer = document.getElementById("sizer");
  const viewport = document.getElementById("viewport");
  const filterInput = document.getElementById("filter");
  const hintEl = document.getElementById("toolbar-hint");
  const ROW_HEIGHT = 26;
  const OVERSCAN = 20;
  const instanceTotal = ${instanceCount};
  const collapsed = new Set();
  const propsOpen = new Set();
  const autoOpenProps = instanceTotal <= 12;
  let filterText = "";
  let flat = [];
  let renderFrame = 0;
  let lastStart = -1;
  let lastCount = -1;

  const matchCache = new Map();
  function childKey(child) {
    return JSON.stringify([child.name, Math.max(1, Number(child.ordinal) || 1)]);
  }
  function nodeMatches(node, pathKey) {
    if (!filterText) return true;
    const cached = matchCache.get(pathKey);
    if (cached !== undefined) return cached;
    let out = node.name.toLowerCase().includes(filterText)
      || (node.className && node.className.toLowerCase().includes(filterText))
      || (node.changes && node.changes.some((c) => c.property.toLowerCase().includes(filterText)));
    if (!out) {
      for (const child of node.children.values()) {
        if (nodeMatches(child, pathKey + "/" + childKey(child))) { out = true; break; }
      }
    }
    matchCache.set(pathKey, out);
    return out;
  }

  function flattenNode(node, pathKey, depth) {
    if (!nodeMatches(node, pathKey)) return;
    let chain = [node.name];
    let current = node;
    let key = pathKey;
    while (!current.changes && current.children.size === 1 && !filterText) {
      const child = current.children.values().next().value;
      chain.push(child.name);
      key = key + "/" + childKey(child);
      current = child;
    }
    const isFolder = !current.changes;
    const hasKids = current.children.size > 0;
    const propCount = current.changes ? current.changes.length : 0;
    const isCollapsed = collapsed.has(key);
    const propsShown = propCount > 0 && !isCollapsed && (propsOpen.has(key) || autoOpenProps || !!filterText);
    flat.push({ kind: "node", key, chain, depth, isFolder, hasKids, propCount, isCollapsed, propsShown,
      status: current.status,
      className: current.className, icon: isFolder ? (depth === 0 ? (SERVICE_ICONS[chain[0]] || FOLDER_ICON) : FOLDER_ICON) : current.icon });
    if (isCollapsed) return;
    if (propsShown) {
      for (const change of current.changes) {
        flat.push({ kind: "prop", depth, change, neutral: current.status === "added" });
      }
    }
    const children = [...current.children.values()].sort((a, b) => a.name.localeCompare(b.name, undefined, { numeric: true }));
    for (const child of children) {
      flattenNode(child, key + "/" + childKey(child), depth + 1);
    }
  }

  function rebuildFlat() {
    flat = [];
    matchCache.clear();
    for (const service of root.children.values()) {
      flattenNode(service, childKey(service), 0);
    }
    sizer.style.height = (flat.length * ROW_HEIGHT) + "px";
    hintEl.textContent = filterText && flat.length === 0 ? "No changes match" : "";
    lastStart = -1;
    renderWindow();
  }

  function nodeRowHtml(item) {
    const expandable = item.hasKids || item.propCount > 0;
    const open = !(item.isCollapsed || (item.propCount > 0 && !item.propsShown && !item.hasKids));
    const statusClass = item.status === "added" ? " added" : item.status === "removed" ? " removed" : "";
    const statusTitle = item.status === "added" ? "Added in Studio" : item.status === "removed" ? "Removed in Studio" : (item.className || "");
    return '<div class="row' + (item.isFolder ? " folder" : "") + statusClass + '" data-key="' + esc(item.key) + '" title="' + esc(statusTitle) + '" style="padding-left:' + (item.depth * 14 + 6) + 'px">' +
      '<span class="twisty' + (open ? " open" : "") + (expandable ? "" : " blank") + '">\\u25B8</span>' +
      '<img class="icon" src="' + ASSET + "/" + esc(item.icon || "Folder") + '.png">' +
      '<span class="rname">' + item.chain.map(esc).join('<span class="rsep">\\u203A</span>') + "</span>" +
      (item.propCount > 0 ? '<span class="count">' + item.propCount + "</span>" : "") +
      "</div>";
  }

  function propRowHtml(item) {
    const row = item.change;
    const oldHtml = item.neutral ? null : fmt(row.oldValue);
    const neutral = item.neutral || oldHtml === null;
    const scopeBadge = row.scope !== "property" ? '<span class="scope-badge">' + esc(row.scope) + "</span>" : "";
    return '<div class="prop-row" style="margin-left:' + (item.depth * 14 + 23) + 'px">' +
      '<span class="prop-name-cell"><span class="prop-name">' + esc(row.property) + "</span>" + scopeBadge + "</span>" +
      '<span class="values">' + (oldHtml !== null ? '<span class="val old">' + oldHtml + '</span><span class="arrow">\\u2192</span>' : "") +
      '<span class="val ' + (neutral ? "neutral" : "new") + '">' + fmt(row.newValue) + "</span></span></div>";
  }

  function renderWindow() {
    const start = Math.max(0, Math.floor(list.scrollTop / ROW_HEIGHT) - OVERSCAN);
    const count = Math.min(flat.length - start, Math.ceil((list.clientHeight || 400) / ROW_HEIGHT) + OVERSCAN * 2);
    if (start === lastStart && count === lastCount) return;
    lastStart = start;
    lastCount = count;
    const parts = [];
    for (let i = start; i < start + count; i++) {
      const item = flat[i];
      parts.push(item.kind === "node" ? nodeRowHtml(item) : propRowHtml(item));
    }
    viewport.style.top = (start * ROW_HEIGHT) + "px";
    viewport.innerHTML = parts.join("");
  }

  list.addEventListener("scroll", () => {
    if (renderFrame) return;
    renderFrame = requestAnimationFrame(() => { renderFrame = 0; renderWindow(); });
  });

  viewport.addEventListener("click", (event) => {
    const row = event.target.closest(".row");
    if (!row) return;
    const key = row.dataset.key;
    const item = flat.find((entry) => entry.kind === "node" && entry.key === key);
    if (!item || !(item.hasKids || item.propCount > 0)) return;
    if (item.isFolder || item.hasKids) {
      if (collapsed.has(key)) collapsed.delete(key); else collapsed.add(key);
    }
    if (item.propCount > 0 && !item.hasKids) {
      if (item.propsShown) { propsOpen.delete(key); if (autoOpenProps || filterText) collapsed.add(key); }
      else { propsOpen.add(key); collapsed.delete(key); }
    }
    rebuildFlat();
  });

  filterInput.addEventListener("input", () => {
    filterText = filterInput.value.trim().toLowerCase();
    rebuildFlat();
  });
  filterInput.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { filterInput.value = ""; filterText = ""; rebuildFlat(); }
  });
  rebuildFlat();

  let secs = 90;
  let paused = false;
  const secsEl = document.getElementById("secs");
  const fillEl = document.getElementById("fill");
  const labelEl = document.getElementById("count-label");
  list.addEventListener("mouseenter", () => { paused = true; labelEl.innerHTML = "Auto import paused while reviewing"; });
  list.addEventListener("mouseleave", () => {
    paused = false;
    labelEl.innerHTML = 'Protected full import in <b id="secs">' + secs + "</b>s &mdash; hover the list to pause";
  });
  const timer = setInterval(() => {
    if (paused) return;
    secs -= 1;
    const liveSecs = document.getElementById("secs");
    if (liveSecs) liveSecs.textContent = String(secs);
    fillEl.style.width = (secs / 90 * 100) + "%";
    fillEl.style.background = "hsl(" + Math.round(120 * secs / 90) + ", 55%, 45%)";
    if (secs <= 0) { clearInterval(timer); vscode.postMessage({ action: "full" }); }
  }, 1000);

  const applyButton = document.getElementById("apply");
  if (applyButton) applyButton.addEventListener("click", () => vscode.postMessage({ action: "apply" }));
  document.getElementById("full").addEventListener("click", () => vscode.postMessage({ action: "full" }));
  document.getElementById("skip").addEventListener("click", () => vscode.postMessage({ action: "discard" }));
</script>
</body>
</html>`;
}
