
import * as vscode from "vscode";
import { loadAssetIconNames } from "./fileExplorer";
import { decodeRbsyncToTree } from "./rbsyncDecode";


export type ReniumCliResolver = () => { cliPath: string; cwd: string } | undefined;



export type RbsyncSelectHandler = (node: {
  name?: string;
  className?: string;
  settingsId?: string;
  properties?: Record<string, unknown>;
  attributes?: Record<string, unknown>;
}) => void;

export class RbsyncEditorProvider implements vscode.CustomReadonlyEditorProvider {
  public static readonly viewType = "renium.reniumEditor";
  private readonly iconNames: string[];

  constructor(
    private readonly extensionUri: vscode.Uri,
    private readonly resolveCli: ReniumCliResolver,
    private readonly onSelect: RbsyncSelectHandler,
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
    webview.html = rbsyncEditorHtml(assetBase, this.iconNames);

    let revealedProperties = false;
    let decodeStarted = false;
    webview.onDidReceiveMessage((message: unknown) => {
      if (!message || typeof message !== "object") {
        return;
      }
      const msg = message as { type?: string; node?: Parameters<RbsyncSelectHandler>[0] };
      if (msg.type === "ready") {
        if (!decodeStarted) {
          decodeStarted = true;
          void this.decodeAndPost(webview, document.uri.fsPath);
        }
      } else if (msg.type === "select" && msg.node) {
        if (!revealedProperties) {
          revealedProperties = true;
          void vscode.commands.executeCommand("workbench.view.extension.reniumContainer");
        }
        this.onSelect(msg.node);
      }
    });
  }

