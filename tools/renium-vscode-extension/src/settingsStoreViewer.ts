import * as path from "path";
import * as vscode from "vscode";
import { loadAssetIconNames } from "./fileExplorer";
import { decodeSettingsStoreToTree } from "./settingsStoreDecode";
import { settingsStoreTreeRuntime } from "./settingsStoreTreeWebview";

type ReniumCliResolver = () => { cliPath: string; cwd: string } | undefined;

type SettingsStoreSelectHandler = (node: {
  name?: string;
  className?: string;
  settingsId?: string;
  properties?: Record<string, unknown>;
  attributes?: Record<string, unknown>;
}) => void;

export class SettingsStoreEditorProvider implements vscode.CustomReadonlyEditorProvider {
  public static readonly viewType = "renium.reniumEditor";
  private readonly iconNames: string[];

  constructor(
    private readonly extensionUri: vscode.Uri,
    private readonly resolveCli: ReniumCliResolver,
    private readonly onSelect: SettingsStoreSelectHandler,
  ) {
    this.iconNames = loadAssetIconNames(extensionUri);
  }

  public openCustomDocument(uri: vscode.Uri): vscode.CustomDocument {
    return { uri, dispose: () => undefined };
  }

  public resolveCustomEditor(document: vscode.CustomDocument, panel: vscode.WebviewPanel): void {
    const webview = panel.webview;
    webview.options = {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(this.extensionUri, "assets")],
    };
    const assetBase = webview.asWebviewUri(vscode.Uri.joinPath(this.extensionUri, "assets")).toString();
    webview.html = storeEditorHtml(assetBase, this.iconNames);

    let revealedProperties = false;
    let ready = false;
    let disposed = false;
    let decodeRunning = false;
    let decodeDirty = false;
    let decodeAbort: AbortController | undefined;
    let decodeTimer: NodeJS.Timeout | undefined;
    const filePath = path.normalize(document.uri.fsPath);
    const watcher = vscode.workspace.createFileSystemWatcher(
      new vscode.RelativePattern(path.dirname(filePath), path.basename(filePath)),
    );
    const decode = async (): Promise<void> => {
      if (!ready || disposed || decodeRunning) {
        return;
      }
      decodeRunning = true;
      try {
        while (ready && !disposed && decodeDirty) {
          decodeDirty = false;
          const abort = new AbortController();
          decodeAbort = abort;
          await this.decodeAndPost(webview, filePath, () => !disposed && !abort.signal.aborted, abort.signal);
          if (decodeAbort === abort) {
            decodeAbort = undefined;
          }
        }
      } finally {
        decodeRunning = false;
      }
    };
    const scheduleDecode = (): void => {
      decodeDirty = true;
      if (decodeTimer) {
        clearTimeout(decodeTimer);
      }
      decodeTimer = setTimeout(() => {
        decodeTimer = undefined;
        void decode();
      }, 75);
    };
    const sameFile = (uri: vscode.Uri): boolean => path.normalize(uri.fsPath) === filePath;
    watcher.onDidCreate((uri) => {
      if (sameFile(uri)) {
        scheduleDecode();
      }
    });
    watcher.onDidChange((uri) => {
      if (sameFile(uri)) {
        scheduleDecode();
      }
    });
    watcher.onDidDelete((uri) => {
      if (!sameFile(uri)) {
        return;
      }
      decodeDirty = false;
      decodeAbort?.abort();
      void webview.postMessage({ type: "error", message: "This file was deleted." });
    });
    const messageSubscription = webview.onDidReceiveMessage((message: unknown) => {
      if (!message || typeof message !== "object") {
        return;
      }
      const msg = message as { type?: string; node?: Parameters<SettingsStoreSelectHandler>[0] };
      if (msg.type === "ready") {
        if (!ready) {
          ready = true;
          scheduleDecode();
        }
      } else if (msg.type === "select" && msg.node) {
        if (!revealedProperties) {
          revealedProperties = true;
          void vscode.commands.executeCommand("workbench.view.extension.reniumContainer");
        }
        this.onSelect(msg.node);
      }
    });
    panel.onDidDispose(() => {
      disposed = true;
      decodeDirty = false;
      decodeAbort?.abort();
      if (decodeTimer) {
        clearTimeout(decodeTimer);
        decodeTimer = undefined;
      }
      watcher.dispose();
      messageSubscription.dispose();
    });
  }

  private async decodeAndPost(
    webview: vscode.Webview,
    filePath: string,
    isCurrent: () => boolean,
    signal: AbortSignal,
  ): Promise<void> {
    const cli = this.resolveCli();
    if (!cli) {
      if (isCurrent()) {
        void webview.postMessage({
        type: "error",
        message: "Could not locate renium.exe. Set renium.cliPath or build the CLI.",
        });
      }
      return;
    }
    const result = await decodeSettingsStoreToTree(cli.cliPath, cli.cwd, filePath, { signal });
    if (!isCurrent()) {
      return;
    }
    if (result.ok) {
      void webview.postMessage({ type: "tree", result: result.tree });
    } else {
      void webview.postMessage({ type: "error", message: result.error });
    }
  }
}

