import * as path from "path";
import * as vscode from "vscode";

import type { CommandRunResult } from "./automationClient";
import { AUTOMATION_OP } from "./automationProtocol.generated";

export type CollabSelection = {
  startLine: number;
  startCharacter: number;
  endLine: number;
  endCharacter: number;
};

export type CollabParticipant = {
  clientId: number;
  user: string;
  color: string;
  self: boolean;
  file?: string;
  selection?: CollabSelection;
};

export type CollabStatus = {
  running: boolean;
  role?: "host" | "guest";
  name?: string;
  invite?: string;
  connected?: boolean;
  ready?: boolean;
  tunnel?: boolean;
  peers?: number;
  participants?: CollabParticipant[];
  files?: number;
  localChanges?: number;
  remoteChanges?: number;
  error?: string;
  defaultRelay?: string;
};

export interface CollaborationDeps {
  output: vscode.OutputChannel;
  projectRoot: () => string | undefined;
  cliPath: () => string | undefined;
  liveSyncRunning: () => boolean;
  startLiveSync: () => Promise<void>;
  runOperation: (
    op: number,
    parameters: Record<string, unknown>,
    options?: { quietWait?: boolean; timeoutMs?: number },
  ) => Promise<CommandRunResult>;
}

const POLL_ACTIVE_MS = 750;
const POLL_IDLE_MS = 5_000;
const SELECTION_DEBOUNCE_MS = 120;
const ACTIVE_CONTEXT = "renium.collab.active";

type TreeNode =
  | { kind: "session" }
  | { kind: "invite" }
  | { kind: "participant"; participant: CollabParticipant }
  | { kind: "empty" };

export function relativeProjectPath(root: string, file: string): string | undefined {
  const relative = path.relative(root, file);
  if (!relative || relative.startsWith("..") || path.isAbsolute(relative)) {
    return undefined;
  }
  return relative.split(path.sep).join("/");
}

export function summarizeStatus(status: CollabStatus): string {
  if (!status.running) {
    return "Not collaborating";
  }
  const others = (status.participants ?? []).filter((participant) => !participant.self).length;
  const link = status.connected ? (status.ready ? "synced" : "syncing") : "reconnecting";
  const who = others === 1 ? "1 other" : `${others} others`;
  return `${status.role === "host" ? "Hosting" : "Joined"}, ${who}, ${link}`;
}

