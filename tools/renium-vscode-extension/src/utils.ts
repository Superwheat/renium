import * as vscode from "vscode";
import * as fs from "fs";
import * as path from "path";

export function isScriptClass(className: string): boolean {
    return className === "Script" || className === "LocalScript" || className === "ModuleScript";
}

export function pickWorkspaceRootFolder(): vscode.WorkspaceFolder | undefined {
    const folders = vscode.workspace.workspaceFolders;
    if (!folders || folders.length === 0) {
        return undefined;
    }
    const activeUri = vscode.window.activeTextEditor?.document.uri;
    if (activeUri?.scheme === "file") {
        const activeFolder = vscode.workspace.getWorkspaceFolder(activeUri);
        if (activeFolder) {
            return activeFolder;
        }
    }
    if (folders.length > 1) {
        const match = folders.find((folder) => {
            const root = folder.uri.fsPath;
            return fs.existsSync(path.join(root, "renium.experience.json"))
                || fs.existsSync(path.join(root, "renium.project.json"))
                || fs.existsSync(path.join(root, "renium.project.jsonc"))
                || fs.existsSync(path.join(root, "src"))
                || fs.existsSync(path.join(root, "sourcemap.json"))
                || fs.existsSync(path.join(root, "renium-link.json"))
                || fs.existsSync(path.join(root, ".renium"));
        });
        if (match) {
            return match;
        }
    }
    return folders[0];
}

export function pickWorkspaceRoot(): string | undefined {
    return pickWorkspaceRootFolder()?.uri.fsPath;
}