function jsonForScript(value: unknown): string {
  return JSON.stringify(value) ?? "null";
}

function storeEditorHtml(assetBase: string, iconNames: string[]): string {
  return `<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<style>
html,body{height:100%;margin:0;overflow:hidden;font-family:var(--vscode-font-family);font-size:var(--vscode-font-size);color:var(--vscode-foreground);background:var(--vscode-editor-background)}
body{display:flex;flex-direction:column}
#bar{height:34px;flex:0 0 auto;display:flex;align-items:center;padding:5px 8px;border-bottom:1px solid var(--vscode-panel-border)}
#search{flex:1;min-width:0;height:24px;border:1px solid var(--vscode-input-border,transparent);background:var(--vscode-input-background);color:var(--vscode-input-foreground);padding:2px 6px;font:inherit}
#tree{flex:1 1 auto;overflow:auto;padding:0;min-height:0;outline:none;position:relative}
.rbSizer{position:relative;width:100%}
.rbRows{position:absolute;left:0;right:0;top:0;will-change:transform}
.rbhi{background:var(--vscode-editor-findMatchHighlightBackground,rgba(234,140,0,.35));color:inherit;border-radius:2px}
.row{height:22px;display:flex;align-items:center;white-space:nowrap;cursor:pointer;user-select:none;line-height:22px;border-radius:3px}
.row:hover{background:var(--vscode-list-hoverBackground)}
.row.selected{background:var(--vscode-list-activeSelectionBackground);color:var(--vscode-list-activeSelectionForeground)}
.twisty{width:16px;height:22px;display:flex;align-items:center;justify-content:center;font-size:9px;color:var(--vscode-icon-foreground)}
.twisty::before{content:'\\25B6'}
.twisty.open::before{transform:rotate(90deg)}
.twisty.leaf{visibility:hidden}
.icon{width:16px;height:16px;flex:0 0 16px;margin-right:4px;display:block;object-fit:contain}
.labelWrap{display:inline-flex;align-items:center;min-width:0;flex:1 1 auto;overflow:hidden}
.name{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.hint{padding:16px;color:var(--vscode-descriptionForeground)}
.err{padding:14px;color:var(--vscode-errorForeground);white-space:pre-wrap}
</style>
</head>
<body>
<div id="bar"><input id="search" placeholder="Search" spellcheck="false"></div>
<div id="tree" tabindex="0"><div class="hint">Loading...</div></div>
<script>
var vscode=acquireVsCodeApi();
var ASSET=${jsonForScript(assetBase)},AVAILABLE_ICONS=new Set(${jsonForScript(iconNames)});
var treeEl=document.getElementById('tree'),searchEl=document.getElementById('search');
${settingsStoreTreeRuntime}
var storeBrowser=createSettingsStoreTree({
  treeElement:treeEl,
  searchElement:searchEl,
  assetBase:ASSET,
  iconNames:AVAILABLE_ICONS,
  rowPadding:4,
  fallbackHeight:400,
  emptyClass:'hint',
  errorClass:'err',
  emptyHtml:'<div class="hint">This store has no instances.</div>',
  onSelect:function(node){vscode.postMessage({type:'select',node:{name:node.name,className:node.className,settingsId:node.settingsId,properties:node.properties||{},attributes:node.attributes||{}}})}
});
window.addEventListener('message',function(e){
  var m=e.data||{};
  if(m.type==='tree'){
    storeBrowser.setTree(m.result);
  }else if(m.type==='error'){
    storeBrowser.setError(m.message||'Could not read this file.');
  }
});
vscode.postMessage({type:'ready'});
</script>
</body>
</html>`;
}
