import { settingsStoreTreeRuntime } from "./settingsStoreTreeWebview";

type FileExplorerWebviewOptions = {
  assetBase: string;
  classNames: readonly string[];
  availableIconNames: ReadonlySet<string>;
  initialRows: string;
  maxStoreDroppedBytes: number;
};

export function fileExplorerWebviewHtml({
  assetBase,
  classNames,
  availableIconNames,
  initialRows,
  maxStoreDroppedBytes,
}: FileExplorerWebviewOptions): string {
  const classNamesJson = JSON.stringify(classNames);
  const assetIconNamesJson = JSON.stringify(Array.from(availableIconNames));
  return `<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src *; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
<style>
*{box-sizing:border-box}
:root{--property-editor-focus-border:rgba(128,128,128,.45)}
html,body{height:100%;margin:0;overflow:hidden;font-family:var(--vscode-font-family);font-size:var(--vscode-font-size);color:var(--vscode-sideBar-foreground);background:var(--vscode-sideBar-background)}
body{position:relative;display:flex;flex-direction:column}
#tabs{height:30px;display:flex;align-items:end;justify-content:center;padding:3px 4px 0;border-bottom:1px solid var(--vscode-sideBarSectionHeader-border,transparent);gap:2px}
.tabBtn{height:26px;flex:1 1 0;min-width:0;max-width:110px;box-sizing:border-box;border:0;background:transparent;color:var(--vscode-descriptionForeground);padding:0 6px;cursor:pointer;font:inherit;border-radius:3px 3px 0 0;text-align:center;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.tabBtn:hover{background:var(--vscode-toolbar-hoverBackground,var(--vscode-list-hoverBackground));color:var(--vscode-foreground)}
.tabBtn.active{background:var(--vscode-list-activeSelectionBackground);color:var(--vscode-list-activeSelectionForeground)}
#explorerPane,#historyPane,#gitPane{flex:1;min-height:0;display:flex;flex-direction:column}
#explorerPane{position:relative}
.hidden{display:none!important}
#bar{height:30px;display:flex;align-items:center;gap:6px;padding:4px;border-bottom:1px solid var(--vscode-sideBarSectionHeader-border,transparent)}
#search{flex:1;min-width:0;height:22px;border:1px solid var(--vscode-input-border,transparent);background:var(--vscode-input-background);color:var(--vscode-input-foreground);padding:2px 5px;font:inherit}
#searchMeta{height:24px;display:none;align-items:center;border-bottom:1px solid var(--vscode-sideBarSectionHeader-border,transparent);padding:2px 4px;color:var(--vscode-descriptionForeground)}
#searchMeta.active{display:flex}
.searchSummary{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.searchActions{display:flex;align-items:center;gap:2px}
.iconBtn{width:22px;height:20px;border:0;background:transparent;color:var(--vscode-icon-foreground);padding:0;cursor:pointer;font:inherit}
.iconBtn:hover{background:var(--vscode-toolbar-hoverBackground,var(--vscode-list-hoverBackground))}
#suggestions{display:none;position:absolute;z-index:5357;top:30px;left:0;right:0;max-height:min(320px,calc(100% - 30px));overflow:auto;border:1px solid var(--vscode-sideBarSectionHeader-border,transparent);border-top:0;background:var(--vscode-sideBar-background);padding:7px 12px 8px 12px;color:var(--vscode-foreground);box-shadow:0 8px 18px rgba(0,0,0,.22)}
#suggestions.active{display:block}
.suggestTitle{color:var(--vscode-descriptionForeground);margin-bottom:6px}
.suggestItem{height:22px;display:flex;align-items:center;gap:8px;white-space:nowrap;cursor:pointer;margin:0 -6px;padding:0 6px;border-radius:2px;transition:background-color .1s ease,color .1s ease}
.suggestItem:hover{background:var(--vscode-list-hoverBackground);background:color-mix(in srgb,var(--vscode-list-hoverBackground) 72%,white 18%);color:var(--vscode-foreground)}
.suggestItem:hover .suggestIcon{color:var(--vscode-foreground)}
.suggestIcon{width:16px;text-align:center;color:var(--vscode-descriptionForeground);transition:color .1s ease}
#tree{flex:1;min-height:0;overflow:auto;outline:none;padding:2px 0}
#treeEmpty{padding:8px;color:var(--vscode-descriptionForeground)}
.row{height:22px;display:flex;align-items:center;white-space:nowrap;cursor:pointer;user-select:none;outline:1px solid transparent;line-height:22px}
.row:hover{background:var(--vscode-list-hoverBackground)}
.row.selected{background:var(--vscode-list-activeSelectionBackground);color:var(--vscode-list-activeSelectionForeground)}
.row.match-selected:not(.selected){background:var(--vscode-list-inactiveSelectionBackground)}
.row.reference-preview:not(.selected){background:var(--vscode-list-inactiveSelectionBackground,var(--vscode-list-hoverBackground));box-shadow:inset 0 0 0 1px var(--property-editor-focus-border)}
.row.disabled .name,.row.disabled .class,.row.disabled .icon{opacity:.45}
.row.dragging{opacity:.45}
.row.drop-target{outline:2px solid var(--vscode-focusBorder);outline-offset:-2px;background:var(--vscode-list-dropBackground,var(--vscode-list-hoverBackground));box-shadow:inset 4px 0 0 var(--vscode-focusBorder)}
.row.renium-linked{box-shadow:inset 3px 0 0 var(--vscode-charts-blue,var(--vscode-focusBorder))}
.row.renium-package{box-shadow:inset 3px 0 0 var(--vscode-charts-purple,var(--vscode-focusBorder))}
.row.renium-broken{box-shadow:inset 3px 0 0 var(--vscode-editorWarning-foreground,var(--vscode-charts-yellow))}
.row.placeholder{color:var(--vscode-descriptionForeground)}
.twisty{width:16px;height:22px;display:flex;align-items:center;justify-content:center;font-size:9px;opacity:1;color:#fff}
.twisty::before{content:'\\25B6'}
.twisty.open::before{transform:rotate(90deg)}
.twisty.leaf{visibility:hidden}
.icon{width:16px;height:16px;flex:0 0 16px;margin-right:4px;display:block;object-fit:contain;object-position:center center;image-rendering:pixelated}
.labelWrap{display:inline-flex;align-items:center;min-width:0;flex:1 1 auto;overflow:hidden}
.name{display:flex;align-items:center;min-width:0;max-width:100%;height:22px;line-height:22px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;flex:0 1 auto}
.linkBadge{margin-left:6px;box-sizing:border-box;height:16px;line-height:14px;display:inline-flex;align-items:center;transform:translateY(1px);border-radius:999px;border:1px solid currentColor;padding:0 5px;font-size:10px;font-weight:600;letter-spacing:.02em;opacity:.9;flex:0 0 auto}
.linkBadge.linked{color:var(--vscode-charts-blue,var(--vscode-focusBorder))}
.linkBadge.package{color:var(--vscode-charts-purple,var(--vscode-focusBorder))}
.linkBadge.broken{color:var(--vscode-editorWarning-foreground,var(--vscode-charts-yellow))}
.addBtn{margin-left:6px;flex:none;position:relative;width:18px;height:18px;display:inline-flex;align-items:center;justify-content:center;box-sizing:border-box;padding:0;border-radius:999px;border:1px solid var(--vscode-descriptionForeground);background:color-mix(in srgb,var(--vscode-sideBar-background) 88%,white 12%);color:var(--vscode-foreground);font-size:0;line-height:0;appearance:none;opacity:0;pointer-events:none;transform:scale(.92);transition:opacity .12s ease,transform .12s ease,background-color .12s ease,border-color .12s ease}
.addBtn::before{content:'+';font:600 14px/1 var(--vscode-font-family);transform:translateY(-.5px)}
.addBtn:hover{background:var(--vscode-toolbar-hoverBackground,var(--vscode-list-hoverBackground));border-color:var(--vscode-focusBorder)}
.row:hover .addBtn{opacity:1;pointer-events:auto;transform:scale(1)}
.rename{height:20px;min-width:80px;width:160px;border:1px solid var(--vscode-input-border,transparent);background:var(--vscode-input-background);color:var(--vscode-input-foreground);font:inherit;line-height:18px;padding:1px 4px}
.class{margin-left:6px;color:var(--vscode-descriptionForeground);overflow:hidden;text-overflow:ellipsis}
#menu{position:fixed;z-index:10;min-width:150px;border:1px solid var(--vscode-menu-border);background:var(--vscode-menu-background);padding:4px 0}
#menu.hidden{display:none}
.mi{padding:4px 10px;cursor:pointer;color:var(--vscode-menu-foreground)}
.mi:hover{background:var(--vscode-menu-selectionBackground);color:var(--vscode-menu-selectionForeground)}
#classPicker{position:fixed;z-index:11;width:240px;max-height:320px;border:1px solid var(--vscode-menu-border);background:var(--vscode-menu-background);box-shadow:0 4px 14px rgba(0,0,0,.25);padding:6px}
#classPicker.hidden{display:none}
#classSearch{width:100%;height:22px;margin-bottom:6px;border:1px solid var(--vscode-input-border,transparent);background:var(--vscode-input-background);color:var(--vscode-input-foreground);font:inherit;padding:2px 5px}
#search:focus,#search:focus-visible,#classSearch:focus,#classSearch:focus-visible,.rename:focus,.rename:focus-visible{border-color:var(--property-editor-focus-border)!important;background:var(--vscode-input-background)!important;outline:none!important;box-shadow:none!important}
#classList{max-height:260px;overflow:auto}
.classItem{height:22px;display:flex;align-items:center;gap:6px;padding:2px 4px;cursor:pointer;color:var(--vscode-menu-foreground)}
.classItem:hover,.classItem.active{background:var(--vscode-menu-selectionBackground);color:var(--vscode-menu-selectionForeground)}
#historyHeader{height:30px;display:flex;align-items:center;gap:6px;padding:4px;border-bottom:1px solid var(--vscode-sideBarSectionHeader-border,transparent)}
#historyTitle{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--vscode-descriptionForeground)}
#historyList{flex:1;min-height:0;overflow:auto;padding:2px 0}
.historyGroup{position:relative;--history-guide-color:var(--vscode-tree-indentGuidesStroke,var(--vscode-editorIndentGuide-background,rgba(128,128,128,.35)));--history-guide-x:22px;--history-children-indent:23px;--history-connector-length:11px;border-bottom:1px solid var(--vscode-sideBarSectionHeader-border,transparent)}
.historyGroupHeader{position:relative;display:grid;grid-template-columns:18px minmax(0,1fr) max-content;gap:4px;align-items:center;min-height:42px;padding:4px 6px;cursor:pointer}
.historyGroupHeader:hover,.historyChild:hover{background:var(--vscode-list-hoverBackground)}
.historyTwisty{width:18px;height:22px;border:0;background:transparent;color:var(--vscode-icon-foreground);padding:0;cursor:pointer;font:inherit}
.historyTwisty::before{content:'\\25B6';font-size:9px}
.historyGroup.open .historyTwisty::before{display:inline-block;transform:rotate(90deg)}
.historyMain,.historyChildMain{min-width:0}
.historyChildren{padding:0 0 4px var(--history-children-indent)}
.historyGroup.open .historyGroupHeader::after{content:'';position:absolute;left:var(--history-guide-x);top:22px;bottom:-17px;width:1px;background:var(--history-guide-color);opacity:.72;pointer-events:none}
.historyChild{position:relative;display:grid;grid-template-columns:minmax(0,1fr) max-content;gap:6px;align-items:center;min-height:34px;padding:4px 6px 4px 15px;cursor:pointer}
.historyGroup.open .historyChild::before{content:'';position:absolute;left:calc(var(--history-guide-x) - var(--history-children-indent) + 1px);top:17px;width:var(--history-connector-length);height:1px;background:var(--history-guide-color);opacity:.72;pointer-events:none}
.historyGroup.open .historyChild:not(:last-child)::after{content:'';position:absolute;left:calc(var(--history-guide-x) - var(--history-children-indent));top:17px;bottom:-17px;width:1px;background:var(--history-guide-color);opacity:.72;pointer-events:none}
.historyChild.noDiff{cursor:default}
.historyTarget{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--vscode-foreground)}
.historyMeta{margin-top:2px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--vscode-descriptionForeground);font-size:11px}
.historyActions{display:flex;align-items:center;gap:2px;white-space:nowrap}
.historyAction{height:22px;border:0;background:transparent;color:var(--vscode-icon-foreground);padding:0 5px;cursor:pointer;font:inherit}
.historyAction:hover{background:var(--vscode-toolbar-hoverBackground,var(--vscode-list-hoverBackground))}
.historyAction:disabled{opacity:.45;cursor:default}
#gitPane{overflow:auto;padding:0;background:var(--vscode-sideBar-background)}
.gitRoot{display:flex;flex-direction:column;color:var(--vscode-foreground);font-size:12px}
.gitLoading{display:flex;align-items:center;gap:8px;padding:16px 12px;color:var(--vscode-descriptionForeground)}
.ghSpinner{width:13px;height:13px;flex:0 0 auto;border-radius:50%;border:1.6px solid var(--vscode-descriptionForeground);border-top-color:transparent;animation:ghspin .7s linear infinite}
@keyframes ghspin{to{transform:rotate(360deg)}}
.ghSvg{display:block;flex:0 0 auto}
.ghHead{display:flex;align-items:center;gap:8px;padding:10px 10px 7px}
.ghBranch{display:inline-flex;align-items:center;gap:5px;min-width:0;flex:1;padding:3px 9px;border-radius:11px;background:var(--vscode-badge-background);color:var(--vscode-badge-foreground);font-weight:600}
.ghBranch .ghSvg{opacity:.85}
.ghBranchName{min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.ghSync{display:inline-flex;align-items:center;gap:9px;flex:0 0 auto;font-variant-numeric:tabular-nums}
.ghArrow{display:inline-flex;align-items:center;gap:2px}
.ghArrow.zero{opacity:.4}
.ghIconBtn{flex:0 0 auto;width:24px;height:24px;display:inline-flex;align-items:center;justify-content:center;border:0;border-radius:5px;background:transparent;color:var(--vscode-icon-foreground);cursor:pointer;padding:0}
.ghIconBtn:hover{background:var(--vscode-toolbar-hoverBackground,var(--vscode-list-hoverBackground))}
.ghIconBtn:disabled{opacity:.5;cursor:default;background:transparent}
.ghIconBtn.spin .ghSvg{animation:ghspin .8s linear infinite}
.ghMeta{padding:0 12px 9px;color:var(--vscode-descriptionForeground);font-size:11px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.ghStatus{display:flex;align-items:center;gap:8px;margin:0 10px 10px;padding:7px 10px;border-radius:6px;background:var(--vscode-textBlockQuote-background,rgba(127,127,127,.09));border:1px solid var(--vscode-panel-border,transparent)}
.ghStatus .ghDot{width:8px;height:8px;border-radius:50%;flex:0 0 auto;background:currentColor}
.ghStatus.ok{color:var(--vscode-testing-iconPassed,var(--vscode-charts-green))}
.ghStatus.warn{color:var(--vscode-editorWarning-foreground,var(--vscode-charts-yellow))}
.ghStatus.err{color:var(--vscode-editorError-foreground,var(--vscode-charts-red))}
.ghStatusText{color:var(--vscode-foreground);min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.ghNote{margin:0 10px 10px;padding:6px 10px;border-radius:6px;background:var(--vscode-inputValidation-infoBackground,rgba(96,148,237,.1));border:1px solid var(--vscode-inputValidation-infoBorder,transparent);color:var(--vscode-foreground);font-size:11px;line-height:1.5;word-break:break-word}
.ghActions{display:flex;gap:6px;padding:0 10px 12px}
.ghPrimary{flex:1;min-height:30px;border:0;border-radius:6px;padding:0 10px;font:inherit;font-weight:600;cursor:pointer;color:var(--vscode-button-foreground);background:var(--vscode-button-background)}
.ghPrimary:hover{background:var(--vscode-button-hoverBackground,var(--vscode-button-background))}
.ghPrimary:disabled{opacity:.5;cursor:default}
.ghSecondary{flex:0 0 auto;display:inline-flex;align-items:center;gap:5px;min-height:30px;border:0;border-radius:6px;padding:0 12px;font:inherit;cursor:pointer;color:var(--vscode-button-secondaryForeground);background:var(--vscode-button-secondaryBackground)}
.ghSecondary:hover{background:var(--vscode-button-secondaryHoverBackground,var(--vscode-toolbar-hoverBackground))}
.ghSecondary:disabled{opacity:.5;cursor:default}
.ghSection{border-top:1px solid var(--vscode-sideBarSectionHeader-border,rgba(127,127,127,.15))}
.ghSectionHead{width:100%;display:flex;align-items:center;gap:6px;height:30px;padding:0 10px;border:0;background:transparent;color:var(--vscode-foreground);font:inherit;font-size:11px;font-weight:600;letter-spacing:.03em;text-transform:uppercase;cursor:pointer}
.ghSectionHead:hover{background:var(--vscode-list-hoverBackground)}
.ghSectionHead .ghSvg{color:var(--vscode-icon-foreground);transition:transform .12s ease}
.ghSectionHead.open .ghSvg{transform:rotate(90deg)}
.ghSectionTitle{flex:1;text-align:left;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.ghBadgeCount{min-width:18px;height:16px;padding:0 5px;border-radius:8px;display:inline-flex;align-items:center;justify-content:center;font-size:11px;font-weight:600;letter-spacing:0;color:var(--vscode-badge-foreground);background:var(--vscode-badge-background);opacity:.55}
.ghBadgeCount.has{opacity:1}
.ghChanges{padding:2px 0 6px}
.ghChange{display:flex;align-items:center;gap:8px;min-height:24px;padding:0 10px 0 14px;cursor:pointer}
.ghChange:hover{background:var(--vscode-list-hoverBackground)}
.ghChange:focus-visible{outline:1px solid var(--vscode-focusBorder);outline-offset:-1px}
.ghBadge{flex:0 0 auto;width:16px;text-align:center;font-family:var(--vscode-editor-font-family);font-size:11px;font-weight:600;color:var(--vscode-descriptionForeground)}
.ghBadge.added,.ghBadge.untracked{color:var(--vscode-gitDecoration-addedResourceForeground,var(--vscode-charts-green))}
.ghBadge.modified{color:var(--vscode-gitDecoration-modifiedResourceForeground,var(--vscode-charts-blue))}
.ghBadge.deleted{color:var(--vscode-gitDecoration-deletedResourceForeground,var(--vscode-charts-red))}
.ghBadge.conflict{color:var(--vscode-gitDecoration-conflictingResourceForeground,var(--vscode-editorWarning-foreground))}
.ghChangePath{min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:12px}
.ghChangePath .ghDir{color:var(--vscode-descriptionForeground)}
.ghCommands{display:flex;flex-direction:column;gap:1px;padding:2px 8px 10px}
.gitCommand{display:flex;align-items:center;min-height:26px;border:0;border-radius:5px;padding:0 10px;font:inherit;cursor:pointer;background:transparent;color:var(--vscode-foreground);text-align:left}
.gitCommand:hover{background:var(--vscode-list-hoverBackground)}
.gitCommand:disabled{opacity:.45;cursor:default;background:transparent}
.ghEmpty{display:flex;align-items:center;gap:7px;padding:12px 14px;color:var(--vscode-descriptionForeground)}
.ghEmpty code,.ghNote code{font-family:var(--vscode-editor-font-family);font-size:11px;padding:1px 4px;border-radius:3px;background:var(--vscode-textCodeBlock-background,rgba(127,127,127,.15))}
.gitEmpty{padding:12px;color:var(--vscode-descriptionForeground)}
.ghSectionHead:focus-visible,.ghPrimary:focus-visible,.ghSecondary:focus-visible,.ghIconBtn:focus-visible,.gitCommand:focus-visible{outline:1px solid var(--vscode-focusBorder);outline-offset:-1px}
#storePane{flex:1;min-height:0;display:flex;flex-direction:column;position:relative}
#storeBar{height:30px;display:flex;align-items:center;gap:6px;padding:4px;border-bottom:1px solid var(--vscode-sideBarSectionHeader-border,transparent)}
#storeSearch{flex:1;min-width:0;height:22px;border:1px solid var(--vscode-input-border,transparent);background:var(--vscode-input-background);color:var(--vscode-input-foreground);padding:2px 5px;font:inherit}
#storeOpen{flex:0 0 auto;width:24px;height:24px;display:flex;align-items:center;justify-content:center;border:0;background:transparent;color:var(--vscode-icon-foreground);padding:0;cursor:pointer;border-radius:4px}
#storeOpen:hover{background:var(--vscode-toolbar-hoverBackground,var(--vscode-list-hoverBackground))}
#storeOpen:active{background:var(--vscode-toolbar-activeBackground,var(--vscode-list-activeSelectionBackground))}
#storeOpen svg{display:block}
#storeBrowse{color:var(--vscode-textLink-foreground);cursor:pointer}
.storeDim{color:var(--vscode-descriptionForeground);opacity:.8;font-size:11px}
#storeTree{flex:1 1 auto;overflow:auto;padding:0;min-height:0;outline:none;position:relative}
.rbSizer{position:relative;width:100%}
.rbRows{position:absolute;left:0;right:0;top:0;will-change:transform}
.rbhi{background:var(--vscode-editor-findMatchHighlightBackground,rgba(234,140,0,.35));color:inherit;border-radius:2px}
.storeHint{padding:14px;color:var(--vscode-descriptionForeground);line-height:1.6}
.rberr{padding:12px;color:var(--vscode-errorForeground);white-space:pre-wrap}
#storeDrop{position:absolute;inset:0;display:none;align-items:center;justify-content:center;background:var(--vscode-list-dropBackground,rgba(0,120,215,.18));border:2px dashed var(--vscode-focusBorder);pointer-events:none;z-index:5}
#storeDrop .storeDropIn{font-weight:600;color:var(--vscode-foreground);background:var(--vscode-editor-background);padding:10px 16px;border-radius:8px;box-shadow:0 2px 10px rgba(0,0,0,.35)}
#storePane.rbdrag{outline:2px solid var(--vscode-focusBorder);outline-offset:-2px}
#storePane.rbdrag #storeDrop{display:flex}
</style>
</head>
<body>
<div id="tabs"><button class="tabBtn active" data-tab="explorer">Explorer</button><button class="tabBtn" data-tab="history">History</button><button class="tabBtn" data-tab="git">Git</button><button class="tabBtn" data-tab="store">Inspector</button></div>
<div id="explorerPane">
  <div id="bar"><input id="search" placeholder="Search" spellcheck="false"></div>
  <div id="suggestions">
    <div class="suggestTitle">Suggested Filters</div>
    <div class="suggestItem" data-insert="anchored="><span class="suggestIcon">A</span><span>anchored=</span></div>
    <div class="suggestItem" data-insert="locked="><span class="suggestIcon">L</span><span>locked=</span></div>
    <div class="suggestItem" data-insert="transparency="><span class="suggestIcon">%</span><span>transparency=</span></div>
    <div class="suggestItem" data-insert="material="><span class="suggestIcon">M</span><span>material=</span></div>
    <div class="suggestItem" data-insert="meshid="><span class="suggestIcon">#</span><span>meshid=</span></div>
    <div class="suggestItem" data-insert="textureid="><span class="suggestIcon">T</span><span>textureid=</span></div>
    <div class="suggestItem" data-insert="tag:"><span class="suggestIcon">&#9671;</span><span>tag:</span></div>
  </div>
  <div id="searchMeta"><span class="searchSummary">0 matches</span><span class="searchActions"><button class="iconBtn" id="prevMatch" title="Select previous match">&uarr;</button><button class="iconBtn" id="nextMatch" title="Select next match">&darr;</button><button class="iconBtn" id="selectMatches" title="Select all matches">&#9633;</button><button class="iconBtn" id="refreshResults" title="Refresh results">&#8635;</button></span></div>
  <div id="tree" tabindex="0">${initialRows || '<div id="treeEmpty">Loading services...</div>'}</div>
</div>
<div id="historyPane" class="hidden">
  <div id="historyHeader"><span id="historyTitle">Editor History</span><button class="iconBtn" id="refreshHistory" title="Refresh history">&#8635;</button></div>
  <div id="historyList"><div id="treeEmpty">Loading history...</div></div>
</div>
<div id="gitPane" class="hidden">
  <div id="gitApp"><div class="gitEmpty">Loading Git status...</div></div>
</div>
<div id="storePane" class="hidden">
  <div id="storeBar"><input id="storeSearch" placeholder="Search" spellcheck="false"><button id="storeOpen" type="button" title="Open a .renium file" aria-label="Open a .renium file"><svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M1.75 2.5h3.9c.27 0 .53.1.72.3L7.5 4h6.25c.41 0 .75.34.75.75v8c0 .41-.34.75-.75.75H1.75A.75.75 0 0 1 1 12.75v-9.5c0-.41.34-.75.75-.75Zm.25 1.5v8h11V5.5H6.88L5.38 4H2Z"/></svg></button></div>
  <div id="storeTree" tabindex="0"><div class="storeHint">Open a <b>.renium</b> store with the folder button above, or <a id="storeBrowse" href="#">browse for a file</a>.</div></div>
  <div id="storeDrop"><div class="storeDropIn">Drop to inspect</div></div>
</div>
<div id="menu" class="hidden"></div>
<div id="classPicker" class="hidden"><input id="classSearch" placeholder="ClassName" spellcheck="false"><div id="classList"></div></div>
<script>
(function(){
var vscode=acquireVsCodeApi(),ASSET=${JSON.stringify(assetBase)},CLASS_NAMES=${classNamesJson},AVAILABLE_ICONS=new Set(${assetIconNamesJson});
var nodes={},rootIds=[],expanded=new Set(),selectedId=null,lastHostSelectionId=null,referencePreviewId=null,filter='',menuNode=null,menuX=0,menuY=0;
var linkKeys={},externalPackageDrag=null,packageDragCursorSawDown=false;
function nodeLinkState(n){
  if(!n||!n.pathSegments||n.pathSegments.length<2)return null;
  return linkKeys[n.pathSegments[0]+String.fromCharCode(1)+n.pathSegments.slice(1).join('/')]||null;
}
function directReniumState(n){
  if(!n||n.kind==='service')return null;
  var linked=nodeLinkState(n),isPackage=n.hasPackageLink===true;
  if(linked==='broken')return{kind:'broken',inherited:false,package:isPackage};
  if(isPackage)return{kind:'package',inherited:false,package:true};
  if(linked==='linked')return{kind:'linked',inherited:false,package:false};
  return null;
}
function reniumBadgeHtml(state){
  if(!state)return '';
  var label=state.kind==='broken'?'Broken':(state.kind==='package'?'Package':'Linked');
  var title=state.inherited?label+' by parent Renium target':label+' Renium target';
  return '<span class="linkBadge '+esc(state.kind)+'" title="'+esc(title)+'">'+esc(label)+'</span>';
}
function canDesyncPackage(n){return !!n&&n.kind!=='service'&&(n.className==='PackageLink'||n.hasPackageLink===true)}
var renameId=null,renameOriginal='',suppressRenameFocusoutRender=false,renamePointerStartedInside=false,renameSuppressFocusoutUntil=0,draggedId=null,dropId=null,lastPointerRowId=null,screenOffsetX=null,screenOffsetY=null,addParentId=null,classActive=0,loadingIds={},loadDelayUntil={},autoLoadIds=[],matchIds=[],matchIndex=-1,searchLoading=false,searchRequested=false,searchLoaded=0,searchTotal=0,searchMatchCount=0,allMatchesSelected=false;
var searchPlanFilter=null,searchPlanGroups=[],selfMatchCache={},subtreeMatchCache={},searchExpanded=new Set(),renderFrame=0,pendingRenderAnchor=null;
var searchIndexDirty=true,searchEntries={},searchEntryIds=[],searchResultsFilter=null,searchVisibleSet=new Set(),searchResultIds=[];
var ROW_HEIGHT=22,VIRTUAL_OVERSCAN=40,flatRows=[],visibleRenderFrame=0,currentEmptyHtml='',totalRows=0,rowWindowStart=0,lastRequestedStart=-1,lastRequestedCount=0,lastRequestMode='normal',searchDebounce=null,rowRequestPending=false,searchPointerOpenUntil=0,searchRetainFocusUntil=0,searchRestoringFocus=false,searchSuggestionsShownThisFocus=false,rowCache={},rowCacheMode='normal',backendErrorRetryCount=0,searchRevision=0,searchInitialLoading=false,prefetchPending=false,prefetchTimer=null,lastScrollTop=0,lastScrollTime=0,scrollVelocityRows=0,scrollDirection=1;
var dragAutoScrollFrame=0,dragAutoScrollDirection=0,dragAutoScrollPointerY=0;
var tree=document.getElementById('tree'),search=document.getElementById('search'),searchMeta=document.getElementById('searchMeta'),suggestions=document.getElementById('suggestions'),menu=document.getElementById('menu');
var searchSummary=searchMeta.querySelector('.searchSummary'),prevMatch=document.getElementById('prevMatch'),nextMatch=document.getElementById('nextMatch'),selectMatches=document.getElementById('selectMatches'),refreshResults=document.getElementById('refreshResults');
var classPicker=document.getElementById('classPicker'),classSearch=document.getElementById('classSearch'),classList=document.getElementById('classList');
var tabs=document.getElementById('tabs'),explorerPane=document.getElementById('explorerPane'),historyPane=document.getElementById('historyPane'),gitPane=document.getElementById('gitPane'),gitApp=document.getElementById('gitApp'),historyList=document.getElementById('historyList'),historyTitle=document.getElementById('historyTitle'),refreshHistory=document.getElementById('refreshHistory');
var storePane=document.getElementById('storePane'),storeTree=document.getElementById('storeTree'),storeSearch=document.getElementById('storeSearch');
var saved=vscode.getState()||{};
var activeTab=saved.activeTab==='history'||saved.activeTab==='git'||saved.activeTab==='store'?saved.activeTab:'explorer',historyGroups=[],historyLoading=false,historyLoaded=false,historyRestoring={},historyExpanded=new Set(Array.isArray(saved.historyExpanded)?saved.historyExpanded:[]);
var gitState=null,gitLoading=false,gitProjectRoot='',gitGeneration=0,gitChangesOpen=saved.gitChangesOpen!==false,gitAdvancedOpen=!!saved.gitAdvancedOpen;
var hasClipboardInstance=false;
var packageDragDebugLast=0,packageDragDebugLastMessage='';
function debugPackageDrag(message){
  var text=String(message||'');
  var now=Date.now();
  var noisy=/^(row from|no row|target row|cursor)/.test(text);
  if(noisy&&text===packageDragDebugLastMessage&&now-packageDragDebugLast<500)return;
  if(noisy&&now-packageDragDebugLast<120)return;
  packageDragDebugLast=now;
  packageDragDebugLastMessage=text;
  vscode.postMessage({type:'packageDragDebug',message:text});
}
var VALUE_ICON_FALLBACKS={BinaryStringValue:1,Color3Value:1,DoubleConstrainedValue:1,IntConstrainedValue:1,IntValue:1,NumberValue:1,ObjectValue:1,StringValue:1,Vector3Value:1};
var CLASS_NAME_SET=new Set(CLASS_NAMES);
var FREQUENT_CLASS_DEFAULT=['Folder','Model','Part','Script','LocalScript','ModuleScript','Attachment','RemoteEvent','RemoteFunction','Configuration'];
var FREQUENT_CLASS_BY_SERVICE={
  Workspace:['Part','Model','Folder','SpawnLocation','Script','Attachment','WeldConstraint','PointLight','Sound','Highlight'],
  ReplicatedStorage:['Folder','ModuleScript','RemoteEvent','RemoteFunction','BindableEvent','BindableFunction','Configuration','Model','Part','Animation'],
  ReplicatedFirst:['LocalScript','ModuleScript','Folder','ScreenGui','Sound','Configuration','Model','Part','BindableEvent','Animation'],
  ServerScriptService:['Script','ModuleScript','Folder','Configuration','BindableEvent','BindableFunction','Model','Part','Sound','Animation'],
  ServerStorage:['Folder','ModuleScript','Script','Model','Part','Tool','Configuration','Animation','Sound','MeshPart'],
  StarterGui:['ScreenGui','Frame','TextLabel','TextButton','ImageLabel','ImageButton','ScrollingFrame','UIListLayout','UIPadding','UICorner'],
  StarterPack:['Tool','LocalScript','ModuleScript','Folder','Model','Part','Animation','Sound','Configuration','RemoteEvent'],
  StarterPlayer:['StarterPlayerScripts','StarterCharacterScripts','LocalScript','ModuleScript','Folder','Tool','Animation','Sound','Configuration','Model'],
  StarterPlayerScripts:['LocalScript','ModuleScript','Folder','Configuration','BindableEvent','BindableFunction','RemoteEvent','RemoteFunction','Sound','Animation'],
  StarterCharacterScripts:['LocalScript','Script','ModuleScript','Folder','Animation','Sound','Attachment','ParticleEmitter','Trail','Configuration'],
  Lighting:['Sky','Atmosphere','BloomEffect','ColorCorrectionEffect','SunRaysEffect','DepthOfFieldEffect','BlurEffect','Clouds','Folder','Script'],
  SoundService:['Sound','Folder','EqualizerSoundEffect','ReverbSoundEffect','CompressorSoundEffect','ChorusSoundEffect','DistortionSoundEffect','EchoSoundEffect','FlangeSoundEffect','Script'],
  MaterialService:['MaterialVariant','Folder','Configuration','ModuleScript','Script','StringValue','Model','Part','SurfaceAppearance','Texture'],
  Teams:['Team','Folder','Script','ModuleScript','Configuration','StringValue','BoolValue','Color3Value','Part','Model']
};
var FREQUENT_CLASS_BY_PARENT={
  Folder:['Folder','Model','Part','Script','LocalScript','ModuleScript','Attachment','Configuration','RemoteEvent','RemoteFunction'],
  Model:['Part','MeshPart','UnionOperation','Attachment','WeldConstraint','Motor6D','Script','LocalScript','ModuleScript','Folder'],
  Part:['Attachment','WeldConstraint','PointLight','ParticleEmitter','Decal','Texture','SurfaceGui','Highlight','Sound','Script'],
  MeshPart:['Attachment','WeldConstraint','PointLight','ParticleEmitter','SurfaceAppearance','Decal','Texture','Sound','Trail','Script'],
  Attachment:['ParticleEmitter','Trail','Beam','PointLight','SpotLight','Smoke','Fire','Sparkles','Sound','Script'],
  ScreenGui:['Frame','TextLabel','TextButton','ImageLabel','ImageButton','ScrollingFrame','UIListLayout','UIPadding','UICorner','UIStroke'],
  SurfaceGui:['Frame','TextLabel','TextButton','ImageLabel','ImageButton','ScrollingFrame','UIListLayout','UIPadding','UICorner','UIStroke'],
  BillboardGui:['Frame','TextLabel','TextButton','ImageLabel','ImageButton','UIListLayout','UIPadding','UICorner','UIStroke','UIScale'],
  Frame:['Frame','TextLabel','TextButton','ImageLabel','ImageButton','ScrollingFrame','UIListLayout','UIPadding','UICorner','UIStroke'],
  ScrollingFrame:['Frame','TextLabel','TextButton','ImageLabel','ImageButton','UIListLayout','UIPadding','UICorner','UIStroke','UISizeConstraint'],
  ViewportFrame:['Model','Part','Camera','Folder','Attachment','PointLight','Highlight','Script','ModuleScript','Sound'],
  TextButton:['UICorner','UIStroke','UIGradient','UIScale','UITextSizeConstraint','LocalScript','Sound','Frame','ImageLabel','UIAspectRatioConstraint'],
  ImageButton:['UICorner','UIStroke','UIGradient','UIScale','UIAspectRatioConstraint','LocalScript','Sound','Frame','ImageLabel','TextLabel'],
  Tool:['Part','LocalScript','Script','ModuleScript','Animation','Sound','Attachment','Folder','Configuration','Handle']
};
if(Array.isArray(saved.expanded))expanded=new Set(saved.expanded);
if(saved.selectedId)selectedId=saved.selectedId;
function save(){vscode.setState({expanded:Array.from(expanded),selectedId:selectedId,activeTab:activeTab,historyExpanded:Array.from(historyExpanded),gitChangesOpen:gitChangesOpen,gitAdvancedOpen:gitAdvancedOpen,screenOffsetX:screenOffsetX,screenOffsetY:screenOffsetY,screenOffsetWX:screenOffsetWX,screenOffsetWY:screenOffsetWY})}
function syncSelectionToHost(){
  if(selectedId&&nodes[selectedId]&&selectedId!==lastHostSelectionId){
    lastHostSelectionId=selectedId;
    vscode.postMessage({type:'selectNode',nodeId:selectedId});
  }
}
function expandAncestors(id){
  var n=nodes[id],changed=false;
  while(n&&n.parentId){
    if(!expanded.has(n.parentId)){expanded.add(n.parentId);changed=true}
    n=nodes[n.parentId];
  }
  if(changed)save();
}
function esc(s){return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;')}
function canShowSearchSuggestions(){
  return activeTab==='explorer'&&!search.value.trim();
}
function showSearchSuggestionsOnce(){
  if(searchSuggestionsShownThisFocus)return;
  if(canShowSearchSuggestions()){
    suggestions.classList.add('active');
    searchSuggestionsShownThisFocus=true;
  }
}
function hideSearchSuggestions(){
  suggestions.classList.remove('active');
}
function setActiveTab(tab, skipLoad){
  activeTab=tab==='history'||tab==='git'||tab==='store'?tab:'explorer';
  save();
  Array.prototype.forEach.call(tabs.querySelectorAll('.tabBtn'),function(btn){
    btn.classList.toggle('active',btn.dataset.tab===activeTab);
  });
  explorerPane.classList.toggle('hidden',activeTab!=='explorer');
  historyPane.classList.toggle('hidden',activeTab!=='history');
  gitPane.classList.toggle('hidden',activeTab!=='git');
  storePane.classList.toggle('hidden',activeTab!=='store');
  hideSearchSuggestions();
  closeMenus();
  if(activeTab==='history'){
    renderHistory();
    if(!skipLoad&&!historyLoaded&&!historyLoading)loadHistory();
  }else if(activeTab==='git'){
    renderGit();
    if(!skipLoad)vscode.postMessage({type:'gitReady'});
  }else if(activeTab==='store'){
    if(typeof rbPaint==='function')rbPaint();
  }else if(activeTab==='explorer'&&!skipLoad){
    requestRows(false);
  }
}
function prepareReferencePreview(){
  if(activeTab!=='explorer')setActiveTab('explorer',true);
  if(filter||search.value.trim()){
    search.value='';
    searchRevision++;
    filter='';
    if(searchDebounce){clearTimeout(searchDebounce);searchDebounce=null}
    if(prefetchTimer){clearTimeout(prefetchTimer);prefetchTimer=null}
    prefetchPending=false;
    searchInitialLoading=false;
    searchExpanded.clear();
    searchRequested=false;
    searchLoading=false;
    searchLoaded=0;
    searchTotal=0;
    searchMatchCount=0;
    matchIds=[];
    allMatchesSelected=false;
    rowWindowStart=0;
    totalRows=0;
    flatRows=[];
    lastRequestedStart=-1;
    lastRequestMode='normal';
    resetRowCache('normal');
    currentEmptyHtml='<div id="treeEmpty">Loading...</div>';
    renderFlatRows();
    updateSearchMeta();
  }
}
function loadHistory(){
  historyLoading=true;
  renderHistory();
  vscode.postMessage({type:'loadHistory'});
}
function historyMetaText(entry){
  var bits=[];
  if(entry.service)bits.push(entry.service);
  if(entry.className)bits.push(entry.className);
  if(entry.settingsId)bits.push(entry.settingsId);
  if(entry.timeLabel||entry.createdLabel)bits.push(entry.timeLabel||entry.createdLabel);
  return bits.join(' · ');
}
function renderHistory(){
  if(!historyList)return;
  if(historyLoading&&historyGroups.length===0){
    historyTitle.textContent='Editor History';
    historyList.innerHTML='<div id="treeEmpty">Loading history...</div>';
    return;
  }
  var editTotal=0;
  for(var gCount=0;gCount<historyGroups.length;gCount++)editTotal+=historyGroups[gCount].entryCount||0;
  historyTitle.textContent=historyGroups.length?('Editor History ('+historyGroups.length+' sessions, '+editTotal+' edits)'):'Editor History';
  if(historyGroups.length===0){
    historyList.innerHTML='<div id="treeEmpty">No editor history found.</div>';
    return;
  }
  var html='';
  for(var i=0;i<historyGroups.length;i++){
    var group=historyGroups[i],open=historyExpanded.has(group.id),restoring=!!historyRestoring[group.id];
    var primary=(group.items||[])[0];
    var restoreIds=(group.items||[]).map(function(item){return item.restoreId}).filter(Boolean);
    html+='<div class="historyGroup'+(open?' open':'')+'" data-group-id="'+esc(group.id)+'">';
    html+='<div class="historyGroupHeader" data-action="toggleHistoryGroup" data-group-id="'+esc(group.id)+'">';
    html+='<button class="historyTwisty" data-action="toggleHistoryGroup" data-group-id="'+esc(group.id)+'" title="'+(open?'Collapse':'Expand')+'"></button>';
    html+='<div class="historyMain"><div class="historyTarget" title="'+esc(group.title||'')+'">'+esc(group.title||'History session')+'</div>';
    html+='<div class="historyMeta">'+esc(group.subtitle||'')+'</div></div>';
    html+='<div class="historyActions">';
    if(primary&&primary.hasSourceBackup){
      html+='<button class="historyAction" data-action="compareHistoryBackup" data-id="'+esc(primary.openId||primary.id)+'" title="Compare backup with current file">Diff</button>';
      html+='<button class="historyAction" data-action="openHistoryBackup" data-id="'+esc(primary.openId||primary.id)+'" title="Open source backup">Open</button>';
    }
    html+='<button class="historyAction" data-action="restoreHistoryGroup" data-group-id="'+esc(group.id)+'" data-ids="'+esc(JSON.stringify(restoreIds))+'" '+(restoring?'disabled':'')+' title="Restore this edit session">'+(restoring?'Restoring':'Restore')+'</button>';
    html+='</div></div>';
    if(open){
      html+='<div class="historyChildren">';
      var items=group.items||[];
      for(var j=0;j<items.length;j++){
        var entry=items[j],entryRestoring=!!historyRestoring[entry.restoreId];
        html+='<div class="historyChild'+(entry.hasSourceBackup?'':' noDiff')+'" data-id="'+esc(entry.restoreId)+'" data-open-id="'+esc(entry.openId||entry.id)+'" title="'+(entry.hasSourceBackup?'Click to compare with current file':'')+'">';
        html+='<div class="historyChildMain"><div class="historyTarget" title="'+esc(entry.targetLabel||'')+'">'+esc(entry.targetLabel||entry.service||'History entry')+'</div>';
        html+='<div class="historyMeta">'+esc(historyMetaText(entry)+(entry.editCount>1?' · '+entry.editCount+' versions':''))+'</div></div>';
        html+='<div class="historyActions">';
        html+='<button class="historyAction" data-action="compareHistoryBackup" data-id="'+esc(entry.openId||entry.id)+'" '+(entry.hasSourceBackup?'':'disabled')+' title="Compare backup with current file">Diff</button>';
        html+='<button class="historyAction" data-action="openHistoryBackup" data-id="'+esc(entry.openId||entry.id)+'" '+(entry.hasSourceBackup?'':'disabled')+' title="Open source backup">Open</button>';
        html+='<button class="historyAction" data-action="restoreHistory" data-id="'+esc(entry.restoreId||entry.id)+'" '+(entryRestoring?'disabled':'')+' title="Restore this item">'+(entryRestoring?'Restoring':'Restore')+'</button>';
        html+='</div></div>';
      }
      html+='</div>';
    }
    html+='</div>';
  }
  historyList.innerHTML=html;
}
function gitDisabled(enabled){return enabled&&!gitLoading?'':' disabled'}
function gitEntryBadge(entry){
  if(entry.conflicted)return '!';
  if(entry.deleted)return 'D';
  if(entry.untracked)return 'U';
  if(entry.kind==='added')return 'A';
  if(entry.kind==='renamed')return 'R';
  if(entry.kind==='copied')return 'C';
  if(entry.kind==='typechange')return 'T';
  return 'M';
}
function gitEntryClass(entry){
  if(entry.conflicted)return 'conflict';
  if(entry.deleted)return 'deleted';
  if(entry.untracked)return 'untracked';
  if(entry.kind==='added'||entry.kind==='copied')return 'added';
  return 'modified';
}
function gitActionButton(label, action, enabled, title){
  return '<button class="gitCommand" data-gh-action="'+esc(action)+'" title="'+esc(title||label)+'"'+gitDisabled(enabled)+'>'+esc(label)+'</button>';
}
function toggleGitGroup(name){
  if(name==='changes')gitChangesOpen=!gitChangesOpen;
  else if(name==='actions')gitAdvancedOpen=!gitAdvancedOpen;
  save();
  renderGit();
}
function closeGitActions(){
  if(!gitAdvancedOpen)return;
  gitAdvancedOpen=false;
  save();
  renderGit();
}
function ghSvg(inner,cls){return '<svg class="ghSvg'+(cls?' '+cls:'')+'" viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.35" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">'+inner+'</svg>'}
function ghBranchIcon(){return ghSvg('<circle cx="4.5" cy="4" r="1.6"/><circle cx="4.5" cy="12" r="1.6"/><circle cx="11.5" cy="5.5" r="1.6"/><path d="M4.5 5.6v4.8"/><path d="M11.5 7.1c0 2.3-2.5 2.7-4.1 3.1"/>')}
function ghUpIcon(){return ghSvg('<path d="M8 12.5V4"/><path d="M4.8 7.2 8 4l3.2 3.2"/>')}
function ghDownIcon(){return ghSvg('<path d="M8 3.5V12"/><path d="M4.8 8.8 8 12l3.2-3.2"/>')}
function ghRefreshIcon(){return ghSvg('<path d="M12.8 8a4.8 4.8 0 1 1-1.4-3.4"/><path d="M12.9 2.7v2.6h-2.6"/>')}
function ghCheckIcon(){return ghSvg('<path d="M3.4 8.4 6.3 11.3 12.6 4.6"/>')}
function ghTwisty(){return ghSvg('<path d="M6 4l4 4-4 4"/>')}
function renderGit(){
  if(!gitApp)return;
  if(!gitState){
    gitApp.innerHTML='<div class="gitLoading"><span class="ghSpinner"></span>Loading Git status...</div>';
    return;
  }
  var counts=gitState.counts||{},entries=Array.isArray(gitState.entries)?gitState.entries:[];
  var conflicts=counts.conflicted||0,behind=gitState.behind||0,ahead=gitState.ahead||0,total=counts.total||0;
  var branch=gitState.branch||'unknown';
  var remote=gitState.remote||'origin';
  var trusted=gitState.trusted!==false;
  var connected=!!gitState.connected;
  var canRepo=trusted&&connected;
  var canSync=canRepo&&conflicts===0;
  var canSetup=trusted;
  var dotClass=!trusted||!gitState.ok||conflicts?'err':(!connected||behind)?'warn':'ok';
  var statusText=!trusted?'Workspace not trusted':!connected?'No remote connected':conflicts?(conflicts+' conflict'+(conflicts===1?'':'s')+' to resolve'):behind?(behind+' commit'+(behind===1?'':'s')+' to pull'):ahead?(ahead+' commit'+(ahead===1?'':'s')+' to push'):'Up to date';
  var repoMeta=connected?((gitState.upstream||remote)+(gitState.remoteUrl?' · '+gitState.remoteUrl:'')):'Not connected to a remote';
  var message=gitState.message||'';
  var primaryActionLabel=gitLoading?'Working...':connected?'Commit & Push':'Connect Remote...';
  var primaryAction=connected?'commitPush':'connect';
  var primaryActionEnabled=connected?canSync:canSetup;
  var html='<div class="gitRoot">';
  html+='<div class="ghHead">';
  html+='<span class="ghBranch" title="Current branch">'+ghBranchIcon()+'<span class="ghBranchName">'+esc(branch)+'</span></span>';
  html+='<span class="ghSync">';
  html+='<span class="ghArrow'+(ahead?'':' zero')+'" title="'+esc(ahead)+' to push">'+ghUpIcon()+esc(ahead)+'</span>';
  html+='<span class="ghArrow'+(behind?'':' zero')+'" title="'+esc(behind)+' to pull">'+ghDownIcon()+esc(behind)+'</span>';
  html+='</span>';
  html+='<button class="ghIconBtn'+(gitLoading?' spin':'')+'" data-gh-refresh="1" title="Refresh Git status"'+(gitLoading?' disabled':'')+'>'+ghRefreshIcon()+'</button>';
  html+='</div>';
  html+='<div class="ghMeta" title="'+esc(repoMeta)+'">'+esc(repoMeta)+'</div>';
  html+='<div class="ghStatus '+dotClass+'"><span class="ghDot"></span><span class="ghStatusText">'+esc(gitLoading?'Syncing...':statusText)+'</span></div>';
  if(message)html+='<div class="ghNote">'+esc(message)+'</div>';
  html+='<div class="ghActions">';
  html+='<button class="ghPrimary" data-gh-action="'+esc(primaryAction)+'"'+gitDisabled(primaryActionEnabled)+'>'+esc(primaryActionLabel)+'</button>';
  if(connected)html+='<button class="ghSecondary" data-gh-action="pull"'+gitDisabled(canSync)+' title="Pull from '+esc(remote)+'">'+ghDownIcon()+'Pull'+(behind?' '+esc(behind):'')+'</button>';
  else html+='<button class="ghSecondary" data-gh-output="1"'+gitDisabled(trusted)+'>Show Output</button>';
  html+='</div>';
  html+='<div class="ghSection">';
  html+='<button class="ghSectionHead '+(gitChangesOpen?'open':'')+'" data-gh-group="changes" aria-expanded="'+(gitChangesOpen?'true':'false')+'">'+ghTwisty()+'<span class="ghSectionTitle">Changes</span><span class="ghBadgeCount'+(total?' has':'')+'">'+esc(total)+'</span></button>';
  if(gitChangesOpen){
    html+='<div class="ghChanges">';
    if(entries.length===0){
      html+='<div class="ghEmpty">'+ghCheckIcon()+'<span>No changes in project sources</span></div>';
    }else{
      for(var i=0;i<Math.min(entries.length,200);i++){
        var entry=entries[i];
        var badge=gitEntryBadge(entry),badgeClass=gitEntryClass(entry);
        var path=String(entry.path||''),slash=path.lastIndexOf('/');
        var pathHtml=slash>=0?'<span class="ghDir">'+esc(path.slice(0,slash+1))+'</span>'+esc(path.slice(slash+1)):esc(path);
        html+='<div class="ghChange" data-gh-diff="'+esc(path)+'" role="button" tabindex="0" title="Open diff: '+esc(path)+'"><span class="ghBadge '+badgeClass+'">'+esc(badge)+'</span><span class="ghChangePath">'+pathHtml+'</span></div>';
      }
      if(entries.length>200)html+='<div class="ghNote">Showing first 200 of '+esc(entries.length)+' changes.</div>';
    }
    html+='</div>';
  }
  html+='</div>';
  html+='<div class="ghSection">';
  html+='<button class="ghSectionHead '+(gitAdvancedOpen?'open':'')+'" data-gh-group="actions" aria-expanded="'+(gitAdvancedOpen?'true':'false')+'">'+ghTwisty()+'<span class="ghSectionTitle">Repository Actions</span></button>';
  if(gitAdvancedOpen){
    html+='<div class="ghCommands">';
    html+=gitActionButton('Pull from Studio, Commit and Push','pullCommitPush',canSync,'Pull Studio changes before committing project source changes');
    html+=gitActionButton('Fetch','fetch',canRepo,'Fetch remote refs');
    html+=gitActionButton('Connect Remote...','connect',canSetup,'Initialize or configure the Git remote');
    html+=gitActionButton('Open on Git','openRemote',canRepo,'Open the configured remote in a browser');
    html+=gitActionButton('Checkout Branch...','checkoutBranch',canSync,'Switch to another local branch');
    html+=gitActionButton('Create Branch...','createBranch',canSync,'Create a new branch');
    html+=gitActionButton('Publish Branch','publishBranch',canSync,'Publish the current branch upstream');
    html+=gitActionButton('Log Status','status',trusted,'Write detailed Git status to the Renium output');
    html+='<button class="gitCommand" data-gh-output="1"'+gitDisabled(trusted)+'>Show Output</button>';
    html+='</div>';
  }
  html+='</div></div>';
  gitApp.innerHTML=html;
}
function iconName(className){
  var preferred=VALUE_ICON_FALLBACKS[className]?'Value':className;
  if(AVAILABLE_ICONS.has(preferred))return preferred;
  var fallback=className&&className.endsWith('Service')?'Service':'Class';
  return AVAILABLE_ICONS.has(fallback)?fallback:preferred;
}
function closeMenus(){menu.classList.add('hidden');classPicker.classList.add('hidden');hideSearchSuggestions()}
function uniqueClassNames(items){
  var out=[],seen={};
  for(var i=0;i<items.length;i++){
    var name=String(items[i]||'');
    if(!CLASS_NAME_SET.has(name)||seen[name])continue;
    seen[name]=1;
    out.push(name);
  }
  return out;
}
function frequentClassesForNodeId(id){
  var node=id&&nodes[id],preferred=[];
  if(node){
    var serviceClasses=FREQUENT_CLASS_BY_SERVICE[node.service||''];
    var parentClasses=FREQUENT_CLASS_BY_PARENT[node.className||''];
    if(Array.isArray(serviceClasses))preferred=preferred.concat(serviceClasses);
    if(Array.isArray(parentClasses))preferred=preferred.concat(parentClasses);
  }
  preferred=uniqueClassNames(preferred.concat(FREQUENT_CLASS_DEFAULT));
  return preferred.slice(0,10);
}
function orderedClassNamesForParent(id){
  var preferred=frequentClassesForNodeId(id),out=[],seen={};
  for(var i=0;i<preferred.length;i++){
    var preferredName=preferred[i];
    seen[preferredName]=1;
    out.push(preferredName);
  }
  for(var j=0;j<CLASS_NAMES.length;j++){
    var className=CLASS_NAMES[j];
    if(!seen[className])out.push(className);
  }
  return out;
}
function rowEl(id){
  var rows=tree.querySelectorAll('.row');
  for(var i=0;i<rows.length;i++)if(rows[i].dataset.id===id)return rows[i];
  return null;
}
function flatRowIndex(id){
  for(var i=0;i<flatRows.length;i++)if(flatRows[i].id===id)return rowWindowStart+i;
  return -1;
}
function firstVisibleRow(){
  var treeRect=tree.getBoundingClientRect(),rows=tree.querySelectorAll('.row[data-id]');
  for(var i=0;i<rows.length;i++){
    var rect=rows[i].getBoundingClientRect();
    if(rect.bottom>=treeRect.top&&rect.top<=treeRect.bottom)return rows[i];
  }
  return null;
}
function captureScrollAnchor(id){
  if(id){
    var flatIndex=flatRowIndex(id);
    if(flatIndex>=0)return {id:id,top:flatIndex*ROW_HEIGHT-tree.scrollTop,scrollTop:tree.scrollTop};
  }
  var row=id?rowEl(id):firstVisibleRow();
  if(!row)return {scrollTop:tree.scrollTop};
  return {id:row.dataset.id,top:row.getBoundingClientRect().top-tree.getBoundingClientRect().top,scrollTop:tree.scrollTop};
}
function restoreScrollAnchor(anchor){
  if(!anchor)return;
  if(anchor.id){
    var flatIndex=flatRowIndex(anchor.id);
    if(flatIndex>=0){
      tree.scrollTop=Math.max(0,flatIndex*ROW_HEIGHT-anchor.top);
      return;
    }
  }
  var row=anchor.id?rowEl(anchor.id):null;
  if(row){
    tree.scrollTop+=row.getBoundingClientRect().top-tree.getBoundingClientRect().top-anchor.top;
  }else if(typeof anchor.scrollTop==='number'){
    tree.scrollTop=anchor.scrollTop;
  }
}
function visibleStart(){
  return Math.max(0,Math.floor(tree.scrollTop/ROW_HEIGHT)-VIRTUAL_OVERSCAN);
}
function visibleCount(){
  return Math.max(40,Math.ceil((tree.clientHeight||300)/ROW_HEIGHT)+VIRTUAL_OVERSCAN*2);
}
function resetRowCache(mode){
  rowCache={};
  rowCacheMode=mode||((filter?'search':'normal'));
}
function rememberRows(rows){
  for(var i=0;i<rows.length;i++){
    var row=rows[i];
    if(row&&row.id){
      nodes[row.id]=row;
      delete loadingIds[row.id];
    }
  }
}
function pruneRowCache(start,count){
  var span=rowCacheMode==='search'?30000:10000;
  var keepBefore=Math.max(0,start-span),keepAfter=start+count+span;
  Object.keys(rowCache).forEach(function(key){
    var index=Number(key);
    if(index<keepBefore||index>keepAfter){
      var row=rowCache[key];
      if(row&&row.id&&row.id!==selectedId)delete nodes[row.id];
      delete rowCache[key];
    }
  });
}
function cachedWindow(start,count){
  var rows=[],limit=totalRows>0?Math.min(count,Math.max(0,totalRows-start)):count;
  for(var i=0;i<limit;i++){
    rows.push(rowCache[start+i]||{type:'loading',depth:0});
  }
  return rows;
}
function optimisticDelete(id){
  if(!id)return;
  var anchor=captureScrollAnchor(),idx=-1,row=null;
  for(var i=0;i<flatRows.length;i++){
    if(flatRows[i]&&flatRows[i].id===id){idx=i;row=flatRows[i];break}
  }
  delete nodes[id];delete loadingIds[id];expanded.delete(id);
  if(selectedId===id)selectedId=null;
  if(idx>=0&&row){
    var depth=Number(row.depth)||0,removeCount=1;
    while(idx+removeCount<flatRows.length){
      var next=flatRows[idx+removeCount];
      if(!next||typeof next.depth!=='number'||next.depth<=depth)break;
      if(next.id){delete nodes[next.id];delete loadingIds[next.id];expanded.delete(next.id);if(selectedId===next.id)selectedId=null}
      removeCount++;
    }
    flatRows.splice(idx,removeCount);
    totalRows=Math.max(0,totalRows-removeCount);
  }
  rowCache={};
  save();render(anchor);syncSelectionToHost();
}
function firstMissingRow(start,end){
  start=Math.max(0,start);end=Math.min(totalRows,end);
  for(var i=start;i<end;i++)if(!rowCache[i])return i;
  return -1;
}
function lastMissingRow(start,end){
  start=Math.max(0,start);end=Math.min(totalRows,end);
  for(var i=end-1;i>=start;i--)if(!rowCache[i])return i;
  return -1;
}
function updateScrollVelocity(){
  var now=(typeof performance!=='undefined'&&performance.now)?performance.now():Date.now();
  var top=tree.scrollTop||0;
  if(!lastScrollTime){
    lastScrollTime=now;lastScrollTop=top;return;
  }
  var dt=Math.max(16,now-lastScrollTime),dy=top-lastScrollTop;
  if(dy!==0){
    var instant=(dy/ROW_HEIGHT)/(dt/1000);
    scrollVelocityRows=scrollVelocityRows*0.55+instant*0.45;
    scrollDirection=dy<0?-1:1;
  }else{
    scrollVelocityRows*=0.82;
  }
  lastScrollTime=now;lastScrollTop=top;
}
function schedulePrefetch(){
  if(prefetchTimer)clearTimeout(prefetchTimer);
  prefetchTimer=setTimeout(function(){
    prefetchTimer=null;
    if(prefetchPending||rowRequestPending||totalRows<=0)return;
    var mode=filter?'search':'normal',chunk=mode==='search'?1800:700;
    if(mode==='search'&&searchInitialLoading)return;
    if(rowCacheMode!==mode)return;
    var currentStart=visibleStart(),currentEnd=currentStart+visibleCount();
    var speedRows=Math.abs(scrollVelocityRows);
    var lookAhead=Math.max(chunk,Math.min(chunk*6,visibleCount()+Math.ceil(speedRows*0.85)));
    var direction=scrollDirection||1,missing=-1;
    if(direction>=0){
      missing=firstMissingRow(currentStart,Math.min(totalRows,currentEnd+lookAhead));
      if(missing<0)missing=lastMissingRow(Math.max(0,currentStart-Math.floor(lookAhead*0.35)),currentEnd);
    }else{
      missing=lastMissingRow(Math.max(0,currentStart-lookAhead),currentEnd);
      if(missing<0)missing=firstMissingRow(currentStart,Math.min(totalRows,currentEnd+Math.floor(lookAhead*0.35)));
    }
    if(missing<0)return;
    var start=Math.max(0,Math.floor(missing/chunk)*chunk);
    prefetchPending=true;
    vscode.postMessage({type:'prefetchRows',start:start,count:chunk,mode:mode,revision:searchRevision});
  },0);
}
function requestRows(force){
  var start=visibleStart(),count=visibleCount(),mode=filter?'search':'normal';
  if(mode==='search'&&searchInitialLoading&&totalRows===0)return;
  var missingVisible=totalRows>0&&firstMissingRow(start,start+count)>=0;
  if(!force&&!missingVisible&&start===lastRequestedStart&&count===lastRequestedCount&&mode===lastRequestMode)return;
  if(rowCacheMode!==mode)resetRowCache(mode);
  lastRequestedStart=start;lastRequestedCount=count;lastRequestMode=mode;
  rowRequestPending=true;
  if(totalRows>0){
    rowWindowStart=start;
    flatRows=cachedWindow(start,count);
    renderFlatRows();
  }
  var speedRows=Math.abs(scrollVelocityRows);
  var maxRequest=mode==='search'?2600:1400;
  var requestCount=Math.max(count,Math.min(maxRequest,count+Math.ceil(speedRows*1.2)+(mode==='search'?900:400)));
  vscode.postMessage({type:'getRows',start:start,count:requestCount,mode:mode,revision:searchRevision});
}
function canDrag(n){return !!n&&n.kind!=='service'&&n.canMove!==false}
function isDescendant(id,ancestorId){
  var n=nodes[id];
  while(n&&n.parentId){if(n.parentId===ancestorId)return true;n=nodes[n.parentId]}
  return false;
}
function canDrop(dragId,targetId){
  var drag=nodes[dragId],target=nodes[targetId];
  return canDrag(drag)&&!!target&&dragId!==targetId&&!isDescendant(targetId,dragId);
}
function clearDropTarget(){
  if(dropId){
    var old=rowEl(dropId);
    if(old)old.classList.remove('drop-target');
  }
  dropId=null;
}
function markDropTarget(id){
  if(!id)return;
  if(dropId!==id){
    clearDropTarget();
    dropId=id;
  }
  var row=rowEl(id);
  if(externalPackageDrag)debugPackageDrag('target row '+id+' rendered='+(row?'1':'0')+' windowStart='+rowWindowStart+' flatRows='+flatRows.length+' totalRows='+totalRows);
  if(row)row.classList.add('drop-target');
}
function rememberPointerRow(row){
  if(!row||!row.dataset||!row.dataset.id||!nodes[row.dataset.id])return null;
  lastPointerRowId=row.dataset.id;
  return lastPointerRowId;
}
function packageDropTargetFromState(preferPointer){
  if(dropId&&nodes[dropId])return dropId;
  if(preferPointer&&lastPointerRowId&&nodes[lastPointerRowId])return lastPointerRowId;
  if(selectedId&&nodes[selectedId])return selectedId;
  if(lastPointerRowId&&nodes[lastPointerRowId])return lastPointerRowId;
  return null;
}
function markPackageFallbackTarget(reason){
  var targetId=packageDropTargetFromState();
  if(targetId){
    if(externalPackageDrag)debugPackageDrag(reason+': fallback target '+targetId);
    markDropTarget(targetId);
  }
  else clearDropTarget();
  return targetId;
}
var screenOffsetWX=null,screenOffsetWY=null;
if(typeof saved.screenOffsetX==='number'&&typeof saved.screenOffsetY==='number'){
  screenOffsetX=saved.screenOffsetX;screenOffsetY=saved.screenOffsetY;
  screenOffsetWX=typeof saved.screenOffsetWX==='number'?saved.screenOffsetWX:null;
  screenOffsetWY=typeof saved.screenOffsetWY==='number'?saved.screenOffsetWY:null;
}
function rememberPointerEvent(e){
  if(!e)return;
  if(typeof e.screenX==='number'&&typeof e.clientX==='number'){screenOffsetX=e.screenX-e.clientX;screenOffsetWX=typeof window.screenX==='number'?window.screenX:null;}
  if(typeof e.screenY==='number'&&typeof e.clientY==='number'){screenOffsetY=e.screenY-e.clientY;screenOffsetWY=typeof window.screenY==='number'?window.screenY:null;}
}
document.addEventListener('pointermove',rememberPointerEvent,{passive:true,capture:true});
function rowFromClientPoint(clientX,clientY,source){
  var x=Number(clientX)||0,y=Number(clientY)||0;
  var el=document.elementFromPoint(x,y);
  var row=el&&el.closest?el.closest('.row'):null;
  if(row&&tree.contains(row)){
    rememberPointerRow(row);
    if(externalPackageDrag)debugPackageDrag('row from '+source+' elementFromPoint '+row.dataset.id);
    return row;
  }
  var rect=tree.getBoundingClientRect();
  if(y<rect.top||y>rect.bottom){
    if(externalPackageDrag)debugPackageDrag('no row from '+source+': y outside tree y='+Math.round(y)+' top='+Math.round(rect.top)+' bottom='+Math.round(rect.bottom));
    return null;
  }
  var rowIndex=Math.floor((tree.scrollTop+y-rect.top)/ROW_HEIGHT);
  var localIndex=rowIndex-rowWindowStart;
  var item=localIndex>=0&&localIndex<flatRows.length?flatRows[localIndex]:null;
  if(!item||item.type!=='node'||!item.id){
    if(externalPackageDrag)debugPackageDrag('no row from '+source+': rowIndex='+rowIndex+' local='+localIndex+' windowStart='+rowWindowStart+' flatRows='+flatRows.length+' total='+totalRows);
    return null;
  }
  lastPointerRowId=item.id;
  if(externalPackageDrag)debugPackageDrag('row from '+source+' coordinates '+item.id+' rowIndex='+rowIndex);
  return rowEl(item.id);
}
function dragEventRow(e){
  rememberPointerEvent(e);
  return rowFromClientPoint(e.clientX||0,e.clientY||0,'event');
}
function screenCursorRow(screenX,screenY,bounds){
  var x=Number(screenX),y=Number(screenY),candidates=[];
  if(!isFinite(x)||!isFinite(y)){
    debugPackageDrag('cursor invalid screen='+screenX+','+screenY);
    return null;
  }
  var wx=typeof window.screenX==='number'?window.screenX:(typeof window.screenLeft==='number'?window.screenLeft:0);
  var wy=typeof window.screenY==='number'?window.screenY:(typeof window.screenTop==='number'?window.screenTop:0);
  if(typeof screenOffsetX==='number'&&typeof screenOffsetY==='number'){
    var adjX=screenOffsetX+(typeof screenOffsetWX==='number'?wx-screenOffsetWX:0);
    var adjY=screenOffsetY+(typeof screenOffsetWY==='number'?wy-screenOffsetWY:0);
    candidates.push({source:'cursor calibrated',x:x-adjX,y:y-adjY});
  }
  if(bounds){
    var left=Number(bounds.left),top=Number(bounds.top),right=Number(bounds.right),bottom=Number(bounds.bottom);
    var width=right-left,height=bottom-top;
    if(isFinite(left)&&isFinite(top)&&width>0&&height>0&&x>=left&&x<=right&&y>=top&&y<=bottom){
      var scaleX=width/(window.innerWidth||width);
      var scaleY=height/(window.innerHeight||height);
      if(scaleX>=0.5&&scaleX<=4&&scaleY>=0.5&&scaleY<=4){
        candidates.push({source:'cursor hwnd',x:(x-left)/(scaleX||1),y:(y-top)/(scaleY||1)});
      }
      else debugPackageDrag('cursor hwnd rejected scale='+scaleX.toFixed(2)+','+scaleY.toFixed(2)+' hwnd='+left+','+top+','+right+','+bottom+' inner='+window.innerWidth+','+window.innerHeight);
    }
    else debugPackageDrag('cursor outside hwnd screen='+x+','+y+' hwnd='+left+','+top+','+right+','+bottom);
  }
  candidates.push({source:'cursor window',x:x-wx,y:y-wy});
  debugPackageDrag('cursor candidates='+candidates.map(function(c){return c.source+':'+Math.round(c.x)+','+Math.round(c.y)}).join('|')+' treeTop='+Math.round(tree.getBoundingClientRect().top)+' scroll='+Math.round(tree.scrollTop));
  for(var i=0;i<candidates.length;i++){
    var c=candidates[i];
    var row=rowFromClientPoint(c.x,c.y,c.source);
    if(row)return row;
  }
  return null;
}
function droppedModelPaths(dataTransfer){
  var out=[],seen={};
  function add(value){
    var text=String(value||'').trim();
    if(!/\\.(rbxm|rbxmx)$/i.test(text))return;
    var key=text.toLowerCase();
    if(seen[key])return;
    seen[key]=1;
    out.push(text);
  }
  if(!dataTransfer)return out;
  var files=dataTransfer.files;
  if(files){
    for(var i=0;i<files.length;i++)add(files[i]&&files[i].path);
  }
  var uriList='';
  try{uriList=dataTransfer.getData('text/uri-list')||''}catch(_){uriList=''}
  uriList.split(/\\r?\\n/).forEach(function(line){
    var text=String(line||'').trim();
    if(text&&text.charAt(0)!=='#')add(text);
  });
  return out;
}
function hasExternalFileData(dataTransfer){
  if(!dataTransfer)return false;
  var types=dataTransfer.types;
  if(!types)return false;
  for(var i=0;i<types.length;i++)if(types[i]==='Files')return true;
  return false;
}
function hasPackageDragData(dataTransfer){
  if(externalPackageDrag)return true;
  if(!dataTransfer||!dataTransfer.types)return false;
  var hasText=false;
  for(var i=0;i<dataTransfer.types.length;i++){
    var type=String(dataTransfer.types[i]||'').toLowerCase();
    if(type==='application/vnd.renium.package')return true;
    if(type==='text/plain')hasText=true;
  }
  if(hasText){
    try{if(droppedPackage(dataTransfer)!==null)return true}catch(_){}
    if(!hasExternalFileData(dataTransfer))return true;
  }
  return false;
}
function droppedPackage(dataTransfer){
  if(externalPackageDrag)return externalPackageDrag;
  if(!dataTransfer)return null;
  var raw='';
  try{raw=dataTransfer.getData('application/vnd.renium.package')||''}catch(_){raw=''}
  if(!raw){
    try{raw=dataTransfer.getData('text/plain')||''}catch(_){raw=''}
  }
  raw=String(raw||'').trim();
  var prefix='renium-package:';
  if(raw.indexOf(prefix)===0)raw=raw.slice(prefix.length);
  if(!raw)return null;
  try{
    var parsed=JSON.parse(raw);
    if(parsed&&parsed.type==='renium-package'&&typeof parsed.id==='string'&&parsed.id.length>0){
      return {id:parsed.id,name:typeof parsed.name==='string'?parsed.name:parsed.id};
    }
  }catch(_){}
  return null;
}
function insertExternalPackage(targetId,reason){
  if(!externalPackageDrag||!targetId)return false;
  var pkg=externalPackageDrag;
  debugPackageDrag(reason+': inserting '+pkg.id+' mode='+(pkg.mode||'')+' into '+targetId);
  expanded.add(targetId);save();
  vscode.postMessage({type:'insertPackage',nodeId:targetId,linkId:pkg.id,name:pkg.name});
  externalPackageDrag=null;
  stopDragAutoScroll();
  clearDropTarget();
  render();
  return true;
}
function requestLoad(id,force){
  var n=nodes[id];if(!n||loadingIds[id])return;
  if(!force&&!n.hasChildren)return;
  var anchor=captureScrollAnchor(id);
  var delayUntil=loadDelayUntil[id]||0;
  if(delayUntil>Date.now()){
    setTimeout(function(){requestLoad(id,force)},delayUntil-Date.now());
    return;
  }
  loadingIds[id]=true;
  vscode.postMessage({type:'expandNode',nodeId:id,mode:filter?'search':'normal',start:visibleStart(),count:visibleCount()});
  render(anchor);
}
function compactText(value){return String(value===undefined||value===null?'':value).toLowerCase().replace(/\\s+/g,'')}
function displayText(value){
  if(value===undefined||value===null)return '';
  if(typeof value==='string'||typeof value==='number'||typeof value==='boolean')return String(value);
  if(value&&typeof value==='object'&&!Array.isArray(value)&&value._type==='EnumItem')return String(value.name||'');
  try{return JSON.stringify(value)}catch(_){return String(value)}
}
function tokenize(query){
  var out=[],re=/"([^"]*)"|'([^']*)'|(\\S+)/g,m;
  while((m=re.exec(query))!==null)out.push(m[1]!==undefined?m[1]:m[2]!==undefined?m[2]:m[3]);
  return out;
}
function splitOr(tokens){
  var groups=[[]];
  tokens.forEach(function(token){
    if(token.toLowerCase()==='or')groups.push([]);
    else if(token.toLowerCase()!=='and')groups[groups.length-1].push(token.replace(/^\\(+|\\)+$/g,''));
  });
  return groups.filter(function(group){return group.length>0});
}
function resetSearchResults(){searchResultsFilter=null;searchVisibleSet=new Set();searchResultIds=[];subtreeMatchCache={}}
function invalidateSearchCache(){searchPlanFilter=null;searchPlanGroups=[];selfMatchCache={};resetSearchResults()}
function invalidateSearchIndex(){searchIndexDirty=true;searchEntries={};searchEntryIds=[];invalidateSearchCache()}
function searchGroups(){
  if(searchPlanFilter!==filter){
    searchPlanFilter=filter;
    searchPlanGroups=filter?splitOr(tokenize(filter)):[];
    selfMatchCache={};
    resetSearchResults();
  }
  return searchPlanGroups;
}
function searchableRecord(n){
  var s=n.search||{},props={};
  function copyRecord(record){
    if(Array.isArray(record)){
      for(var i=0;i<record.length;i++){
        var pair=record[i];
        if(pair&&pair.length>=2&&props[pair[0]]===undefined)props[pair[0]]=pair[1];
      }
      return;
    }
    Object.keys(record||{}).forEach(function(key){if(props[key]===undefined)props[key]=record[key]});
  }
  copyRecord(s.properties||{});
  copyRecord(s.attributes||{});
  props.Name=s.name||n.name;props.ClassName=s.className||n.className;props.Parent=(s.path||'').split('.').slice(-2,-1)[0]||'';
  return props;
}
function buildSearchEntry(n){
  var s=n.search||{},props={};
  function addRecord(record){
    if(Array.isArray(record)){
      for(var i=0;i<record.length;i++){
        var pair=record[i];
        if(pair&&pair.length>=2)props[compactText(pair[0])]=pair[1];
      }
      return;
    }
    Object.keys(record||{}).forEach(function(key){
      var value=record[key],compactKey=compactText(key);
      props[compactKey]=value;
    });
  }
  addRecord(s.properties||{});
  addRecord(s.attributes||{});
  props.name=s.name||n.name;
  props.classname=s.className||n.className;
  props.parent=(s.path||'').split('.').slice(-2,-1)[0]||'';
  var pathParts=String(s.path||'').split('.').filter(Boolean).map(compactText);
  var classChain=(Array.isArray(s.classChain)&&s.classChain.length?s.classChain:[n.className]).map(compactText);
  var tags=(Array.isArray(s.tags)?s.tags:[]).map(compactText);
  return {
    id:n.id||n.treeId||n.name,
    name:compactText(s.name||n.name),
    className:compactText(s.className||n.className),
    classChain:classChain,
    pathParts:pathParts,
    tags:tags,
    props:props
  };
}
function ensureSearchIndex(){
  if(!searchIndexDirty)return;
  searchEntries={};
  searchEntryIds=[];
  var ids=Object.keys(nodes);
  for(var i=0;i<ids.length;i++){
    var id=ids[i],n=nodes[id];
    if(n){searchEntries[id]=buildSearchEntry(n);searchEntryIds.push(id)}
  }
  searchIndexDirty=false;
  resetSearchResults();
}
function searchEntryFor(n){
  ensureSearchIndex();
  return n?searchEntries[n.id||n.treeId||n.name]:undefined;
}
function findProperty(n,name){
  var wanted=compactText(name),entry=searchEntryFor(n);
  if(entry&&Object.prototype.hasOwnProperty.call(entry.props,wanted))return entry.props[wanted];
  var record=searchableRecord(n),keys=Object.keys(record);
  for(var i=0;i<keys.length;i++)if(compactText(keys[i])===wanted)return record[keys[i]];
  return undefined;
}
function propertyCompare(n,prop,op,expected){
  var actual=findProperty(n,prop);
  if(actual===undefined)return false;
  var aNum=Number(displayText(actual)),eNum=Number(expected);
  if((op==='<'||op==='>'||op==='<='||op==='>=')&&isFinite(aNum)&&isFinite(eNum)){
    if(op==='<')return aNum<eNum;
    if(op==='>')return aNum>eNum;
    if(op==='<=')return aNum<=eNum;
    return aNum>=eNum;
  }
  var actualText=compactText(displayText(actual)),expectedText=compactText(expected);
  if(op==='!='||op==='~=')return actualText.indexOf(expectedText)<0;
  return actualText.indexOf(expectedText)>=0;
}
function tagMatch(n,term){
  var entry=searchEntryFor(n),tags=entry?entry.tags:((n.search&&n.search.tags)||[]).map(compactText);
  term=compactText(term);
  for(var i=0;i<tags.length;i++)if(tags[i].indexOf(term)>=0)return true;
  return false;
}
function classMatch(n,term){
  var entry=searchEntryFor(n),chain=entry?entry.classChain:((n.search&&n.search.classChain)||[n.className]).map(compactText);
  term=compactText(term);
  for(var i=0;i<chain.length;i++)if(chain[i]===term)return true;
  return false;
}
function ancestryMatch(n,pattern){
  var entry=searchEntryFor(n),path=entry?entry.pathParts:((n.search&&n.search.path)||'').split('.').filter(Boolean).map(compactText);
  var parts=pattern.split('.').filter(Boolean).map(compactText);
  if(parts.length===0||path.length===0)return false;
  function at(pi,si){
    if(pi===parts.length)return si===path.length;
    if(parts[pi]==='**')return true;
    if(si>=path.length)return false;
    if(parts[pi]==='*'||parts[pi]===path[si])return at(pi+1,si+1);
    return false;
  }
  for(var start=0;start<path.length;start++)if(at(0,start))return true;
  return false;
}
function nameMatch(n,term){
  var entry=searchEntryFor(n);
  return (entry?entry.name:compactText((n.search&&n.search.name)||n.name)).indexOf(compactText(term))>=0;
}
function tokenMatch(n,tokens,index){
  var token=tokens[index]||'',next=tokens[index+1],next2=tokens[index+2];
  if(!token)return {ok:true,next:index+1};
  var colon=token.indexOf(':');
  if(colon>0){
    var prefix=token.slice(0,colon).toLowerCase(),value=token.slice(colon+1);
    if(prefix==='is')return {ok:classMatch(n,value),next:index+1};
    if(prefix==='tag')return {ok:tagMatch(n,value),next:index+1};
  }
  if(next&&(next==='='||next==='=='||next==='!='||next==='~='||next==='<'||next==='>'||next==='<='||next==='>=')){
    return {ok:propertyCompare(n,token,next,next2||''),next:index+3};
  }
  var inline=token.match(/^([^=!<>~]+)(==|=|!=|~=|<=|>=|<|>)(.+)$/);
  if(inline)return {ok:propertyCompare(n,inline[1],inline[2],inline[3]),next:index+1};
  if(token.indexOf('.')>=0||token==='*'||token==='**')return {ok:ancestryMatch(n,token),next:index+1};
  return {ok:nameMatch(n,token),next:index+1};
}
function matchesGroup(n,tokens){
  for(var i=0;i<tokens.length;){
    var result=tokenMatch(n,tokens,i);
    if(!result.ok)return false;
    i=Math.max(result.next,i+1);
  }
  return true;
}
function matchesSelf(n){
  if(!filter)return true;
  if(n.search&&n.search.hostMatch===true)return true;
  var id=n.id||n.treeId||n.name;
  if(Object.prototype.hasOwnProperty.call(selfMatchCache,id))return selfMatchCache[id];
  var groups=searchGroups(),ok=false;
  for(var i=0;i<groups.length;i++)if(matchesGroup(n,groups[i])){ok=true;break}
  selfMatchCache[id]=ok;
  return ok;
}
function fastNameGroups(){
  var groups=searchGroups();
  if(!groups.length)return null;
  var out=[];
  for(var i=0;i<groups.length;i++){
    var group=groups[i],terms=[];
    for(var j=0;j<group.length;j++){
      var token=group[j];
      if(!token||token.indexOf(':')>=0||/[=!<>~]/.test(token)||token.indexOf('.')>=0||token==='*'||token==='**')return null;
      terms.push(compactText(token));
    }
    if(!terms.length)return null;
    out.push(terms);
  }
  return out;
}
function fastNameMatch(entry,groups){
  for(var i=0;i<groups.length;i++){
    var terms=groups[i],ok=true;
    for(var j=0;j<terms.length;j++){
      if(entry.name.indexOf(terms[j])<0){ok=false;break}
    }
    if(ok)return true;
  }
  return false;
}
function markSearchVisible(n){
  while(n){
    searchVisibleSet.add(n.id);
    if(!n.parentId)break;
    n=nodes[n.parentId];
  }
}
function ensureSearchResults(){
  if(!filter){
    if(searchResultsFilter!==filter)resetSearchResults();
    searchResultsFilter=filter;
    return;
  }
  if(searchResultsFilter===filter&&!searchIndexDirty)return;
  ensureSearchIndex();
  var fastGroups=fastNameGroups();
  searchVisibleSet=new Set();
  searchResultIds=[];
  for(var i=0;i<searchEntryIds.length;i++){
    var id=searchEntryIds[i],n=nodes[id];
    if(!n)continue;
    var ok=fastGroups?fastNameMatch(searchEntries[id],fastGroups):matchesSelf(n);
    if(fastGroups)selfMatchCache[id]=ok;
    if(ok){
      searchResultIds.push(id);
      markSearchVisible(n);
    }
  }
  searchResultsFilter=filter;
}
function matches(id){
  var n=nodes[id]; if(!n)return false;
  if(!filter)return true;
  ensureSearchResults();
  return searchVisibleSet.has(id);
}
function isSearchOpen(id){
  if(!filter)return expanded.has(id);
  return !searchExpanded.has(id);
}
function updateSearchMeta(){
  if(!filter){
    searchMeta.classList.remove('active');
    return;
  }
  searchMeta.classList.add('active');
  if(searchLoading){
    var progress=searchTotal?('Loading '+searchLoaded+'/'+searchTotal+'... '):'Loading... ';
    searchSummary.textContent=searchMatchCount?searchMatchCount+' '+(searchMatchCount===1?'match':'matches'):progress;
  }else{
    searchSummary.textContent=searchMatchCount+' '+(searchMatchCount===1?'match':'matches');
  }
}
function collectRow(id,depth,out){
  var n=nodes[id]; if(!n||!matches(id))return;
  var kids=n.children||[],has=n.hasChildren||kids.length>0,open=isSearchOpen(id);
  out.push({type:'node',id:id,depth:depth});
  if(open){
    if(has&&kids.length===0){
      if(!loadingIds[id])autoLoadIds.push(id);
      out.push({type:'placeholder',loadId:id,depth:depth+1});
    }
    for(var i=0;i<kids.length;i++)collectRow(kids[i],depth+1,out);
  }
}
function rowHtml(item){
  if(item.type==='loading'){
    return '<div class="row placeholder" style="padding-left:'+(item.depth*12)+'px"></div>';
  }
  if(item.type==='placeholder'){
    return '<div class="row placeholder" data-load="'+esc(item.loadId)+'" style="padding-left:'+(item.depth*12)+'px"><span class="twisty leaf"></span><span class="name">Loading...</span></div>';
  }
  var id=item.id,n=nodes[id]||item; if(!n)return '';
  var has=!!n.hasChildren,open=!!n.expanded,renaming=renameId===id;
  var reniumState=directReniumState(n),reniumClass=reniumState?' renium-'+reniumState.kind:'';
  var out=[];
  out.push('<div class="row'+reniumClass+(selectedId===id?' selected':'')+(referencePreviewId===id?' reference-preview':'')+(allMatchesSelected&&n.matched?' match-selected':'')+(dropId===id?' drop-target':'')+(n.disabled?' disabled':'')+'" data-id="'+esc(id)+'" draggable="'+(!renaming&&canDrag(n)?'true':'false')+'" style="padding-left:'+(item.depth*12)+'px">');
  out.push('<span class="twisty '+(has?(open?'open':''):'leaf')+'"></span>');
  out.push('<img class="icon" src="'+ASSET+'/'+esc(n.iconName||iconName(n.className))+'.png">');
  if(renaming){
    out.push('<input class="rename" spellcheck="false" draggable="false" value="'+esc(n.name)+'">');
  }else{
    out.push('<span class="labelWrap"><span class="name">'+esc(n.name)+'</span>'+reniumBadgeHtml(reniumState)+'<button class="addBtn" type="button" title="Add child" aria-label="Add child"></button></span>');
  }
  out.push('</div>');
  return out.join('');
}
function renderFlatRows(){
  if(totalRows===0){
    tree.innerHTML=currentEmptyHtml;
    return;
  }
  var out=[];
  if(rowWindowStart>0)out.push('<div style="height:'+(rowWindowStart*ROW_HEIGHT)+'px"></div>');
  for(var i=0;i<flatRows.length;i++){
    var html=rowHtml(flatRows[i]);
    if(html)out.push(html);
  }
  var remaining=Math.max(0,totalRows-rowWindowStart-flatRows.length);
  if(remaining>0)out.push('<div style="height:'+(remaining*ROW_HEIGHT)+'px"></div>');
  tree.innerHTML=out.join('');
}
function scheduleVisibleRows(){
  updateScrollVelocity();
  if(visibleRenderFrame||renameId)return;
  visibleRenderFrame=requestAnimationFrame(function(){
    visibleRenderFrame=0;
    requestRows(false);
    schedulePrefetch();
  });
}
function stopDragAutoScroll(){
  dragAutoScrollDirection=0;
  if(dragAutoScrollFrame){
    cancelAnimationFrame(dragAutoScrollFrame);
    dragAutoScrollFrame=0;
  }
}
function startDragAutoScroll(){
  if(dragAutoScrollFrame||!dragAutoScrollDirection)return;
  dragAutoScrollFrame=requestAnimationFrame(function(){
    dragAutoScrollFrame=0;
    if(!draggedId||!dragAutoScrollDirection)return;
    var rect=tree.getBoundingClientRect();
    var threshold=Math.max(24,Math.min(56,rect.height*0.12));
    var distance=dragAutoScrollDirection<0
      ? Math.max(0,dragAutoScrollPointerY-rect.top)
      : Math.max(0,rect.bottom-dragAutoScrollPointerY);
    var strength=Math.max(0,Math.min(1,(threshold-distance)/threshold));
    if(strength<=0)return;
    var maxScroll=Math.max(8,ROW_HEIGHT*0.9);
    var delta=Math.max(2,Math.round(maxScroll*strength))*dragAutoScrollDirection;
    var previousTop=tree.scrollTop;
    var nextTop=Math.max(0,Math.min(tree.scrollHeight-tree.clientHeight,previousTop+delta));
    if(nextTop!==previousTop){
      tree.scrollTop=nextTop;
      scheduleVisibleRows();
    }
    if(draggedId&&dragAutoScrollDirection){
      startDragAutoScroll();
    }
  });
}
function updateDragAutoScroll(clientY){
  dragAutoScrollPointerY=clientY;
  if(!draggedId){
    stopDragAutoScroll();
    return;
  }
  var rect=tree.getBoundingClientRect();
  var threshold=Math.max(24,Math.min(56,rect.height*0.12));
  if(clientY<=rect.top+threshold)dragAutoScrollDirection=-1;
  else if(clientY>=rect.bottom-threshold)dragAutoScrollDirection=1;
  else dragAutoScrollDirection=0;
  if(dragAutoScrollDirection)startDragAutoScroll();
  else stopDragAutoScroll();
}
function render(anchor){
  var scrollAnchor=anchor||captureScrollAnchor();
  currentEmptyHtml=filter?'<div id="treeEmpty">No matches found.</div>':'<div id="treeEmpty">No services found in src.</div>';
  renderFlatRows();
  updateSearchMeta();
  restoreScrollAnchor(scrollAnchor);
  renderFlatRows();
  if(renameId){setTimeout(function(){var el=rowEl(renameId);var input=el&&el.querySelector('.rename');if(input){input.focus();input.select()}},0)}
}
function scheduleRender(anchor){
  if(anchor)pendingRenderAnchor=anchor;
  if(renderFrame)return;
  renderFrame=requestAnimationFrame(function(){
    renderFrame=0;
    var nextAnchor=pendingRenderAnchor;
    pendingRenderAnchor=null;
    render(nextAnchor);
  });
}
function applySelection(id,post){
  var preview=referencePreviewId;
  referencePreviewId=null;
  if(preview&&preview!==id){
    var previewRow=rowEl(preview);
    if(previewRow)previewRow.classList.remove('reference-preview');
  }
  var previous=selectedId;
  selectedId=id;
  save();
  if(previous&&previous!==id){
    var old=rowEl(previous);
    if(old)old.classList.remove('selected');
  }
  var current=rowEl(id);
  if(current)current.classList.add('selected');else render();
  if(externalPackageDrag&&!draggedId)markDropTarget(id);
  if(post){lastHostSelectionId=id;vscode.postMessage({type:'selectNode',nodeId:id})}
}
function clearSelection(){
  var previous=selectedId,preview=referencePreviewId;
  selectedId=null;
  referencePreviewId=null;
  lastHostSelectionId=null;
  closeMenus();
  save();
  if(previous){
    var old=rowEl(previous);
    if(old)old.classList.remove('selected');
  }
  if(preview){
    var previewRow=rowEl(preview);
    if(previewRow)previewRow.classList.remove('reference-preview');
  }
}
function scrollToId(id){
  var index=flatRowIndex(id);
  if(index>=0){
    var top=index*ROW_HEIGHT,bottom=top+ROW_HEIGHT;
    if(top<tree.scrollTop)tree.scrollTop=top;
    else if(bottom>tree.scrollTop+tree.clientHeight)tree.scrollTop=Math.max(0,bottom-tree.clientHeight);
    renderFlatRows();
  }
}
function selectNode(id){
  tree.focus();applySelection(id,true);
}
function startRename(id){
  var n=nodes[id];if(!n||n.canRename===false)return;
  renameId=id;renameOriginal=n.name;selectedId=id;closeMenus();render();
}
function finishRename(input,shouldRender){
  if(!renameId)return false;
  var id=renameId,value=input.value;
  var original=renameOriginal;
  renameId=null;renameOriginal='';renamePointerStartedInside=false;renameSuppressFocusoutUntil=0;
  if(shouldRender)render();
  if(value&&value!==original)vscode.postMessage({type:'renameInstance',nodeId:id,newName:value});
  return true;
}
function cancelRename(shouldRender){
  if(!renameId)return false;
  renameId=null;renameOriginal='';renamePointerStartedInside=false;renameSuppressFocusoutUntil=0;
  if(shouldRender)render();
  return true;
}
function currentRenameInput(){
  if(!renameId)return null;
  return tree.querySelector('.row[data-id="'+CSS.escape(renameId)+'"] .rename');
}
function keepRenameInputFocused(){
  var expectedId=renameId;
  setTimeout(function(){
    if(!expectedId||renameId!==expectedId)return;
    var input=currentRenameInput();
    if(input&&document.activeElement!==input)input.focus();
  },0);
}
function cleanupStaleRenameInput(){
  if(!renameId&&tree.querySelector('.rename'))render();
}
function finishPointerRenameCleanup(){
  suppressRenameFocusoutRender=false;
  renamePointerStartedInside=false;
  renameSuppressFocusoutUntil=0;
  cleanupStaleRenameInput();
}
function renderClassList(){
  var q=classSearch.value.trim().toLowerCase();
  var ordered=orderedClassNamesForParent(addParentId),found=[];
  for(var i=0;i<ordered.length;i++){
    var name=ordered[i];
    if(!q||name.toLowerCase().indexOf(q)>=0)found.push(name);
  }
  if(classActive>=found.length)classActive=0;
  var html='';
  for(var j=0;j<found.length;j++){
    html+='<div class="classItem'+(j===classActive?' active':'')+'" data-class="'+esc(found[j])+'"><img class="icon" src="'+ASSET+'/'+esc(iconName(found[j]))+'.png"><span>'+esc(found[j])+'</span></div>';
  }
  classList.innerHTML=html||'<div class="classItem">No classes</div>';
}
function showClassPicker(x,y,parentId){
  addParentId=parentId;classActive=0;classSearch.value='';renderClassList();
  var left=Math.max(4,Math.min(x,window.innerWidth-250));
  var top=Math.max(4,Math.min(y,window.innerHeight-330));
  classPicker.style.left=left+'px';classPicker.style.top=top+'px';classPicker.classList.remove('hidden');
  setTimeout(function(){classSearch.focus()},0);
}
function showClassPickerForNode(id){
  var el=rowEl(id),rect=el?el.getBoundingClientRect():tree.getBoundingClientRect();
  showClassPicker(rect.left+18,rect.top+22,id);
}
function showClassPickerForButton(button,id){
  var rect=button.getBoundingClientRect();
  showClassPicker(rect.left-8,rect.bottom+4,id);
}
function createClass(className){
  if(!addParentId||!className)return;
  expanded.add(addParentId);save();
  vscode.postMessage({type:'createInstance',nodeId:addParentId,className:className,name:className});
  classPicker.classList.add('hidden');
}
window.addEventListener('message',function(e){
  var m=e.data;
  if(typeof m.hasClipboardInstance==='boolean')hasClipboardInstance=!!m.hasClipboardInstance;
  if(m.type==='storeTree'){storeOnMessage(m);return}
  if(m.type==='prepareReferencePreview'){prepareReferencePreview();return}
  if(m.type==='packageDrag'){
    var link=m.link;
    externalPackageDrag=link&&typeof link.id==='string'&&link.id.length>0
      ? {id:link.id,name:typeof link.name==='string'?link.name:link.id,mode:typeof link.mode==='string'?link.mode:'armed'}
      : null;
    packageDragCursorSawDown=false;
    debugPackageDrag(externalPackageDrag?'received packageDrag '+externalPackageDrag.id+' mode='+externalPackageDrag.mode:'received packageDrag clear');
    if(externalPackageDrag){
      markPackageFallbackTarget('packageDrag armed');
      render();
    }
    else{
      clearDropTarget();
      render();
    }
    return;
  }
  if(m.type==='packageDragCursor'){
    if(!externalPackageDrag||draggedId)return;
    var leftDown=m.leftButtonDown===true;
    if(externalPackageDrag.mode==='drag'&&leftDown)packageDragCursorSawDown=true;
    var r=screenCursorRow(m.screenX,m.screenY,{left:m.windowLeft,top:m.windowTop,right:m.windowRight,bottom:m.windowBottom});
    debugPackageDrag('cursor result row='+(r&&r.dataset?r.dataset.id:'none')+' screen='+m.screenX+','+m.screenY+' button='+(leftDown?1:0));
    if(r)markDropTarget(r.dataset.id);
    else markPackageFallbackTarget('cursor poll');
    if(externalPackageDrag&&externalPackageDrag.mode==='drag'&&packageDragCursorSawDown&&!leftDown){
      packageDragCursorSawDown=false;
      var targetId=r&&r.dataset?r.dataset.id:null;
      if(targetId)insertExternalPackage(targetId,'cursor release');
      else{
        debugPackageDrag('cursor release: no row, cancel package drag');
        externalPackageDrag=null;
        clearDropTarget();
        render();
        vscode.postMessage({type:'cancelPackageDrag'});
      }
    }
    return;
  }
  if(m.type==='expandInserted'){var ei=nodes[m.nodeId];if(ei){expanded.add(m.nodeId);save();requestLoad(m.nodeId,true);render()}return}
  if(m.type==='linkState'){linkKeys=m.keys||{};render();return}
  if(m.type==='optimisticDelete'){optimisticDelete(m.id);return}
  if(m.type==='clearSelection'){clearSelection();return}
  if(m.type==='setTab'){setActiveTab(m.tab,true);return}
  if(m.type==='gitState'){gitState=m.state||null;gitLoading=!!m.loading;gitProjectRoot=String(m.projectRoot||'');gitGeneration=Number(m.generation||0);if(activeTab==='git')renderGit();return}
  if(m.type==='updateTree'){var anchor=captureScrollAnchor();nodes=m.nodes||{};rootIds=m.rootIds||[];if(m.selectedId)lastHostSelectionId=m.selectedId;selectedId=m.selectedId||selectedId;invalidateSearchIndex();Object.keys(loadingIds).forEach(function(id){var n=nodes[id];if(!n||n.loaded||(n.children&&n.children.length>0))delete loadingIds[id]});save();scheduleRender(anchor);syncSelectionToHost()}
  else if(m.type==='rowsWindow'){
    if(m.scrollToReferencePreview)prepareReferencePreview();
    var expectedMode=filter?'search':'normal';
    if(m.mode&&m.mode!==expectedMode)return;
    if(filter&&typeof m.revision==='number'&&m.revision!==searchRevision)return;
    backendErrorRetryCount=0;
    if(rowCacheMode!==expectedMode)resetRowCache(expectedMode);
    var anchor=captureScrollAnchor();
    rowRequestPending=false;
    rowWindowStart=typeof m.start==='number'?m.start:0;
    totalRows=typeof m.totalRows==='number'?m.totalRows:0;
    var receivedRows=Array.isArray(m.rows)?m.rows:[];
    for(var rowIndex=0;rowIndex<receivedRows.length;rowIndex++){
      rowCache[rowWindowStart+rowIndex]=receivedRows[rowIndex];
    }
    rememberRows(receivedRows);
    pruneRowCache(rowWindowStart,Math.max(lastRequestedCount,receivedRows.length));
    flatRows=cachedWindow(rowWindowStart,visibleCount());
    if(m.selectedId)selectedId=m.selectedId;
    referencePreviewId=typeof m.referencePreviewId==='string'?m.referencePreviewId:null;
    if(Array.isArray(m.matchIds))matchIds=m.matchIds;
    if(typeof m.matchCount==='number')searchMatchCount=m.matchCount;
    if(filter){searchLoading=false;searchInitialLoading=false}
    searchLoaded=typeof m.loaded==='number'?m.loaded:searchLoaded;
    searchTotal=typeof m.total==='number'?m.total:searchTotal;
    currentEmptyHtml=filter?'<div id="treeEmpty">No matches found.</div>':'<div id="treeEmpty">No services found in src.</div>';
    save();render(anchor);syncSelectionToHost();
    if(m.scrollToReferencePreview&&referencePreviewId){setTimeout(function(){scrollToId(referencePreviewId)},0)}
    if(m.scrollToSelected&&selectedId){setTimeout(function(){scrollToId(selectedId);if(document.activeElement===search||Date.now()<searchRetainFocusUntil){if(document.activeElement!==search)searchRestoringFocus=true;search.focus();return}tree.focus()},0)}
    schedulePrefetch();
  }
  else if(m.type==='rowsPrefetch'){
    var expectedPrefetchMode=filter?'search':'normal';
    prefetchPending=false;
    if(m.mode&&m.mode!==expectedPrefetchMode)return;
    if(filter&&typeof m.revision==='number'&&m.revision!==searchRevision)return;
    if(rowCacheMode!==expectedPrefetchMode)return;
    var prefetchStart=typeof m.start==='number'?m.start:0;
    var prefetchRows=Array.isArray(m.rows)?m.rows:[];
    if(typeof m.totalRows==='number')totalRows=m.totalRows;
    var currentCount=lastRequestedCount||visibleCount();
    var affectsVisible=prefetchStart<rowWindowStart+currentCount&&prefetchStart+prefetchRows.length>rowWindowStart;
    for(var pi=0;pi<prefetchRows.length;pi++)rowCache[prefetchStart+pi]=prefetchRows[pi];
    rememberRows(prefetchRows);
    pruneRowCache(rowWindowStart,Math.max(currentCount,prefetchRows.length));
    if(affectsVisible){
      flatRows=cachedWindow(rowWindowStart,currentCount);
      renderFlatRows();
    }
    schedulePrefetch();
  }
	  else if(m.type==='rowsPrefetchDone'){prefetchPending=false;schedulePrefetch()}
	  else if(m.type==='invalidateRows'){lastRequestedStart=-1;requestRows(true)}
	  else if(m.type==='loadComplete'){var completeAnchor=captureScrollAnchor(m.nodeId);delete loadingIds[m.nodeId];if(m.ok===false)loadDelayUntil[m.nodeId]=Date.now()+1200;else delete loadDelayUntil[m.nodeId];render(completeAnchor)}
	  else if(m.type==='searchStatus'){searchLoading=!!m.loading;if(!searchLoading)searchInitialLoading=false;searchLoaded=typeof m.loaded==='number'?m.loaded:searchLoaded;searchTotal=typeof m.total==='number'?m.total:searchTotal;if(typeof m.matchCount==='number')searchMatchCount=m.matchCount;updateSearchMeta()}
	  else if(m.type==='historyEntries'){historyLoading=false;historyLoaded=true;historyGroups=Array.isArray(m.groups)?m.groups:(Array.isArray(m.entries)?m.entries.map(function(entry){return{id:entry.id,title:entry.targetLabel,subtitle:historyMetaText(entry),entryCount:1,targetCount:1,items:[entry]}}):[]);historyRestoring={};renderHistory()}
	  else if(m.type==='historyError'){historyLoading=false;historyLoaded=true;historyList.innerHTML='<div id="treeEmpty">'+esc(m.message||'Failed to load history.')+'</div>'}
	  else if(m.type==='historyRestoreComplete'){if(m.id)delete historyRestoring[m.id];if(m.groupId)delete historyRestoring[m.groupId];renderHistory()}
	  else if(m.type==='clipboardState'){if(!hasClipboardInstance)menu.classList.add('hidden')}
	  else if(m.type==='error'){
    var message=m.message||'Explorer failed to load.';
    rowRequestPending=false;
    lastRequestedStart=-1;
    if(/Explorer backend exited|timed out|not running/i.test(message)&&backendErrorRetryCount<2){
      backendErrorRetryCount++;
      setTimeout(function(){requestRows(true)},180);
      return;
    }
    searchLoading=false;searchInitialLoading=false;searchRequested=false;updateSearchMeta();tree.innerHTML='<div id="treeEmpty">'+esc(message)+'</div>';save()
  }
});
function startSearchLoad(force){
  if(!filter)return;
  if(force||!searchRequested){
    searchRequested=true;searchLoading=true;searchInitialLoading=true;searchLoaded=0;searchTotal=0;searchMatchCount=0;updateSearchMeta();
    if(searchDebounce)clearTimeout(searchDebounce);
    searchDebounce=setTimeout(function(){
      searchDebounce=null;
      lastRequestedStart=-1;
      lastRequestMode='search';
      tree.scrollTop=0;
      currentEmptyHtml='<div id="treeEmpty"></div>';
      if(totalRows===0){renderFlatRows()}
      vscode.postMessage({type:'searchLoad',query:filter,start:0,count:visibleCount(),mode:'search',revision:searchRevision});
    },force?0:23);
  }
}
search.addEventListener('input',function(){
  var nextFilter=search.value.trim().toLowerCase();
  var wasFiltering=!!filter;
  if(nextFilter!==filter){searchRevision++;prefetchPending=false;if(prefetchTimer){clearTimeout(prefetchTimer);prefetchTimer=null}searchInitialLoading=false;searchExpanded.clear();searchRequested=false;matchIds=[];resetRowCache(nextFilter?'search':'normal');rowWindowStart=0;totalRows=0;flatRows=[];lastRequestedStart=-1;lastRequestMode=nextFilter?'search':'normal'}
  filter=nextFilter;allMatchesSelected=false;invalidateSearchCache();
  hideSearchSuggestions();
  if(filter)startSearchLoad(false);else{if(searchDebounce)clearTimeout(searchDebounce);var hadFocus=document.activeElement===search;if(hadFocus)searchRetainFocusUntil=Date.now()+1500;searchLoading=false;searchInitialLoading=false;searchRequested=false;searchLoaded=0;searchTotal=0;searchMatchCount=0;rowWindowStart=0;totalRows=0;flatRows=[];tree.scrollTop=0;lastRequestedStart=-1;lastRequestMode='normal';resetRowCache('normal');currentEmptyHtml='<div id="treeEmpty">Loading...</div>';renderFlatRows();vscode.postMessage({type:'clearSearch',start:0,count:visibleCount(),mode:'normal'});if(wasFiltering&&selectedId)expandAncestors(selectedId);if(hadFocus)showSearchSuggestionsOnce();if(hadFocus)setTimeout(function(){if(document.activeElement!==search)searchRestoringFocus=true;search.focus()},0)}
  updateSearchMeta();
});
tree.addEventListener('scroll',scheduleVisibleRows);
tree.addEventListener('wheel',function(e){
  if(e.deltaY){
    scrollDirection=e.deltaY<0?-1:1;
    schedulePrefetch();
  }
},{passive:true});
search.addEventListener('mousedown',function(){searchPointerOpenUntil=Date.now()+600});
search.addEventListener('focus',function(){if(searchRestoringFocus){searchRestoringFocus=false;return}searchSuggestionsShownThisFocus=false;showSearchSuggestionsOnce()});
search.addEventListener('blur',function(){
  setTimeout(function(){
    if(document.activeElement===search)return;
    if(Date.now()<searchPointerOpenUntil){
      if(suggestions.classList.contains('active'))return;
    }
    hideSearchSuggestions();
    searchSuggestionsShownThisFocus=false;
  },80);
});
suggestions.addEventListener('mousedown',function(e){searchPointerOpenUntil=Date.now()+600;e.preventDefault()});
suggestions.addEventListener('click',function(e){
  var item=e.target.closest('.suggestItem');if(!item)return;
  search.value=item.dataset.insert||'';searchRevision++;filter=search.value.trim().toLowerCase();searchRequested=false;searchInitialLoading=false;prefetchPending=false;matchIds=[];resetRowCache(filter?'search':'normal');rowWindowStart=0;totalRows=0;flatRows=[];lastRequestedStart=-1;invalidateSearchCache();hideSearchSuggestions();search.focus();startSearchLoad(false);render();
});
function jumpMatch(delta){
  if(matchIds.length===0)return;
  var current=selectedId?matchIds.indexOf(selectedId):-1;
  if(current<0)current=delta>0?-1:0;
  var next=(current+delta+matchIds.length)%matchIds.length;
  selectNode(matchIds[next]);
  scrollToId(matchIds[next]);
  var row=rowEl(matchIds[next]);if(row)row.scrollIntoView({block:'nearest'});
}
prevMatch.addEventListener('click',function(){jumpMatch(-1)});
nextMatch.addEventListener('click',function(){jumpMatch(1)});
selectMatches.addEventListener('click',function(){allMatchesSelected=true;render()});
refreshResults.addEventListener('click',function(){searchRequested=false;startSearchLoad(true);render()});
tabs.addEventListener('click',function(e){
  var btn=e.target.closest('.tabBtn');if(!btn)return;
  setActiveTab(btn.dataset.tab);
});
${settingsStoreTreeRuntime}
var storeBrowser=createSettingsStoreTree({
  treeElement:storeTree,
  searchElement:storeSearch,
  assetBase:ASSET,
  iconNames:AVAILABLE_ICONS,
  rowPadding:0,
  fallbackHeight:300,
  emptyClass:'storeHint',
  errorClass:'rberr',
  emptyHtml:'<div class="storeHint">Open a <b>.renium</b> store with the folder button above, or <a id="storeBrowse" href="#">browse for a file</a>.</div>',
  isVisible:function(){return activeTab==='store'},
  onSelect:function(node){vscode.postMessage({type:'storeSelect',node:{name:node.name,className:node.className,settingsId:node.settingsId,properties:node.properties||{},attributes:node.attributes||{}}})}
});
storeTree.addEventListener('click',function(event){if(event.target.closest('#storeBrowse')){event.preventDefault();vscode.postMessage({type:'storeBrowse'})}});
function storeOnMessage(m){
  if(m.error){storeBrowser.setError('Could not read this file:\\n\\n'+m.error);return}
  storeBrowser.setTree(m.result);
}
function storeSendBytes(file){
  var maxBytes=${maxStoreDroppedBytes};
  if(!file||file.size>maxBytes){
    storeSearch.placeholder='Search';
    storeTree.innerHTML='<div class="rberr">Dropped files are limited to '+Math.floor(maxBytes/(1024*1024))+' MiB. Use the file picker for a larger store.</div>';
    return;
  }
  storeSearch.placeholder='decoding...';
  var reader=new FileReader();
  reader.onload=function(){var data=typeof reader.result==='string'?reader.result:'';var comma=data.indexOf(',');if(comma<0){storeSearch.placeholder='Search';return}vscode.postMessage({type:'storeDecode',name:file.name,base64:data.slice(comma+1)})};
  reader.onerror=function(){storeSearch.placeholder='Search'};
  reader.readAsDataURL(file);
}
(function(){
  var openBtn=document.getElementById('storeOpen');if(openBtn)openBtn.addEventListener('click',function(){vscode.postMessage({type:'storeBrowse'})});
  function active(){return activeTab==='store'}
  function draggy(e){var t=(e.dataTransfer&&e.dataTransfer.types)||[];for(var i=0;i<t.length;i++){if(t[i]==='Files'||t[i]==='text/uri-list'||t[i]==='text/plain')return true}return false}
  document.addEventListener('dragenter',function(e){if(!active()||!draggy(e))return;e.preventDefault();storePane.classList.add('rbdrag')},true);
  document.addEventListener('dragover',function(e){if(!active()||!draggy(e))return;e.preventDefault();if(e.dataTransfer)e.dataTransfer.dropEffect='copy';storePane.classList.add('rbdrag')},true);
  document.addEventListener('dragleave',function(e){if(!active())return;if(e.relatedTarget)return;storePane.classList.remove('rbdrag')},true);
  document.addEventListener('drop',function(e){
    if(!active())return;
    e.preventDefault();storePane.classList.remove('rbdrag');
    var dt=e.dataTransfer;if(!dt)return;
    var file=dt.files&&dt.files[0];
    if(file){storeSendBytes(file);return}
    var uri='';try{uri=dt.getData('text/uri-list')||dt.getData('text/plain')||''}catch(_){uri=''}
    if(uri){storeSearch.placeholder='decoding...';vscode.postMessage({type:'storeDecodePath',path:uri})}
  },true);
})();
gitApp.addEventListener('click',function(e){
  var refresh=e.target.closest('[data-gh-refresh]');
  if(refresh){
    vscode.postMessage({type:'gitRefresh',projectRoot:gitProjectRoot,generation:gitGeneration});
    return;
  }
  var group=e.target.closest('[data-gh-group]');
  if(group){
    toggleGitGroup(String(group.dataset.ghGroup||''));
    return;
  }
  var output=e.target.closest('[data-gh-output]');
  if(output){
    closeGitActions();
    vscode.postMessage({type:'gitOpenOutput'});
    return;
  }
  var action=e.target.closest('[data-gh-action]');
  if(action){
    closeGitActions();
    vscode.postMessage({type:'gitAction',action:action.dataset.ghAction,projectRoot:gitProjectRoot,generation:gitGeneration});
    return;
  }
  var diff=e.target.closest('[data-gh-diff]');
  if(diff){
    vscode.postMessage({type:'gitDiff',path:diff.dataset.ghDiff,projectRoot:gitProjectRoot,generation:gitGeneration});
    return;
  }
});
gitApp.addEventListener('keydown',function(e){
  if(e.key!=='Enter'&&e.key!==' ')return;
  var diff=e.target.closest('[data-gh-diff]');
  if(diff){
    e.preventDefault();
    vscode.postMessage({type:'gitDiff',path:diff.dataset.ghDiff,projectRoot:gitProjectRoot,generation:gitGeneration});
  }
});
refreshHistory.addEventListener('click',function(){historyLoaded=false;loadHistory()});
historyList.addEventListener('click',function(e){
  var btn=e.target.closest('.historyAction');if(!btn||btn.disabled)return;
  e.stopPropagation();
  var id=btn.dataset.id,action=btn.dataset.action;
  if(action==='restoreHistory'){
    historyRestoring[id]=true;
    renderHistory();
    vscode.postMessage({type:action,historyId:id});
    return;
  }
  if(action==='restoreHistoryGroup'){
    var ids=[];
    try{ids=JSON.parse(btn.dataset.ids||'[]')}catch(_){}
    var groupId=btn.dataset.groupId;
    historyRestoring[groupId]=true;
    renderHistory();
    vscode.postMessage({type:action,historyGroupId:groupId,historyIds:ids});
    return;
  }
  vscode.postMessage({type:action,historyId:id});
});
historyList.addEventListener('click',function(e){
  if(e.target.closest('.historyAction'))return;
  var toggle=e.target.closest('[data-action="toggleHistoryGroup"]');
  if(toggle){
    var groupId=toggle.dataset.groupId;
    if(historyExpanded.has(groupId))historyExpanded.delete(groupId);else historyExpanded.add(groupId);
    save();renderHistory();
    return;
  }
  var child=e.target.closest('.historyChild');
  if(child&&child.dataset.openId&&!child.classList.contains('noDiff')){
    vscode.postMessage({type:'compareHistoryBackup',historyId:child.dataset.openId});
  }
});
tree.addEventListener('click',function(e){
  if(e.target.closest('.rename'))return;
  closeMenus();
  var r=e.target.closest('.row'); if(!r){finishPointerRenameCleanup();return}
  if(r.dataset.load){requestLoad(r.dataset.load,true);finishPointerRenameCleanup();return}
  var id=r.dataset.id,n=nodes[id]; if(!n){finishPointerRenameCleanup();return}
  var addBtn=e.target.closest('.addBtn');
  if(addBtn){
    e.preventDefault();
    e.stopPropagation();
    applySelection(id,true);
    showClassPickerForButton(addBtn,id);
    finishPointerRenameCleanup();
    return;
  }
  if(e.target.closest('.twisty')&&!e.target.closest('.twisty').classList.contains('leaf')){
    var anchor=captureScrollAnchor(id);
    loadingIds[id]=true;
    if(n.expanded){
      n.expanded=false;
      vscode.postMessage({type:'collapseNode',nodeId:id,mode:filter?'search':'normal',start:visibleStart(),count:visibleCount()});
    }else{
      n.expanded=true;
      vscode.postMessage({type:'expandNode',nodeId:id,mode:filter?'search':'normal',start:visibleStart(),count:visibleCount()});
    }
    suppressRenameFocusoutRender=false;
    render(anchor); return;
  }
  selectNode(id);
  finishPointerRenameCleanup();
});
tree.addEventListener('mousedown',function(e){
  if(!renameId)return;
  var r=e.target.closest('.row');
  if(!r||r.dataset.id===renameId)return;
  var input=tree.querySelector('.row[data-id="'+CSS.escape(renameId)+'"] .rename');
  if(input){
    suppressRenameFocusoutRender=true;
    finishRename(input,false);
  }
},true);
document.addEventListener('mousedown',function(e){
  if(!renameId)return;
  renamePointerStartedInside=!!(e.target&&e.target.closest&&e.target.closest('.rename'));
  if(renamePointerStartedInside)renameSuppressFocusoutUntil=Date.now()+1200;
},true);
document.addEventListener('mouseup',function(){
  if(!renamePointerStartedInside)return;
  renameSuppressFocusoutUntil=Date.now()+180;
  setTimeout(function(){renamePointerStartedInside=false},0);
},true);
tree.addEventListener('dblclick',function(e){
  rememberPointerEvent(e);
  if(e.target.closest('.rename'))return;
  var r=e.target.closest('.row');if(r){var n=nodes[r.dataset.id];if(n&&n.isScript)vscode.postMessage({type:'openScript',nodeId:r.dataset.id})}
});
tree.addEventListener('contextmenu',function(e){
  rememberPointerEvent(e);
  e.preventDefault(); var r=e.target.closest('.row'); if(!r)return;
  menuNode=r.dataset.id; menuX=e.clientX; menuY=e.clientY; tree.focus(); applySelection(menuNode,false); vscode.postMessage({type:'selectNode',nodeId:menuNode});
  var n=nodes[menuNode],html='';
  if(n&&n.isScript)html+='<div class="mi" data-c="openScript">Open Script</div>';
  if(n&&n.canRename!==false)html+='<div class="mi" data-c="renameInstance">Rename</div>';
  if(n&&n.kind!=='service')html+='<div class="mi" data-c="copyInstance">Copy</div>';
  if(hasClipboardInstance)html+='<div class="mi" data-c="pasteInstance">Paste Into</div>';
  if(n&&n.kind!=='service')html+='<div class="mi" data-c="duplicateInstance">Duplicate</div>';
  html+='<div class="mi" data-c="importModel">Import</div>';
  if(n&&n.kind!=='service')html+='<div class="mi" data-c="exportModel">Export</div>';
  var linkSt=nodeLinkState(n);
  if(n&&n.kind!=='service'&&linkSt!=='linked'&&linkSt!=='broken')html+='<div class="mi" data-c="createLink">Create Link</div>';
  if(n&&n.kind!=='service'&&(linkSt==='linked'||linkSt==='broken'))html+='<div class="mi" data-c="resaveLink">Save New Package Version</div>';
  if(n&&n.kind!=='service'&&linkSt==='linked')html+='<div class="mi" data-c="breakLink">Break Link</div>';
  if(n&&n.kind!=='service'&&linkSt==='broken')html+='<div class="mi" data-c="relinkLink">Relink Package</div>';
  if(canDesyncPackage(n))html+='<div class="mi" data-c="desyncPackageLink">Desync Roblox Package</div>';
  if(n&&n.kind!=='service'&&n.canDelete!==false)html+='<div class="mi" data-c="deleteInstance">Delete</div>';
  html+='<div class="mi" data-c="copyPath">Copy Roblox Path</div>';
  classPicker.classList.add('hidden');
  menu.innerHTML=html; menu.style.left=e.clientX+'px'; menu.style.top=e.clientY+'px'; menu.classList.remove('hidden');
});
menu.addEventListener('click',function(e){
  var i=e.target.closest('.mi');if(!i||!menuNode)return;
  var c=i.dataset.c;menu.classList.add('hidden');
  if(c==='renameInstance'){startRename(menuNode);return}
  vscode.postMessage({type:c,nodeId:menuNode});
});
classSearch.addEventListener('input',function(){classActive=0;renderClassList()});
classSearch.addEventListener('keydown',function(e){
  var items=classList.querySelectorAll('.classItem[data-class]');
  if(e.key==='ArrowDown'){e.preventDefault();classActive=Math.min(items.length-1,classActive+1);renderClassList()}
  else if(e.key==='ArrowUp'){e.preventDefault();classActive=Math.max(0,classActive-1);renderClassList()}
  else if(e.key==='Enter'){e.preventDefault();var item=items[classActive]||items[0];if(item)createClass(item.dataset.class)}
  else if(e.key==='Escape'){classPicker.classList.add('hidden');tree.focus()}
});
classList.addEventListener('click',function(e){var item=e.target.closest('.classItem[data-class]');if(item)createClass(item.dataset.class)});
tree.addEventListener('pointermove',function(e){
  rememberPointerEvent(e);
  var r=e.target.closest('.row');
  if(!r)return;
  rememberPointerRow(r);
  if(externalPackageDrag&&!draggedId)markDropTarget(r.dataset.id);
});
tree.addEventListener('keydown',function(e){
  var target=e.target;
  if(target&&(target.tagName==='INPUT'||target.tagName==='SELECT'||target.tagName==='TEXTAREA'||(target.classList&&target.classList.contains('rename'))))return;
  if(!selectedId)return;
  var selected=nodes[selectedId],key=String(e.key||'').toLowerCase();
  if((e.key==='F2'||e.key==='Enter')&&selected&&selected.canRename!==false){e.preventDefault();startRename(selectedId)}
  else if(e.key==='Delete'&&selected&&selected.kind!=='service'&&selected.canDelete!==false){e.preventDefault();vscode.postMessage({type:'deleteInstance',nodeId:selectedId})}
  else if((e.ctrlKey||e.metaKey)&&e.shiftKey&&key==='a'){e.preventDefault();showClassPickerForNode(selectedId)}
  else if((e.ctrlKey||e.metaKey)&&key==='c'&&selected&&selected.kind!=='service'){e.preventDefault();vscode.postMessage({type:'copyInstance',nodeId:selectedId})}
  else if((e.ctrlKey||e.metaKey)&&key==='v'&&hasClipboardInstance){e.preventDefault();vscode.postMessage({type:'pasteInstance',nodeId:selectedId})}
  else if((e.ctrlKey||e.metaKey)&&key==='d'&&selected&&selected.kind!=='service'){e.preventDefault();vscode.postMessage({type:'duplicateInstance',nodeId:selectedId})}
});
tree.addEventListener('dragstart',function(e){
  rememberPointerEvent(e);
  if(e.target.closest('.rename')){
    e.preventDefault();
    e.stopPropagation();
    draggedId=null;
    return;
  }
  var r=e.target.closest('.row');if(!r)return;
  var id=r.dataset.id,n=nodes[id];if(!canDrag(n)){e.preventDefault();return}
  draggedId=id;e.dataTransfer.effectAllowed='move';e.dataTransfer.setData('text/plain',id);r.classList.add('dragging');
  updateDragAutoScroll(e.clientY||0);
});
tree.addEventListener('dragover',function(e){
  rememberPointerEvent(e);
  updateDragAutoScroll(e.clientY||0);
  var r=e.target.closest('.row');
  if(r)rememberPointerRow(r);
  if(draggedId){
    if(!r)return;
    var id=r.dataset.id;if(!canDrop(draggedId,id))return;
    e.preventDefault();e.dataTransfer.dropEffect='move';
    markDropTarget(id);
    return;
  }
  if(!r){
    if(hasPackageDragData(e.dataTransfer)){
      e.preventDefault();
      if(e.dataTransfer)e.dataTransfer.dropEffect='copy';
      markPackageFallbackTarget('tree dragover');
      return;
    }
    clearDropTarget();
    return;
  }
  if(hasPackageDragData(e.dataTransfer)){
    e.preventDefault();e.dataTransfer.dropEffect='copy';
    markDropTarget(r.dataset.id);
    return;
  }
  if(!hasExternalFileData(e.dataTransfer))return;
  e.preventDefault();e.dataTransfer.dropEffect='copy';
  markDropTarget(r.dataset.id);
});
document.addEventListener('dragover',function(e){
  if(draggedId)return;
  if(!hasPackageDragData(e.dataTransfer))return;
  var r=dragEventRow(e);
  if(!r){
    debugPackageDrag('document dragover: package active but no row');
    markPackageFallbackTarget('document dragover');
    return;
  }
  e.preventDefault();
  e.stopPropagation();
  if(e.dataTransfer)e.dataTransfer.dropEffect='copy';
  markDropTarget(r.dataset.id);
},true);
tree.addEventListener('dragleave',function(e){
  if(!tree.contains(e.relatedTarget)){
    if(externalPackageDrag)markPackageFallbackTarget('tree dragleave');
    else clearDropTarget();
    stopDragAutoScroll();
  }
});
tree.addEventListener('drop',function(e){
  var r=e.target.closest('.row');
  if(r)rememberPointerRow(r);
  var targetId=(r&&r.dataset.id)||packageDropTargetFromState();
  var pkg=droppedPackage(e.dataTransfer);
  if(pkg&&targetId){
    e.preventDefault();expanded.add(targetId);save();
    vscode.postMessage({type:'insertPackage',nodeId:targetId,linkId:pkg.id,name:pkg.name});
    externalPackageDrag=null;
    stopDragAutoScroll();
    clearDropTarget();
    draggedId=null;render();
    return;
  }
  var modelPaths=droppedModelPaths(e.dataTransfer);
  if(modelPaths.length>0&&targetId){
    e.preventDefault();expanded.add(targetId);save();
    vscode.postMessage({type:'importModel',nodeId:targetId,modelPaths:modelPaths});
    stopDragAutoScroll();
    clearDropTarget();
    draggedId=null;render();
    return;
  }
  if(!r||!draggedId)return;
  targetId=r.dataset.id;if(!canDrop(draggedId,targetId))return;
  e.preventDefault();expanded.add(targetId);save();
  vscode.postMessage({type:'moveInstance',nodeId:draggedId,targetId:targetId});
  stopDragAutoScroll();
  draggedId=null;clearDropTarget();render();
});
document.addEventListener('drop',function(e){
  if(draggedId)return;
  var pkg=droppedPackage(e.dataTransfer);
  if(!pkg){debugPackageDrag('document drop: no package payload');return}
  var r=dragEventRow(e),targetId=(r&&r.dataset.id)||packageDropTargetFromState(true);
  if(!targetId){debugPackageDrag('document drop: package '+pkg.id+' but no target row/selection');return}
  e.preventDefault();
  e.stopPropagation();
  insertExternalPackage(targetId,'document drop');
},true);
document.addEventListener('mousemove',function(e){
  rememberPointerEvent(e);
  if(!externalPackageDrag||draggedId)return;
  var r=dragEventRow(e);
  if(r){
    rememberPointerRow(r);
    markDropTarget(r.dataset.id);
    if(externalPackageDrag&&externalPackageDrag.mode==='drag'&&e.buttons===0){
      insertExternalPackage(r.dataset.id,'post-release hover');
    }
  }
  else markPackageFallbackTarget('document mousemove');
},true);
document.addEventListener('click',function(e){
  rememberPointerEvent(e);
  if(!externalPackageDrag||draggedId)return;
  var r=dragEventRow(e),targetId=(r&&r.dataset.id)||packageDropTargetFromState();
  if(!targetId){debugPackageDrag('placement click: no target row/selection');return}
  e.preventDefault();
  e.stopPropagation();
  insertExternalPackage(targetId,'placement click');
},true);
tree.addEventListener('dragend',function(){stopDragAutoScroll();draggedId=null;clearDropTarget();render()});
tree.addEventListener('keydown',function(e){
  if(e.target&&e.target.classList&&e.target.classList.contains('rename')){
    if(e.key==='Enter'){
      e.preventDefault();
      e.stopPropagation();
      finishRename(e.target,true);
    }
    else if(e.key==='Escape'){
      e.preventDefault();
      e.stopPropagation();
      cancelRename(true);
    }
  }
  if(e.key==='Escape'&&externalPackageDrag){
    externalPackageDrag=null;
    clearDropTarget();
    render();
  }
},true);
tree.addEventListener('focusout',function(e){
  if(e.target&&e.target.classList&&e.target.classList.contains('rename')){
    if(renamePointerStartedInside||Date.now()<renameSuppressFocusoutUntil){
      keepRenameInputFocused();
      return;
    }
    if(suppressRenameFocusoutRender)return;
    if(!finishRename(e.target,true))cleanupStaleRenameInput();
  }
});
document.addEventListener('click',function(e){
  if(!e.target.closest('#menu')&&!e.target.closest('#classPicker')&&!e.target.closest('#suggestions')&&!e.target.closest('#bar')&&!e.target.closest('#gitPane'))closeMenus();
  if(suppressRenameFocusoutRender)finishPointerRenameCleanup();
});
setActiveTab(activeTab,true);
if(rootIds.length&&activeTab==='explorer')render();
vscode.postMessage({type:'ready'});
if(activeTab==='history')setTimeout(loadHistory,0);
else if(activeTab==='git')setTimeout(function(){vscode.postMessage({type:'gitReady'})},0);
})();
</script>
</body>
</html>`;
}