export class CollaborationController implements vscode.Disposable, vscode.TreeDataProvider<TreeNode> {
  private status: CollabStatus = { running: false };
  private readonly statusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 199);
  private readonly treeEmitter = new vscode.EventEmitter<TreeNode | undefined>();
  private readonly disposables: vscode.Disposable[] = [];
  private readonly decorationTypes = new Map<string, vscode.TextEditorDecorationType>();
  private pollTimer: NodeJS.Timeout | undefined;
  private selectionTimer: NodeJS.Timeout | undefined;
  private polling = false;
  private disposed = false;
  private lastAwareness = "";
  private lastInvite: string | undefined;
  private sessionRoot: string | undefined;

  public readonly onDidChangeTreeData = this.treeEmitter.event;

  public constructor(private readonly deps: CollaborationDeps) {
    this.statusItem.command = "renium.collab.menu";
    this.statusItem.name = "Renium Collaboration";
    this.disposables.push(
      vscode.window.onDidChangeTextEditorSelection((event) => this.onSelectionChanged(event.textEditor)),
      vscode.window.onDidChangeActiveTextEditor((editor) => {
        if (editor) {
          this.onSelectionChanged(editor);
        }
      }),
      vscode.window.onDidChangeVisibleTextEditors(() => this.renderDecorations()),
    );
    void vscode.commands.executeCommand("setContext", ACTIVE_CONTEXT, false);
    this.schedulePoll(1_500);
  }

  public dispose(): void {
    this.disposed = true;
    if (this.pollTimer) {
      clearTimeout(this.pollTimer);
    }
    if (this.selectionTimer) {
      clearTimeout(this.selectionTimer);
    }
    for (const type of this.decorationTypes.values()) {
      type.dispose();
    }
    this.decorationTypes.clear();
    for (const disposable of this.disposables) {
      disposable.dispose();
    }
    this.statusItem.dispose();
    this.treeEmitter.dispose();
  }

  public currentStatus(): CollabStatus {
    return this.status;
  }

  public isActive(): boolean {
    return this.status.running;
  }

  private root(): string | undefined {
    return this.sessionRoot ?? this.deps.projectRoot();
  }

  private async pickSessionFolder(title: string): Promise<string | undefined> {
    const picked = await vscode.window.showOpenDialog({
      title,
      openLabel: "Use Folder",
      canSelectFiles: false,
      canSelectFolders: true,
      canSelectMany: false,
    });
    return picked?.[0]?.fsPath;
  }

  private async openRelaySetup(): Promise<void> {
    const cli = this.deps.cliPath();
    if (!cli) {
      void vscode.window.showErrorMessage("Renium CLI not found; install Renium first.");
      return;
    }
    const terminal = vscode.window.createTerminal({ name: "Renium relay setup" });
    terminal.show(true);
    const quoted = cli.includes(" ") ? `"${cli}"` : cli;
    terminal.sendText(`${quoted} collab relay deploy`, true);
    void vscode.window.showInformationMessage(
      "Deploying the relay. If Cloudflare asks you to sign in, finish that in the browser; the relay becomes the default when the deploy ends.",
    );
  }

  private displayName(): string | undefined {
    const configured = vscode.workspace.getConfiguration("renium").get<string>("collaboration.displayName", "").trim();
    return configured.length > 0 ? configured : undefined;
  }

  private async run(op: number, parameters: Record<string, unknown>, quiet: boolean, timeoutMs?: number): Promise<CommandRunResult> {
    const root = this.root();
    if (!root) {
      throw new Error("Open a project folder before collaborating.");
    }
    return this.deps.runOperation(op, { root, ...parameters }, { quietWait: quiet, timeoutMs });
  }

  public async start(): Promise<void> {
    if (this.status.running) {
      void vscode.window.showInformationMessage("A collaboration session is already running for this project.");
      return;
    }
    if (!this.deps.projectRoot()) {
      void vscode.window.showInformationMessage("Open the project folder you want to share first.");
      return;
    }
    const settingRelay = vscode.workspace.getConfiguration("renium").get<string>("collaboration.relayUrl", "").trim();
    const relayDefault = settingRelay || this.status.defaultRelay || "";
    type ModeItem = vscode.QuickPickItem & { mode: "tunnel" | "relay" | "local" | "setup" };
    const relayItem: ModeItem = {
      label: "$(server) Relay",
      description: relayDefault
        ? `${relayDefault} keeps the room and its history`
        : "Deploy a free relay to your Cloudflare account",
      mode: relayDefault ? "relay" : "setup",
    };
    const tunnelItem: ModeItem = {
      label: "$(globe) Direct tunnel",
      description: "No setup. The room lives on this machine while you share.",
      mode: "tunnel",
    };
    const localItem: ModeItem = {
      label: "$(home) This machine only",
      description: "Share only with editors on this computer.",
      mode: "local",
    };
    const mode = await vscode.window.showQuickPick(
      relayDefault ? [relayItem, tunnelItem, localItem] : [tunnelItem, relayItem, localItem],
      { title: "Start Collaboration", placeHolder: "How should others reach this project?" },
    );
    if (!mode) {
      return;
    }
    if (mode.mode === "setup") {
      await this.openRelaySetup();
      return;
    }
    const parameters: Record<string, unknown> = { name: this.displayName(), tunnel: mode.mode === "tunnel" };
    if (mode.mode === "relay") {
      parameters.relay = relayDefault;
    }
    this.sessionRoot = undefined;
    await vscode.window.withProgress(
      { location: vscode.ProgressLocation.Notification, title: "Renium: starting collaboration" },
      async () => {
        const result = await this.run(AUTOMATION_OP.collabStart, parameters, false, 180_000);
        this.applyResult(result, "start");
      },
    );
    await this.poll();
    if (this.status.running && this.status.invite) {
      await vscode.env.clipboard.writeText(this.status.invite);
      this.lastInvite = this.status.invite;
      if (this.deps.liveSyncRunning()) {
        void vscode.window.showInformationMessage("Collaboration started. The invite link is on your clipboard.");
      } else {
        const choice = await vscode.window.showInformationMessage(
          "Collaboration started and the invite link is on your clipboard. Start Live Sync so guests' edits reach Studio?",
          "Start Live Sync",
        );
        if (choice === "Start Live Sync") {
          await this.deps.startLiveSync();
        }
      }
    }
  }

  public async join(): Promise<void> {
    if (this.status.running) {
      void vscode.window.showInformationMessage("Leave the current collaboration session before joining another.");
      return;
    }
    const clipboard = (await vscode.env.clipboard.readText()).trim();
    const invite = await vscode.window.showInputBox({
      title: "Join Collaboration",
      prompt: "Paste the invite link from the host",
      value: clipboard.includes("token=") ? clipboard : "",
      placeHolder: "wss://…trycloudflare.com/?token=…",
      ignoreFocusOut: true,
      validateInput: (value) => (value.trim().includes("token=") ? undefined : "Invite links carry a token"),
    });
    if (!invite) {
      return;
    }
    let folder = this.deps.projectRoot();
    const workspaceFolder = folder;
    if (!folder) {
      folder = await this.pickSessionFolder("Choose an empty folder for the shared project");
      if (!folder) {
        return;
      }
    }
    const entries = await vscode.workspace.fs.readDirectory(vscode.Uri.file(folder)).then(
      (found) => found.filter(([name]) => !name.startsWith(".")),
      () => [],
    );
    if (entries.length > 0) {
      const choice = await vscode.window.showWarningMessage(
        `${path.basename(folder)} already has files. Joining replaces its project files with the room's copy.`,
        { modal: true },
        "Join and Replace",
      );
      if (choice !== "Join and Replace") {
        return;
      }
    }
    this.sessionRoot = folder;
    await vscode.window.withProgress(
      { location: vscode.ProgressLocation.Notification, title: "Renium: joining collaboration" },
      async () => {
        const result = await this.run(
          AUTOMATION_OP.collabJoin,
          { invite: invite.trim(), name: this.displayName() },
          false,
          120_000,
        );
        this.applyResult(result, "join");
      },
    );
    await this.poll();
    if (this.status.running) {
      if (workspaceFolder !== folder) {
        this.sessionRoot = undefined;
        await vscode.commands.executeCommand("vscode.openFolder", vscode.Uri.file(folder), { forceNewWindow: false });
        return;
      }
      void vscode.window.showInformationMessage("Joined. Project files fill in as the room syncs.");
    }
  }

  public async stop(): Promise<void> {
    if (!this.status.running) {
      return;
    }
    const result = await this.run(AUTOMATION_OP.collabStop, {}, false, 30_000);
    this.applyResult(result, "stop");
    this.status = { running: false };
    this.render();
  }

  public async copyInvite(): Promise<void> {
    if (!this.status.running || !this.status.invite) {
      void vscode.window.showInformationMessage("No collaboration session is running.");
      return;
    }
    await vscode.env.clipboard.writeText(this.status.invite);
    void vscode.window.showInformationMessage("Invite link copied.");
  }

  public async menu(): Promise<void> {
    const status = this.status;
    type Item = vscode.QuickPickItem & { action?: string };
    const items: Item[] = [];
    if (status.running) {
      items.push({ label: summarizeStatus(status), kind: vscode.QuickPickItemKind.Separator });
      if (status.invite) {
        items.push({ label: "$(link) Copy invite link", description: status.tunnel ? "Cloudflare tunnel" : undefined, action: "copy" });
      }
      for (const participant of status.participants ?? []) {
        items.push({
          label: `$(account) ${participant.user}${participant.self ? " (you)" : ""}`,
          description: participant.file ?? "",
          action: participant.file && !participant.self ? `open:${participant.clientId}` : undefined,
        });
      }
      if (status.error) {
        items.push({ label: `$(warning) ${status.error}` });
      }
      items.push({ label: "$(debug-disconnect) Leave session", action: "stop" });
    } else {
      items.push(
        { label: "$(broadcast) Start Collaboration", description: "Share this project", action: "start" },
        { label: "$(plug) Join Collaboration", description: "Use an invite link", action: "join" },
      );
    }
    const picked = await vscode.window.showQuickPick(items, { title: "Renium Collaboration" });
    const action = picked?.action;
    if (!action) {
      return;
    }
    if (action === "copy") {
      await this.copyInvite();
    } else if (action === "stop") {
      await this.stop();
    } else if (action === "start") {
      await this.start();
    } else if (action === "join") {
      await this.join();
    } else if (action.startsWith("open:")) {
      const id = Number(action.slice(5));
      const participant = (status.participants ?? []).find((entry) => entry.clientId === id);
      if (participant) {
        await this.revealParticipant(participant);
      }
    }
  }

  private applyResult(result: CommandRunResult, action: string): void {
    if (result.code !== 0) {
      const message = result.automationError?.m ?? result.output.trim() ?? "Renium reported an error";
      this.deps.output.appendLine(`[collab] ${action} failed: ${message}`);
      throw new Error(message);
    }
    const status = result.result as CollabStatus | undefined;
    if (status && typeof status.running === "boolean") {
      this.status = status;
      this.render();
    }
  }

  private schedulePoll(delayMs: number): void {
    if (this.disposed) {
      return;
    }
    if (this.pollTimer) {
      clearTimeout(this.pollTimer);
    }
    this.pollTimer = setTimeout(() => {
      void this.poll();
    }, delayMs);
  }

  public async poll(): Promise<void> {
    if (this.disposed || this.polling) {
      return;
    }
    this.polling = true;
    let delay = POLL_IDLE_MS;
    try {
      const root = this.root();
      if (root) {
        const result = await this.deps.runOperation(
          AUTOMATION_OP.collabStatus,
          { root },
          { quietWait: true, timeoutMs: 10_000 },
        );
        const status = result.code === 0 ? (result.result as CollabStatus | undefined) : undefined;
        if (status && typeof status.running === "boolean") {
          this.status = status;
          if (
            status.running &&
            status.role === "host" &&
            status.invite &&
            this.lastInvite &&
            status.invite !== this.lastInvite
          ) {
            await vscode.env.clipboard.writeText(status.invite);
            void vscode.window.showInformationMessage(
              "The collaboration invite changed after a restart. The new link is on your clipboard.",
            );
          }
          if (status.running && status.invite) {
            this.lastInvite = status.invite;
          }
        } else if (result.code !== 0) {
          this.status = { running: false };
        }
        this.render();
        delay = this.status.running ? POLL_ACTIVE_MS : POLL_IDLE_MS;
      }
    } catch (error) {
      this.deps.output.appendLine(`[collab] status poll failed: ${error instanceof Error ? error.message : String(error)}`);
      delay = POLL_IDLE_MS;
    } finally {
      this.polling = false;
      this.schedulePoll(delay);
    }
  }

  private render(): void {
    const status = this.status;
    void vscode.commands.executeCommand("setContext", ACTIVE_CONTEXT, status.running);
    if (!status.running) {
      this.statusItem.hide();
    } else {
      const others = (status.participants ?? []).filter((participant) => !participant.self).length;
      const icon = status.connected ? "$(broadcast)" : "$(sync~spin)";
      this.statusItem.text = `${icon} ${others}`;
      this.statusItem.tooltip = new vscode.MarkdownString(
        [
          `**Renium Collaboration**`,
          summarizeStatus(status),
          status.invite ? `Invite: ${status.invite}` : "",
          status.error ? `Error: ${status.error}` : "",
        ]
          .filter((line) => line.length > 0)
          .join("\n\n"),
      );
      this.statusItem.backgroundColor = status.error
        ? new vscode.ThemeColor("statusBarItem.warningBackground")
        : undefined;
      this.statusItem.show();
    }
    this.treeEmitter.fire(undefined);
    this.renderDecorations();
  }

  public getTreeItem(node: TreeNode): vscode.TreeItem {
    switch (node.kind) {
      case "session": {
        const item = new vscode.TreeItem(summarizeStatus(this.status));
        item.iconPath = new vscode.ThemeIcon(this.status.connected ? "broadcast" : "sync~spin");
        item.description = this.status.files !== undefined ? `${this.status.files} files` : undefined;
        item.tooltip = this.status.error ?? undefined;
        item.contextValue = "collabSession";
        return item;
      }
      case "invite": {
        const item = new vscode.TreeItem("Copy invite link");
        item.iconPath = new vscode.ThemeIcon("link");
        item.description = this.status.tunnel ? "tunnel" : "relay";
        item.command = { command: "renium.collab.copyInvite", title: "Copy Collaboration Invite" };
        return item;
      }
      case "participant": {
        const participant = node.participant;
        const item = new vscode.TreeItem(participant.self ? `${participant.user} (you)` : participant.user);
        item.iconPath = new vscode.ThemeIcon("account");
        item.description = participant.file ?? "";
        if (participant.file && !participant.self) {
          item.command = {
            command: "renium.collab.reveal",
            title: "Go to participant",
            arguments: [participant.clientId],
          };
        }
        return item;
      }
      default: {
        const item = new vscode.TreeItem("Not collaborating");
        item.iconPath = new vscode.ThemeIcon("circle-slash");
        return item;
      }
    }
  }

  public getChildren(): TreeNode[] {
    if (!this.status.running) {
      return [];
    }
    const nodes: TreeNode[] = [{ kind: "session" }];
    if (this.status.invite) {
      nodes.push({ kind: "invite" });
    }
    for (const participant of this.status.participants ?? []) {
      nodes.push({ kind: "participant", participant });
    }
    return nodes;
  }

  public async revealParticipant(idOrParticipant: number | CollabParticipant): Promise<void> {
    const participant =
      typeof idOrParticipant === "number"
        ? (this.status.participants ?? []).find((entry) => entry.clientId === idOrParticipant)
        : idOrParticipant;
    const root = this.root();
    if (!participant?.file || !root) {
      return;
    }
    const uri = vscode.Uri.file(path.join(root, ...participant.file.split("/")));
    const editor = await vscode.window.showTextDocument(uri, { preserveFocus: false });
    if (participant.selection) {
      const range = toRange(participant.selection);
      editor.revealRange(range, vscode.TextEditorRevealType.InCenter);
    }
  }

  private onSelectionChanged(editor: vscode.TextEditor): void {
    if (!this.status.running || editor.document.uri.scheme !== "file") {
      return;
    }
    if (this.selectionTimer) {
      clearTimeout(this.selectionTimer);
    }
    this.selectionTimer = setTimeout(() => {
      void this.publishSelection(editor);
    }, SELECTION_DEBOUNCE_MS);
  }

  private async publishSelection(editor: vscode.TextEditor): Promise<void> {
    const root = this.root();
    if (!root || !this.status.running) {
      return;
    }
    const file = relativeProjectPath(root, editor.document.uri.fsPath);
    const selection = editor.selection;
    const state: Record<string, unknown> = file
      ? {
          file,
          selection: {
            startLine: selection.start.line,
            startCharacter: selection.start.character,
            endLine: selection.end.line,
            endCharacter: selection.end.character,
          },
        }
      : { file: null, selection: null };
    const encoded = JSON.stringify(state);
    if (encoded === this.lastAwareness) {
      return;
    }
    this.lastAwareness = encoded;
    try {
      await this.run(AUTOMATION_OP.collabAwareness, { state }, true, 5_000);
    } catch (error) {
      this.deps.output.appendLine(`[collab] cursor update failed: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private decorationFor(participant: CollabParticipant): vscode.TextEditorDecorationType {
    const key = `${participant.color}|${participant.user}`;
    let type = this.decorationTypes.get(key);
    if (!type) {
      type = vscode.window.createTextEditorDecorationType({
        backgroundColor: `${participant.color}33`,
        border: `1px solid ${participant.color}`,
        borderRadius: "2px",
        overviewRulerColor: participant.color,
        overviewRulerLane: vscode.OverviewRulerLane.Right,
        after: {
          contentText: ` ${participant.user} `,
          backgroundColor: participant.color,
          color: "#ffffff",
          margin: "0 0 0 6px",
          fontWeight: "600",
        },
      });
      this.decorationTypes.set(key, type);
    }
    return type;
  }

  private renderDecorations(): void {
    const root = this.root();
    const show = vscode.workspace.getConfiguration("renium").get<boolean>("collaboration.showCursors", true);
    const participants = this.status.running && root && show
      ? (this.status.participants ?? []).filter((participant) => !participant.self && participant.file && participant.selection)
      : [];
    const used = new Set<vscode.TextEditorDecorationType>();
    for (const editor of vscode.window.visibleTextEditors) {
      if (editor.document.uri.scheme !== "file" || !root) {
        continue;
      }
      const file = relativeProjectPath(root, editor.document.uri.fsPath);
      const byType = new Map<vscode.TextEditorDecorationType, vscode.Range[]>();
      for (const participant of participants) {
        if (participant.file !== file || !participant.selection) {
          continue;
        }
        const type = this.decorationFor(participant);
        const range = clampRange(editor.document, toRange(participant.selection));
        byType.set(type, [...(byType.get(type) ?? []), range]);
      }
      for (const [type, ranges] of byType) {
        editor.setDecorations(type, ranges);
        used.add(type);
      }
      for (const type of this.decorationTypes.values()) {
        if (!byType.has(type)) {
          editor.setDecorations(type, []);
        }
      }
    }
  }
}

function toRange(selection: CollabSelection): vscode.Range {
  return new vscode.Range(
    new vscode.Position(selection.startLine, selection.startCharacter),
    new vscode.Position(selection.endLine, selection.endCharacter),
  );
}

function clampRange(document: vscode.TextDocument, range: vscode.Range): vscode.Range {
  return document.validateRange(range);
}
