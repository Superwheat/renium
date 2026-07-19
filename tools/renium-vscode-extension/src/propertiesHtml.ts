import * as vscode from "vscode";
import * as fs from "fs";
import * as path from "path";
import { getThemeCssBlock, getThemeScriptBlock, getThemeStyleAttribute } from "./webviewTheme";

export interface PropertiesHtmlOptions {
    showToggleButton?: boolean;
    showFilterInput?: boolean;
}

export function getPropertiesHtml(extensionUri: vscode.Uri, options: PropertiesHtmlOptions = {}): string {
    const { showToggleButton = false, showFilterInput = false } = options;

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

        const styleTag = `<style>${getThemeCssBlock()}${cssContent}</style>`;
        const themeScriptTag = getThemeScriptBlock();
        const sortersScriptTag = `<script>\n${sortersContent}\n</script>`;
        htmlContent = htmlContent.replace('[[themeStyle]]', getThemeStyleAttribute());
        htmlContent = htmlContent.replace('<link href="[[styleUri]]" rel="stylesheet">', styleTag);
        htmlContent = htmlContent.replace('<script>', `${sortersScriptTag}\n${themeScriptTag}\n<script>`);
        htmlContent = htmlContent.replace('[[topbarHtml]]', getTopbarHtml(options));
        htmlContent = htmlContent.replace('[[scriptElements]]', getScriptElements(options));
        htmlContent = htmlContent.replace('[[filterLogic]]', getFilterLogic(options));

        return htmlContent;
    } catch (error) {
        console.error("Failed to read properties.html:", error);
        const themeScriptTag = getThemeScriptBlock();
        return `<!DOCTYPE html>
<html style="${getThemeStyleAttribute()}">
<head>
	<meta charset="UTF-8">
	<meta name="viewport" content="width=device-width, initial-scale=1.0">
	<title>Properties</title>
	<style>${getThemeCssBlock()}${cssContent}</style>
	${themeScriptTag}
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
