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
    if (folders.length > 1) {
        const match = folders.find((folder) => fs.existsSync(path.join(folder.uri.fsPath, "renium.exe")));
        if (match) {
            return match;
        }
    }
    return folders[0];
}

export function pickWorkspaceRoot(): string | undefined {
    return pickWorkspaceRootFolder()?.uri.fsPath;
}
