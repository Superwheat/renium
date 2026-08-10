import * as vscode from "vscode";
import * as fs from "fs";
import * as path from "path";
const THEME_CSS = `
:root,
body.vscode-light {
	--renium-webview-foreground: #000;
}

body.vscode-dark,
body.vscode-high-contrast {
	--renium-webview-foreground: #fff;
}

body.vscode-high-contrast[data-vscode-theme-name*="light" i],
body.vscode-high-contrast[data-vscode-theme-id*="light" i] {
	--renium-webview-foreground: #000;
}

body {
	color: var(--renium-webview-foreground) !important;
	--vscode-foreground: var(--renium-webview-foreground);
	--vscode-sideBar-foreground: var(--renium-webview-foreground);
	--vscode-input-foreground: var(--renium-webview-foreground);
	--vscode-list-inactiveSelectionForeground: var(--renium-webview-foreground);
	--vscode-list-activeSelectionForeground: var(--renium-webview-foreground);
	--vscode-menu-foreground: var(--renium-webview-foreground);
	--vscode-menu-selectionForeground: var(--renium-webview-foreground);
	--vscode-dropdown-foreground: var(--renium-webview-foreground);
	--vscode-descriptionForeground: var(--renium-webview-foreground);
	--vscode-textLink-foreground: var(--renium-webview-foreground);
	--vscode-button-foreground: var(--renium-webview-foreground);
	--vscode-button-secondaryForeground: var(--renium-webview-foreground);
	--vscode-badge-foreground: var(--renium-webview-foreground);
}

body * {
	color: var(--renium-webview-foreground);
}
`;

interface PropertiesHtmlOptions {
    showToggleButton?: boolean;
    showFilterInput?: boolean;
}

export function getPropertiesHtml(extensionUri: vscode.Uri, options: PropertiesHtmlOptions = {}): string {
    const htmlPath = path.join(extensionUri.fsPath, "resources", "properties.html");
    const cssPath = path.join(extensionUri.fsPath, "resources", "properties.css");
    const sortersPath = path.join(extensionUri.fsPath, "resources", "robloxPropertySorters.js");

    let cssContent = '';
    try {
        cssContent = fs.readFileSync(cssPath, 'utf8');
    } catch (cssError) {
        console.error("Failed to read properties.css:", cssError);
        cssContent = `body { background: red; color: white; }`;
    }

    let sortersContent = '';
    try {
        sortersContent = fs.readFileSync(sortersPath, 'utf8');
    } catch (sortersError) {
        console.error("Failed to read robloxPropertySorters.js:", sortersError);
        sortersContent = '';
    }

    try {
        let htmlContent = fs.readFileSync(htmlPath, 'utf8');

        const styleTag = `<style>${THEME_CSS}${cssContent}</style>`;
        const sortersScriptTag = `<script>\n${sortersContent}\n</script>`;
        htmlContent = htmlContent.replace('[[themeStyle]]', '');
        htmlContent = htmlContent.replace('<link href="[[styleUri]]" rel="stylesheet">', styleTag);
        htmlContent = htmlContent.replace('<script>', `${sortersScriptTag}\n<script>`);
        htmlContent = htmlContent.replace('[[topbarHtml]]', getTopbarHtml(options));
        htmlContent = htmlContent.replace('[[scriptElements]]', getScriptElements(options));
        htmlContent = htmlContent.replace('[[filterLogic]]', getFilterLogic(options));

        return htmlContent;
    } catch (error) {
        console.error("Failed to read properties.html:", error);
        return `<!DOCTYPE html>
<html>
<head>
	<meta charset="UTF-8">
	<meta name="viewport" content="width=device-width, initial-scale=1.0">
	<title>Properties</title>
	<style>${THEME_CSS}${cssContent}</style>
</head>
<body>
	<div class="root">
		${getTopbarHtml(options)}
		<div id="scroller" class="scroller">
			<div id="properties-container">Failed to load properties interface</div>
		</div>
	</div>
</body>
</html>`;
    }
}

function getTopbarHtml(options: PropertiesHtmlOptions): string {
    const { showToggleButton = false, showFilterInput = false } = options;

    if (!showToggleButton && !showFilterInput) {
        return '';
    }

    let topbarContent = '';

    if (showToggleButton) {
        topbarContent += '<button id="toggle-mode" class="toggle-button" title="Toggle Panel Mode">&harr;</button>';
    }

    if (!showFilterInput) {
        topbarContent += '<span id="properties-title" class="properties-title">Properties</span>';
    }

    if (showFilterInput) {
        topbarContent += '<input id="filter" class="filter" type="text" placeholder="Filter Properties" spellcheck="false" />';
    }

    return `<div class="topbar">${topbarContent}</div>`;
}

function getScriptElements(options: PropertiesHtmlOptions): string {
    const { showToggleButton = false, showFilterInput = false } = options;

    let scriptElements = '';

    if (showToggleButton) {
        scriptElements += `
		const toggleButton = document.getElementById("toggle-mode");
		toggleButton.addEventListener("click", () => {
			vscode.postMessage({
				type: "togglePanelMode"
			});
		});`;
    }

    if (showFilterInput) {
        scriptElements += `
		const filterInput = document.getElementById("filter");
		filterInput.addEventListener("input", () => {
			filterText = (filterInput.value || "").trim().toLowerCase();
			render();
		});

		filterInput.addEventListener("keydown", (e) => {
			if (e.key === "Escape") {
				filterInput.value = "";
				filterText = "";
				render();
			}
		});`;
    }

    return scriptElements;
}

function getFilterLogic(options: PropertiesHtmlOptions): string {
    const { showFilterInput = false } = options;

    if (showFilterInput) {
        return `
		let filterText = "";
		if (!filterText) return true;
		const name = (prop.name || "").toString().toLowerCase();
		const category = (prop.category || "Other").toString().toLowerCase();
		const type = (prop.type || "").toString().toLowerCase();
		return name.includes(filterText) || category.includes(filterText) || type.includes(filterText);`;
    } else {
        return `return true;`;
    }
}