  private async decodeAndPost(webview: vscode.Webview, filePath: string): Promise<void> {
    const cli = this.resolveCli();
    if (!cli) {
      void webview.postMessage({
        type: "error",
        message: "Could not locate renium.exe. Set renium.exportCliPath or build the CLI.",
      });
      return;
    }
    const result = await decodeRbsyncToTree(cli.cliPath, cli.cwd, filePath);
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



function rbsyncEditorHtml(assetBase: string, iconNames: string[]): string {
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
var tree=null,expanded={},selId=null,byId={};
var query='',searchCollapsed={},flat=[],sizer=null,rowsEl=null,paintQueued=false;
var ROW_H=22,OVER=6;
var treeEl=document.getElementById('tree'),searchEl=document.getElementById('search');
function esc(s){return String(s==null?'':s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;')}
function iconName(c){if(AVAILABLE_ICONS.has(c))return c;var f=c&&c.slice(-7)==='Service'?'Service':'Class';return AVAILABLE_ICONS.has(f)?f:c}
function index(){byId={};function w(n){byId[n.settingsId]=n;if(n.children)for(var i=0;i<n.children.length;i++)w(n.children[i])}var r=(tree&&tree.roots)||[];for(var i=0;i<r.length;i++)w(r[i])}
function hi(name){
  var s=String(name==null?'':name);
  if(!query)return esc(s);
  var i=s.toLowerCase().indexOf(query);
  if(i<0)return esc(s);
  return esc(s.slice(0,i))+'<span class="rbhi">'+esc(s.slice(i,i+query.length))+'</span>'+esc(s.slice(i+query.length));
}
function rowHtml(f){
  var n=f.n;
  var h='<div class="row'+(selId===n.settingsId?' selected':'')+(f.match?' rbmatch':'')+'" data-id="'+esc(n.settingsId)+'" style="padding-left:'+(f.depth*12+4)+'px">';
  h+='<span class="twisty '+(f.has?(f.open?'open':''):'leaf')+'"></span>';
  h+='<img class="icon" src="'+ASSET+'/'+esc(iconName(n.className))+'.png">';
  h+='<span class="labelWrap"><span class="name">'+(f.match?hi(n.name):esc(n.name))+'</span></span></div>';
  return h;
}
function buildFlat(){
  flat=[];
  if(!tree)return;
  var roots=tree.roots||[];
  if(!query){
    (function walk(list,depth){
      for(var i=0;i<list.length;i++){
        var n=list[i],kids=n.children||[],open=!!expanded[n.settingsId];
        flat.push({n:n,depth:depth,has:kids.length>0,open:open,match:false});
        if(kids.length&&open)walk(kids,depth+1);
      }
    })(roots,0);
    return;
  }
  var inc={};
  function mark(n){
    var kids=n.children||[],any=false;
    for(var i=0;i<kids.length;i++){if(mark(kids[i]))any=true;}
    var self=(String(n.name)+' '+String(n.className)).toLowerCase().indexOf(query)>=0;
    if(self||any){inc[n.settingsId]=self?2:1;return true;}
    return false;
  }
  for(var i=0;i<roots.length;i++)mark(roots[i]);
  (function walk(list,depth){
    for(var j=0;j<list.length;j++){
      var n=list[j],flag=inc[n.settingsId];
      if(!flag)continue;
      var kids=n.children||[],open=!searchCollapsed[n.settingsId];
      flat.push({n:n,depth:depth,has:kids.length>0,open:open,match:flag===2});
      if(kids.length&&open)walk(kids,depth+1);
    }
  })(roots,0);
}
function ensureShell(){
  if(sizer&&sizer.parentNode===treeEl)return;
  treeEl.innerHTML='';
  sizer=document.createElement('div');sizer.className='rbSizer';
  rowsEl=document.createElement('div');rowsEl.className='rbRows';
  sizer.appendChild(rowsEl);treeEl.appendChild(sizer);
}
function paint(){
  if(!tree||!flat.length)return;
  ensureShell();
  var total=flat.length;
  sizer.style.height=(total*ROW_H)+'px';
  var vh=treeEl.clientHeight||400,scrollTop=treeEl.scrollTop;
  var start=Math.max(0,Math.floor(scrollTop/ROW_H)-OVER);
  var end=Math.min(total,Math.ceil((scrollTop+vh)/ROW_H)+OVER);
  var out=[];for(var i=start;i<end;i++)out.push(rowHtml(flat[i]));
  rowsEl.style.transform='translateY('+(start*ROW_H)+'px)';
  rowsEl.innerHTML=out.join('');
}
function schedulePaint(){
  if(paintQueued)return;
  paintQueued=true;
  requestAnimationFrame(function(){paintQueued=false;paint()});
}
function render(){
  if(!tree||!((tree.roots||[]).length)){sizer=null;rowsEl=null;treeEl.innerHTML='<div class="hint">This store has no instances.</div>';return}
  buildFlat();
  if(!flat.length){sizer=null;rowsEl=null;treeEl.innerHTML='<div class="hint">No matches.</div>';return}
  paint();
}
function select(id){
  selId=id;var n=byId[id];paint();
  if(n)vscode.postMessage({type:'select',node:{name:n.name,className:n.className,settingsId:n.settingsId,properties:n.properties||{},attributes:n.attributes||{}}});
}
treeEl.addEventListener('scroll',schedulePaint);
treeEl.addEventListener('click',function(e){
  var row=e.target.closest('.row');if(!row)return;
  var id=row.dataset.id,n=byId[id];if(!n)return;
  var tw=e.target.closest('.twisty');
  if(tw&&!tw.classList.contains('leaf')){
    if(query)searchCollapsed[id]=!searchCollapsed[id];
    else expanded[id]=!expanded[id];
    render();return;
  }
  select(id);
});
function search(term){
  if(!tree)return;
  var q=(term||'').trim().toLowerCase();
  if(q===query)return;
  query=q;searchCollapsed={};treeEl.scrollTop=0;
  render();
}
searchEl.addEventListener('input',function(){search(searchEl.value)});
window.addEventListener('resize',schedulePaint);
window.addEventListener('message',function(e){
  var m=e.data||{};
  if(m.type==='tree'){
    tree=m.result;expanded={};selId=null;query='';searchCollapsed={};searchEl.value='';treeEl.scrollTop=0;index();
    var big=tree&&tree.instanceCount>800;
    (function w(l,depth){for(var i=0;i<l.length;i++){var n=l[i];if(n.children&&n.children.length&&(!big||depth===0))expanded[n.settingsId]=true;if(n.children)w(n.children,depth+1)}})((tree&&tree.roots)||[],0);
    searchEl.placeholder=tree?('Search '+tree.instanceCount+' instances'):'Search';
    render();
    if(flat.length)select(flat[0].n.settingsId);
  }else if(m.type==='error'){
    sizer=null;rowsEl=null;treeEl.innerHTML='';var d=document.createElement('div');d.className='err';d.textContent=m.message||'Could not read this file.';treeEl.appendChild(d);
  }
});
vscode.postMessage({type:'ready'});
</script>
</body>
</html>`;
}
