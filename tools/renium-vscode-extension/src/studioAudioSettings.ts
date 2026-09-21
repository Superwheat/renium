import * as fs from "fs";
import * as vscode from "vscode";
import { userConfigPath, userStudioAudioMode } from "./sharedConfig";

export function watchStudioAudioSettings(
  apply: (mode: string) => Promise<void>,
  report: (error: unknown) => void,
): vscode.Disposable {
  let disposed = false;
  let queue = Promise.resolve();
  const synchronize = (source: "editor" | "file" | "startup"): void => {
    queue = queue.then(async () => {
      if (disposed) { return; }
      const config = vscode.workspace.getConfiguration("renium");
      const editor = config.inspect<string>("studioAudioMode")?.globalValue;
      const shared = userStudioAudioMode();
      if (source === "editor" || (source === "startup" && shared === undefined && editor !== undefined)) {
        const mode = editor ?? "off";
        if (mode !== (shared ?? "off")) { await apply(mode); }
      } else {
        const mode = shared ?? "off";
        if (mode !== (editor ?? "off")) {
          await config.update("studioAudioMode", mode, vscode.ConfigurationTarget.Global);
        }
        if (source === "startup" && mode !== "off") { await apply(mode); }
      }
    }).catch(report);
  };
  const configuration = vscode.workspace.onDidChangeConfiguration(event => {
    if (event.affectsConfiguration("renium.studioAudioMode")) { synchronize("editor"); }
  });
  const file = userConfigPath();
  const changed = (): void => synchronize("file");
  fs.watchFile(file, { interval: 1000, persistent: false }, changed);
  synchronize("startup");
  return new vscode.Disposable(() => {
    disposed = true;
    configuration.dispose();
    fs.unwatchFile(file, changed);
  });
}
