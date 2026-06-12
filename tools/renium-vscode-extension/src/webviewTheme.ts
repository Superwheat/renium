// VS Code injects live --vscode-* theme variables and theme classes into webviews.
// The only hardcoded override we apply is foreground text color so light themes render
// black text and dark/high-contrast themes render white text, similar to the terminal.
export function getThemeCssVars(): string {
	return "";
}

export function getThemeStyleAttribute(): string {
	return "";
}

export function getThemeCssBlock(): string {
	return `
:root {
	--verde-webview-foreground: #000;
}

body.vscode-light {
	--verde-webview-foreground: #000;
}

body.vscode-dark {
	--verde-webview-foreground: #fff;
}

body.vscode-high-contrast {
	--verde-webview-foreground: #fff;
}

body.vscode-high-contrast[data-vscode-theme-name*="light" i],
body.vscode-high-contrast[data-vscode-theme-id*="light" i] {
	--verde-webview-foreground: #000;
}

body {
	color: var(--verde-webview-foreground) !important;
	--vscode-foreground: var(--verde-webview-foreground);
	--vscode-sideBar-foreground: var(--verde-webview-foreground);
	--vscode-input-foreground: var(--verde-webview-foreground);
	--vscode-list-inactiveSelectionForeground: var(--verde-webview-foreground);
	--vscode-list-activeSelectionForeground: var(--verde-webview-foreground);
	--vscode-menu-foreground: var(--verde-webview-foreground);
	--vscode-menu-selectionForeground: var(--verde-webview-foreground);
	--vscode-dropdown-foreground: var(--verde-webview-foreground);
	--vscode-descriptionForeground: var(--verde-webview-foreground);
	--vscode-textLink-foreground: var(--verde-webview-foreground);
	--vscode-button-foreground: var(--verde-webview-foreground);
	--vscode-button-secondaryForeground: var(--verde-webview-foreground);
	--vscode-badge-foreground: var(--verde-webview-foreground);
}

body * {
	color: var(--verde-webview-foreground);
}
`;
}

export function getThemeScriptBlock(): string {
	return "";
}
