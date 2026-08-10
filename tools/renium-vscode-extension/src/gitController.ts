import * as fs from "fs";
import * as path from "path";
import { URLSearchParams } from "url";
import * as vscode from "vscode";

import {
  buildCommitMessage,
  nameStatusAffectedPaths,
  parseAheadBehind,
  parseNameStatusZ,
  parsePorcelainV1Z,
  redactRemoteUrl,
  remoteUrlToWebUrl,
  renderGitArgs,
  runGit,
  shouldPullFromStudioBeforePush,
  summarizeStatus,
  type GitNameStatusEntry,
  type GitRunResult,
  type GitStatusEntry,
} from "./gitSync";
import { emptyGitViewState, type GitViewActions, type GitViewState } from "./gitView";
import { projectProcessOwner } from "./processSupervisor";
import { loadProjectSourceGraph } from "./sharedConfig";
import { filesystemPathKey, isPathInside } from "./utils";

export type GitSyncConfig = {
  gitPath: string;
  remote: string;
  branch: string;
  autoFetch: boolean;
  pullFromStudioBeforePush: "ask" | "always" | "never";
  stageMode: "tracked" | "configuredPaths";
  stagePaths: string[];
  includeUntracked: boolean;
  commitMessageTemplate: string;
  confirmBeforePush: boolean;
  requireCleanWorktreeBeforePull: boolean;
  applyPulledChangesToStudio: "ask" | "always" | "never";
  timeoutSeconds: number;
  outputBehavior: "onStart" | "onError" | "silent";
};

type GitControllerConfig = {
  projectRoot: string;
  services: string[];
  gitSync: GitSyncConfig;
};

type GitRepoState = {
  view: GitViewState;
  entries: GitStatusEntry[];
  worktreeEntries: GitStatusEntry[];
  repoRoot?: string;
  branch?: string;
  upstream?: string;
  remote?: string;
  remoteUrl?: string;
  ahead: number;
  behind: number;
};

type GitProjectToken = {
  projectRoot: string;
  generation: number;
};

type GitControllerHost<TConfig extends GitControllerConfig> = {
  context: vscode.ExtensionContext;
  output: vscode.OutputChannel;
  getConfig: () => TConfig;
  enqueue: (taskName: string, task: () => Promise<void>) => Promise<void>;
  experienceChanging: () => boolean;
  experienceGeneration: () => number;
  servicesForProjectSourcePath: (filePath: string, config: TConfig) => string[];
  isProjectSourcePath: (filePath: string, config: TConfig) => boolean;
  pushEditorPathsNow: (
    paths: string[],
    options: { force: boolean; skipChangeFilter: boolean; taskName: string },
  ) => Promise<boolean>;
  isLiveSyncActiveOrStarting: () => boolean;
  stopLiveSync: () => Promise<void>;
  startLiveSync: () => Promise<void>;
  pullFromStudio: (config: TConfig) => Promise<void>;
};

export class GitController<TConfig extends GitControllerConfig> {
  private gitViewRefreshSuppression = 0;

  public constructor(private readonly host: GitControllerHost<TConfig>) {}

  public actions(): GitViewActions {
    return {
      refresh: (options) => this.getGitViewState(options),
      runAction: (action, context) => this.runGitViewAction(action, context.projectRoot),
      openOutput: () => this.host.output.show(true),
      openDiff: (filePath, context) => this.openGitDiff(filePath, context.projectRoot),
    };
  }

  private gitHeadProviderRegistered = false;

  private ensureGitHeadProvider(): void {
    if (this.gitHeadProviderRegistered) {
      return;
    }
    this.gitHeadProviderRegistered = true;
    const provider: vscode.TextDocumentContentProvider = {
      provideTextDocumentContent: async (uri) => {
        try {
          const repoRoot = new URLSearchParams(uri.query).get("root") ?? "";
          const relPath = uri.path.replace(/^\/+/, "");
          if (!repoRoot || !relPath) {
            return "";
          }
          return await this.gitOutput(this.host.getConfig(), repoRoot, ["show", `HEAD:${relPath}`], "read HEAD version");
        } catch {
          return "";
        }
      },
    };
    this.host.context.subscriptions.push(
      vscode.workspace.registerTextDocumentContentProvider("renium-githead", provider),
    );
  }

  private async openGitDiff(filePath: string, expectedProjectRoot: string): Promise<void> {
    const requested = String(filePath ?? "").trim();
    if (!requested) {
      return;
    }
    this.ensureGitHeadProvider();
    const token = this.captureGitProjectToken(expectedProjectRoot);
    const cfg = this.gitConfigForToken(token);
    let repoRoot: string;
    try {
      const state = await this.inspectGitRepo(cfg, { fetch: false });
      this.gitConfigForToken(token);
      repoRoot = this.requireGitRepoRoot(state);
    } catch (err) {
      vscode.window.showErrorMessage(`Cannot open diff. ${err instanceof Error ? err.message : String(err)}`);
      return;
    }
    const absFile = path.isAbsolute(requested) ? requested : path.join(repoRoot, requested);
    const relForGit = path.relative(repoRoot, absFile).split(path.sep).join("/");
    const title = `${path.basename(absFile)} (HEAD ↔ Working Tree)`;
    const headUri = vscode.Uri.from({
      scheme: "renium-githead",
      path: `/${relForGit}`,
      query: `root=${encodeURIComponent(repoRoot)}&t=${Date.now()}`,
    });
    if (!fs.existsSync(absFile)) {
      await vscode.window.showTextDocument(headUri, { preview: true });
      return;
    }
    await vscode.commands.executeCommand("vscode.diff", headUri, vscode.Uri.file(absFile), title);
  }

  public async openGitSync(): Promise<void> {
    await vscode.commands.executeCommand("workbench.view.extension.reniumContainer");
    await vscode.commands.executeCommand("renium.fileExplorer.showGit");
  }

  private captureGitProjectToken(expectedProjectRoot?: string): GitProjectToken {
    const cfg = this.host.getConfig();
    const projectRoot = filesystemPathKey(cfg.projectRoot);
    if (
      this.host.experienceChanging()
      || (
        expectedProjectRoot !== undefined
        && filesystemPathKey(expectedProjectRoot) !== projectRoot
      )
    ) {
      throw new Error("The active Renium place changed. Run the Git action again.");
    }
    return { projectRoot, generation: this.host.experienceGeneration() };
  }

  private gitConfigForToken(token: GitProjectToken): TConfig {
    const cfg = this.host.getConfig();
    if (
      this.host.experienceChanging()
      || this.host.experienceGeneration() !== token.generation
      || filesystemPathKey(cfg.projectRoot) !== token.projectRoot
    ) {
      throw new Error("The active Renium place changed. Run the Git action again.");
    }
    return cfg;
  }

  public async gitStatus(token = this.captureGitProjectToken()): Promise<void> {
    await this.host.enqueue("Git status", async () => {
      const cfg = this.gitConfigForToken(token);
      const state = await this.inspectGitRepo(cfg, { fetch: false });
      this.host.output.show(false);
      this.logGitState(state);
      await this.refreshView();
    });
  }

  public async gitFetch(token = this.captureGitProjectToken()): Promise<void> {
    await this.host.enqueue("Git fetch", async () => {
      const cfg = this.gitConfigForToken(token);
      this.ensureWorkspaceTrustedForGitSync();
      const state = await this.inspectGitRepo(cfg, { fetch: false, requireRemote: true });
      const repoRoot = this.requireGitRepoRoot(state);
      const remote = state.remote ?? cfg.gitSync.remote;
      const result = await this.runGitCommand(cfg, repoRoot, ["fetch", "--prune", remote], "fetch");
      this.ensureGitSuccess(result, "fetch");
      await this.refreshView();
      vscode.window.showInformationMessage(`Fetched ${remote}.`);
    });
  }

  public async gitPull(token = this.captureGitProjectToken()): Promise<void> {
    await this.host.enqueue("Git pull", async () => {
      const cfg = this.gitConfigForToken(token);
      this.ensureWorkspaceTrustedForGitSync();
      let state = await this.inspectGitRepo(cfg, { fetch: cfg.gitSync.autoFetch, requireRemote: true });
      const repoRoot = this.requireGitRepoRoot(state);
      this.ensureNoGitConflicts(state);
      if (cfg.gitSync.requireCleanWorktreeBeforePull && state.worktreeEntries.length > 0) {
        throw new Error("Pull is blocked because the worktree has local changes. Commit, stash, or discard them before pulling.");
      }

      const remote = state.remote ?? cfg.gitSync.remote;
      const branch = this.resolveGitBranch(cfg, state);
      if (state.upstream && state.behind === 0 && state.ahead === 0) {
        this.host.output.appendLine(`[git-sync] pull skipped: ${remote}/${branch} is already up to date.`);
        vscode.window.showInformationMessage("Git pull is already up to date.");
        return;
      }
      if (state.upstream && state.ahead > 0 && state.behind > 0) {
        throw new Error("Pull is blocked because the branch has diverged. Resolve with VS Code Source Control or git manually.");
      }

      const resumeLiveSync = await this.ensureLiveSyncStoppedForGitPull();
      try {
        this.gitConfigForToken(token);
        const oldHead = await this.gitOutput(cfg, repoRoot, ["rev-parse", "HEAD"], "read HEAD");
        const pullResult = await this.runGitCommand(cfg, repoRoot, ["pull", "--ff-only", remote, branch], "pull --ff-only");
        this.ensureGitSuccess(pullResult, "pull --ff-only");
        const newHead = await this.gitOutput(cfg, repoRoot, ["rev-parse", "HEAD"], "read HEAD after pull");
        const changedFiles = oldHead !== newHead
          ? await this.gitChangedFilesBetween(cfg, repoRoot, oldHead, newHead)
          : [];
        await this.refreshExplorerForGitPaths(repoRoot, changedFiles, cfg);
        await this.maybeApplyPulledPathsToStudio(repoRoot, changedFiles, cfg);
        state = await this.inspectGitRepo(cfg, { fetch: false });
        this.logGitState(state);
        await this.refreshView();
        vscode.window.showInformationMessage(`Pulled ${remote}/${branch}.`);
      } catch (error) {
        if (
          resumeLiveSync
          && this.host.experienceGeneration() === token.generation
          && filesystemPathKey(this.host.getConfig().projectRoot) === token.projectRoot
        ) {
          await this.host.startLiveSync();
        }
        throw error;
      }
    });
  }

  public async gitCommitAndPush(
    options: { pullFromStudioFirst?: boolean } = {},
    token = this.captureGitProjectToken(),
  ): Promise<void> {
    await this.host.enqueue(options.pullFromStudioFirst ? "Pull from Studio, commit and push" : "Git commit & push", async () => {
      const cfg = this.gitConfigForToken(token);
      this.ensureWorkspaceTrustedForGitSync();
      await this.maybePullFromStudioBeforeGitPush(cfg, options.pullFromStudioFirst === true);
      this.gitConfigForToken(token);

      let state = await this.inspectGitRepo(cfg, { fetch: cfg.gitSync.autoFetch, requireRemote: true });
      const repoRoot = this.requireGitRepoRoot(state);
      this.ensureNoGitConflicts(state);
      if (state.behind > 0) {
        throw new Error("Push is blocked because the remote has new commits. Pull first, then retry.");
      }
      const preexistingStaged = await this.gitStagedChanges(cfg, repoRoot);
      if (preexistingStaged.length > 0) {
        throw new Error(`Push is blocked because ${preexistingStaged.length} file(s) are already staged. Commit or unstage them first so Renium does not publish unintended changes.`);
      }

      const plannedChanges = await this.plannedGitStageChanges(cfg, repoRoot);
      state = await this.inspectGitRepo(cfg, { fetch: false, requireRemote: true });
      const remote = state.remote ?? cfg.gitSync.remote;
      const branch = this.resolveGitBranch(cfg, state);

      if (plannedChanges.length === 0) {
        if (state.ahead > 0) {
          await this.confirmGitPush(`No new files were staged, but ${state.ahead} local commit(s) are ahead of ${state.upstream ?? remote + "/" + branch}. Push them now?`, cfg);
          await this.pushGitBranch(cfg, repoRoot, remote, branch, state.upstream === undefined);
          await this.refreshView();
          return;
        }
        throw new Error("No tracked changes are available to commit. Untracked files are excluded unless renium.gitSync.includeUntracked is enabled or stage paths are configured.");
      }

      await this.confirmGitCommitAndPush(plannedChanges, state, cfg);
      const commitMessage = await this.gitCommitMessage(cfg, branch);
      await this.stageGitSyncChanges(cfg, repoRoot);
      const staged = await this.gitStagedChanges(cfg, repoRoot);
      if (staged.length === 0) {
        throw new Error("No files were staged after applying the configured Git sync path filters.");
      }
      const commitResult = await this.runGitCommand(cfg, repoRoot, ["commit", "-m", commitMessage], "commit");
      this.ensureGitSuccess(commitResult, "commit");
      const shortSha = await this.gitOutput(cfg, repoRoot, ["rev-parse", "--short", "HEAD"], "read commit sha");
      await this.pushGitBranch(cfg, repoRoot, remote, branch, state.upstream === undefined);
      await this.refreshView();
      vscode.window.showInformationMessage(`Pushed ${shortSha} to ${remote}/${branch}.`);
    });
  }

  public async gitPublishBranch(token = this.captureGitProjectToken()): Promise<void> {
    await this.host.enqueue("Git publish branch", async () => {
      const cfg = this.gitConfigForToken(token);
      this.ensureWorkspaceTrustedForGitSync();
      const state = await this.inspectGitRepo(cfg, { fetch: false, requireRemote: true });
      const repoRoot = this.requireGitRepoRoot(state);
      const remote = state.remote ?? cfg.gitSync.remote;
      const branch = this.resolveGitBranch(cfg, state);
      await this.confirmGitPush(`Publish current branch to ${remote}/${branch}?`, cfg);
      await this.pushGitBranch(cfg, repoRoot, remote, branch, true);
      await this.refreshView();
      vscode.window.showInformationMessage(`Published ${remote}/${branch}.`);
    });
  }

  public async gitCreateBranch(token = this.captureGitProjectToken()): Promise<void> {
    const branchName = await vscode.window.showInputBox({
      title: "Create Git Branch",
      prompt: "New branch name",
      validateInput: (value) => this.validateBranchName(value),
    });
    if (!branchName) {
      return;
    }
    await this.host.enqueue("Git create branch", async () => {
      const cfg = this.gitConfigForToken(token);
      this.ensureWorkspaceTrustedForGitSync();
      const state = await this.inspectGitRepo(cfg, { fetch: false });
      const repoRoot = this.requireGitRepoRoot(state);
      const result = await this.runGitCommand(cfg, repoRoot, ["switch", "-c", branchName.trim()], "create branch");
      this.ensureGitSuccess(result, "create branch");
      await this.refreshView();
      vscode.window.showInformationMessage(`Created branch ${branchName.trim()}.`);
    });
  }

  public async gitCheckoutBranch(token = this.captureGitProjectToken()): Promise<void> {
    const cfg = this.gitConfigForToken(token);
    const state = await this.inspectGitRepo(cfg, { fetch: false });
    const repoRoot = this.requireGitRepoRoot(state);
    if (state.worktreeEntries.length > 0) {
      vscode.window.showWarningMessage("Checkout is blocked while local changes are present.");
      return;
    }
    const branchesResult = await this.runGitCommand(cfg, repoRoot, ["branch", "--format=%(refname:short)"], "list branches", { quiet: true });
    this.ensureGitSuccess(branchesResult, "list branches");
    const branches = branchesResult.stdout.split(/\r?\n/).map((line) => line.trim()).filter((line) => line.length > 0);
    const branchName = await vscode.window.showQuickPick(branches, { title: "Checkout Git Branch" });
    if (!branchName) {
      return;
    }
    await this.host.enqueue("Git checkout branch", async () => {
      const runCfg = this.gitConfigForToken(token);
      const result = await this.runGitCommand(runCfg, repoRoot, ["switch", branchName], "checkout branch");
      this.ensureGitSuccess(result, "checkout branch");
      await this.refreshView();
      vscode.window.showInformationMessage(`Checked out ${branchName}.`);
    });
  }

  public async gitConnectRepo(token = this.captureGitProjectToken()): Promise<void> {
    const cfg = this.gitConfigForToken(token);
    this.ensureWorkspaceTrustedForGitSync();
    const state = await this.inspectGitRepo(cfg, { fetch: false, allowMissing: true });
    let repoRoot = state.repoRoot;
    if (!repoRoot) {
      const init = await vscode.window.showWarningMessage(
        `Initialize a Git repository at ${cfg.projectRoot}?`,
        { modal: true },
        "Initialize Repository",
      );
      if (init !== "Initialize Repository") {
        return;
      }
      this.gitConfigForToken(token);
      repoRoot = cfg.projectRoot;
    }
    const remote = await vscode.window.showInputBox({
      title: "Git Remote Name",
      value: cfg.gitSync.remote,
      prompt: "Remote name to connect to Git",
    });
    if (!remote) {
      return;
    }
    this.gitConfigForToken(token);
    const remoteUrl = await vscode.window.showInputBox({
      title: "Git Remote URL",
      value: "",
      placeHolder: state.view.remoteUrl ? `Current: ${state.view.remoteUrl}` : "https://github.com/owner/repo.git or git@github.com:owner/repo.git",
      prompt: "HTTPS or SSH Git repository URL",
      ignoreFocusOut: true,
    });
    if (!remoteUrl) {
      return;
    }

    await this.host.enqueue("Git connect repo", async () => {
      const runCfg = this.gitConfigForToken(token);
      if (!state.repoRoot) {
        const initResult = await this.runGitCommand(runCfg, runCfg.projectRoot, ["init"], "git init");
        this.ensureGitSuccess(initResult, "git init");
      }
      const targetRoot = repoRoot ?? runCfg.projectRoot;
      const currentRemote = await this.runGitCommand(runCfg, targetRoot, ["remote", "get-url", remote.trim()], "get remote", { quiet: true });
      const args = currentRemote.code === 0
        ? ["remote", "set-url", remote.trim(), remoteUrl.trim()]
        : ["remote", "add", remote.trim(), remoteUrl.trim()];
      const remoteResult = await this.runGitCommand(runCfg, targetRoot, args, currentRemote.code === 0 ? "set remote" : "add remote");
      this.ensureGitSuccess(remoteResult, currentRemote.code === 0 ? "set remote" : "add remote");
      await this.refreshView();
      vscode.window.showInformationMessage(`Connected ${remote.trim()} to ${redactRemoteUrl(remoteUrl.trim())}.`);
    });
  }

  public async gitOpenRemote(token = this.captureGitProjectToken()): Promise<void> {
    const cfg = this.gitConfigForToken(token);
    const state = await this.inspectGitRepo(cfg, { fetch: false, allowMissing: true });
    this.gitConfigForToken(token);
    const remoteWebUrl = state.view.remoteWebUrl;
    if (!remoteWebUrl) {
      vscode.window.showWarningMessage("No Git remote URL is configured.");
      return;
    }
    await vscode.env.openExternal(vscode.Uri.parse(remoteWebUrl));
  }

  private async getGitViewState(options: { fetch?: boolean; projectRoot: string }): Promise<GitViewState> {
    const token = this.captureGitProjectToken(options.projectRoot);
    const state = await this.inspectGitRepo(this.gitConfigForToken(token), {
      fetch: options.fetch === true,
      allowMissing: true,
    });
    this.gitConfigForToken(token);
    return state.view;
  }

  public async refreshView(options: { fetch?: boolean } = {}): Promise<void> {
    if (this.gitViewRefreshSuppression > 0) {
      return;
    }
    await vscode.commands.executeCommand("renium.fileExplorer.refreshGit", options);
  }

  private async runGitViewAction(action: string, expectedProjectRoot: string): Promise<void> {
    const token = this.captureGitProjectToken(expectedProjectRoot);
    this.gitViewRefreshSuppression += 1;
    try {
      switch (action) {
        case "connect":
          await this.gitConnectRepo(token);
          return;
        case "fetch":
          await this.gitFetch(token);
          return;
        case "pull":
          await this.gitPull(token);
          return;
        case "commitPush":
          await this.gitCommitAndPush({}, token);
          return;
        case "pullCommitPush":
          await this.gitCommitAndPush({ pullFromStudioFirst: true }, token);
          return;
        case "publishBranch":
          await this.gitPublishBranch(token);
          return;
        case "createBranch":
          await this.gitCreateBranch(token);
          return;
        case "checkoutBranch":
          await this.gitCheckoutBranch(token);
          return;
        case "openRemote":
          await this.gitOpenRemote(token);
          return;
        case "status":
          await this.gitStatus(token);
          return;
        default:
          return;
      }
    } finally {
      this.gitViewRefreshSuppression -= 1;
    }
  }

  private async inspectGitRepo(
    cfg: TConfig,
    options: { fetch?: boolean; requireRemote?: boolean; allowMissing?: boolean } = {},
  ): Promise<GitRepoState> {
    if (!vscode.workspace.isTrusted) {
      const view = emptyGitViewState(
        cfg.projectRoot,
        vscode.workspace.isTrusted,
        "Workspace is not trusted. Trust this workspace before using Git sync.",
      );
      if (options.allowMissing) {
        return { view, entries: [], worktreeEntries: [], ahead: 0, behind: 0 };
      }
      throw new Error(view.message);
    }

    const repoResult = await this.runGitCommand(cfg, cfg.projectRoot, ["rev-parse", "--show-toplevel"], "repo root", { quiet: true });
    if (repoResult.code !== 0) {
      const view = emptyGitViewState(
        cfg.projectRoot,
        vscode.workspace.isTrusted,
        "No Git repository is connected. Use Connect Repo to initialize or configure one.",
      );
      if (options.allowMissing) {
        return { view, entries: [], worktreeEntries: [], ahead: 0, behind: 0 };
      }
      throw new Error(view.message);
    }

    const repoRoot = path.normalize(repoResult.stdout.trim());
    if (!isPathInside(cfg.projectRoot, repoRoot)) {
      throw new Error(`Configured projectRoot is outside the Git repository: ${cfg.projectRoot}`);
    }

    const branchResult = await this.runGitCommand(cfg, repoRoot, ["branch", "--show-current"], "branch", { quiet: true });
    const branch = branchResult.code === 0 ? branchResult.stdout.trim() : "";
    const configuredRemote = cfg.gitSync.remote || "origin";
    const remoteResult = await this.runGitCommand(cfg, repoRoot, ["remote", "get-url", configuredRemote], "remote", { quiet: true });
    const remoteUrl = remoteResult.code === 0 ? remoteResult.stdout.trim() : undefined;
    if (options.requireRemote && !remoteUrl) {
      throw new Error(`Git remote '${configuredRemote}' is not configured. Use the Git tab's Connect Repo action.`);
    }

    if (options.fetch && remoteUrl) {
      const fetchResult = await this.runGitCommand(cfg, repoRoot, ["fetch", "--prune", configuredRemote], "fetch");
      this.ensureGitSuccess(fetchResult, "fetch");
    }

    const branchOverride = cfg.gitSync.branch.trim();
    let upstream: string | undefined;
    let comparisonRef: string | undefined;
    if (branchOverride) {
      const remoteRef = `refs/remotes/${configuredRemote}/${branchOverride}`;
      const remoteBranchResult = await this.runGitCommand(
        cfg,
        repoRoot,
        ["rev-parse", "--verify", `${remoteRef}^{commit}`],
        "configured branch",
        { quiet: true },
      );
      if (remoteBranchResult.code === 0) {
        upstream = `${configuredRemote}/${branchOverride}`;
        comparisonRef = remoteRef;
      }
    } else {
      const upstreamResult = await this.runGitCommand(
        cfg,
        repoRoot,
        ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        "upstream",
        { quiet: true },
      );
      if (upstreamResult.code === 0) {
        upstream = upstreamResult.stdout.trim();
        comparisonRef = "@{u}";
      }
    }
    let ahead = 0;
    let behind = 0;
    if (comparisonRef) {
      const aheadBehindResult = await this.runGitCommand(
        cfg,
        repoRoot,
        ["rev-list", "--left-right", "--count", `HEAD...${comparisonRef}`],
        "ahead/behind",
        { quiet: true },
      );
      if (aheadBehindResult.code === 0) {
        ({ ahead, behind } = parseAheadBehind(aheadBehindResult.stdout));
      }
    }

    const statusResult = await this.runGitCommand(
      cfg,
      repoRoot,
      ["status", "--porcelain=v1", "-z", "-uall"],
      "status",
      { quiet: true },
    );
    this.ensureGitSuccess(statusResult, "status");
    const worktreeEntries = parsePorcelainV1Z(statusResult.stdout);
    const statusScopes = this.defaultGitStageScopes(repoRoot, cfg);
    const entries = worktreeEntries.filter((entry) =>
      this.gitEntryMatchesScopes(entry, statusScopes));
    const counts = summarizeStatus(entries);
    const redactedRemoteUrl = remoteUrl ? redactRemoteUrl(remoteUrl) : undefined;
    const remoteWebUrl = remoteUrlToWebUrl(remoteUrl ?? "");
    const worktreeConflicts = worktreeEntries.filter((entry) => entry.conflicted).length;
    const messages: string[] = [];
    if (!remoteUrl) {
      messages.push(`Remote '${configuredRemote}' is not configured.`);
    } else if (worktreeConflicts > 0) {
      messages.push(`${worktreeConflicts} conflicted file(s) need manual resolution.`);
    } else if (behind > 0) {
      messages.push(`${behind} remote commit(s) available to pull.`);
    }
    const hiddenChanges = worktreeEntries.length - entries.length;
    if (hiddenChanges > 0) {
      messages.push(`${hiddenChanges} repository change(s) outside this place's source files are hidden.`);
    }
    const message = messages.length > 0 ? messages.join(" ") : undefined;
    const view: GitViewState = {
      ok: Boolean(remoteUrl) && worktreeConflicts === 0,
      message,
      trusted: true,
      projectRoot: cfg.projectRoot,
      repoRoot,
      connected: Boolean(remoteUrl),
      branch: branch || undefined,
      upstream,
      remote: configuredRemote,
      remoteUrl: redactedRemoteUrl,
      remoteWebUrl,
      ahead,
      behind,
      counts,
      entries: entries.map((entry) => ({
        path: entry.path,
        originalPath: entry.originalPath,
        kind: entry.kind,
        staged: entry.staged,
        unstaged: entry.unstaged,
        untracked: entry.untracked,
        conflicted: entry.conflicted,
        deleted: entry.deleted,
      })),
      lastUpdated: new Date().toISOString(),
    };
    return {
      view,
      entries,
      worktreeEntries,
      repoRoot,
      branch,
      upstream,
      remote: configuredRemote,
      remoteUrl,
      ahead,
      behind,
    };
  }

  private ensureWorkspaceTrustedForGitSync(): void {
    if (!vscode.workspace.isTrusted) {
      throw new Error("Workspace is not trusted. Trust this workspace before running Git sync commands.");
    }
  }

  private requireGitRepoRoot(state: GitRepoState): string {
    if (!state.repoRoot) {
      throw new Error(state.view.message || "No Git repository is connected.");
    }
    return state.repoRoot;
  }

  private ensureNoGitConflicts(state: GitRepoState): void {
    const conflicts = state.worktreeEntries.filter((entry) => entry.conflicted);
    if (conflicts.length > 0) {
      throw new Error(`Git sync is blocked by ${conflicts.length} conflicted file(s). Resolve conflicts before continuing.`);
    }
  }

  private resolveGitBranch(cfg: TConfig, state: GitRepoState): string {
    const branch = cfg.gitSync.branch.trim() || state.branch?.trim() || "";
    if (!branch) {
      throw new Error("Current Git HEAD is detached. Checkout or create a branch before using Git sync.");
    }
    return branch;
  }

  private async runGitCommand(
    cfg: TConfig,
    cwd: string,
    args: string[],
    label: string,
    options: { quiet?: boolean } = {},
  ): Promise<GitRunResult> {
    const quiet = options.quiet === true || cfg.gitSync.outputBehavior === "silent";
    if (!quiet && cfg.gitSync.outputBehavior === "onStart") {
      this.host.output.show(false);
    }
    if (!quiet) {
      this.host.output.appendLine(`[git-sync] ${label}: git ${renderGitArgs(args)}`);
    }
    const result = await runGit(args, {
      cwd,
      gitPath: cfg.gitSync.gitPath,
      timeoutMs: Math.max(10, cfg.gitSync.timeoutSeconds) * 1000,
      owner: projectProcessOwner(cfg.projectRoot),
    });
    if (!quiet) {
      const output = redactRemoteUrl(result.output.trim());
      if (output) {
        for (const line of output.replace(/\r\n/g, "\n").split("\n").slice(-80)) {
          this.host.output.appendLine(`[git-sync:git] ${line}`);
        }
      }
      this.host.output.appendLine(`[git-sync] ${label}: exited code=${result.code}${result.timedOut ? " (timed out)" : ""}`);
    }
    return result;
  }

  private ensureGitSuccess(result: GitRunResult, label: string): void {
    if (result.code === 0 && !result.timedOut) {
      return;
    }
    const detail = redactRemoteUrl((result.stderr || result.stdout || result.output || "").trim());
    const timeout = result.timedOut ? " timed out" : "";
    throw new Error(`Git ${label}${timeout} failed with code ${result.code}.${detail ? ` ${detail}` : ""}`);
  }

  private async gitOutput(cfg: TConfig, repoRoot: string, args: string[], label: string): Promise<string> {
    const result = await this.runGitCommand(cfg, repoRoot, args, label, { quiet: true });
    this.ensureGitSuccess(result, label);
    return result.stdout.trim();
  }

  private async gitChangedFilesBetween(cfg: TConfig, repoRoot: string, oldHead: string, newHead: string): Promise<GitNameStatusEntry[]> {
    const result = await this.runGitCommand(cfg, repoRoot, ["diff", "--name-status", "-z", oldHead, newHead], "changed files", { quiet: true });
    this.ensureGitSuccess(result, "changed files");
    return parseNameStatusZ(result.stdout);
  }

  private async gitStagedChanges(cfg: TConfig, repoRoot: string): Promise<GitNameStatusEntry[]> {
    const result = await this.runGitCommand(cfg, repoRoot, ["diff", "--cached", "--name-status", "-z"], "staged changes", { quiet: true });
    this.ensureGitSuccess(result, "staged changes");
    return parseNameStatusZ(result.stdout);
  }

  private async refreshExplorerForGitPaths(repoRoot: string, changedFiles: GitNameStatusEntry[], cfg: TConfig): Promise<void> {
    const services = new Set<string>();
    for (const affectedPath of nameStatusAffectedPaths(changedFiles)) {
      const absolutePath = path.join(repoRoot, affectedPath);
      for (const service of this.host.servicesForProjectSourcePath(absolutePath, cfg)) {
        services.add(service);
      }
    }
    if (services.size === 0) {
      return;
    }
    await vscode.commands.executeCommand("renium.fileExplorer.refreshServices", Array.from(services));
  }

  private async maybeApplyPulledPathsToStudio(repoRoot: string, changedFiles: GitNameStatusEntry[], cfg: TConfig): Promise<void> {
    const srcPaths = nameStatusAffectedPaths(changedFiles)
      .map((affectedPath) => path.join(repoRoot, affectedPath))
      .filter((filePath) => this.host.isProjectSourcePath(filePath, cfg));
    if (srcPaths.length === 0 || cfg.gitSync.applyPulledChangesToStudio === "never") {
      return;
    }
    let apply = cfg.gitSync.applyPulledChangesToStudio === "always";
    if (!apply) {
      const picked = await vscode.window.showInformationMessage(
        `Apply ${srcPaths.length} pulled project source file(s) to Studio now?`,
        { modal: true },
        "Apply to Studio",
      );
      apply = picked === "Apply to Studio";
    }
    if (!apply) {
      return;
    }
    const pushed = await this.host.pushEditorPathsNow(srcPaths, { force: true, skipChangeFilter: true, taskName: "Git pull -> Studio sync" });
    if (!pushed) {
      vscode.window.showInformationMessage("Pulled changes stayed local. Start Serve or live sync before applying to Studio.");
    }
  }

  private async ensureLiveSyncStoppedForGitPull(): Promise<boolean> {
    if (!this.host.isLiveSyncActiveOrStarting()) {
      return false;
    }
    const picked = await vscode.window.showWarningMessage(
      "Git pull can rewrite project source files. Stop Renium live sync before pulling?",
      { modal: true },
      "Stop Live Sync",
    );
    if (picked !== "Stop Live Sync") {
      throw new Error("Git pull cancelled because live sync is active.");
    }
    await this.host.stopLiveSync();
    return true;
  }

  private async maybePullFromStudioBeforeGitPush(cfg: TConfig, forced: boolean): Promise<void> {
    let choice: "pull" | "current" | undefined;
    if (!forced && cfg.gitSync.pullFromStudioBeforePush === "ask") {
      const picked = await vscode.window.showInformationMessage(
        "Pull from Studio before committing to Git?",
        { modal: true },
        "Pull from Studio",
        "Commit Current Files",
      );
      if (!picked) {
        throw new Error("Git commit cancelled before the Studio pull choice.");
      }
      choice = picked === "Pull from Studio" ? "pull" : "current";
    }
    if (!shouldPullFromStudioBeforePush(cfg.gitSync.pullFromStudioBeforePush, forced, choice)) {
      return;
    }
    await this.host.pullFromStudio(cfg);
  }

  private async stageGitSyncChanges(cfg: TConfig, repoRoot: string): Promise<void> {
    const configuredPaths = cfg.gitSync.stagePaths.map((value) => value.trim()).filter((value) => value.length > 0);
    const hasConfiguredPaths = cfg.gitSync.stageMode === "configuredPaths" && configuredPaths.length > 0;
    const defaultScopes = this.defaultGitStageScopes(repoRoot, cfg);
    const args = cfg.gitSync.includeUntracked
      ? ["add", "-A", "--", ...(hasConfiguredPaths ? configuredPaths : defaultScopes)]
      : ["add", "-u", "--", ...(hasConfiguredPaths ? configuredPaths : defaultScopes)];
    const result = await this.runGitCommand(cfg, repoRoot, args, "stage changes");
    this.ensureGitSuccess(result, "stage changes");
  }

  private async plannedGitStageChanges(cfg: TConfig, repoRoot: string): Promise<GitNameStatusEntry[]> {
    const configuredPaths = cfg.gitSync.stagePaths.map((value) => value.trim()).filter((value) => value.length > 0);
    const scopes = cfg.gitSync.stageMode === "configuredPaths" && configuredPaths.length > 0
      ? configuredPaths
      : this.defaultGitStageScopes(repoRoot, cfg);
    const result = await this.runGitCommand(
      cfg,
      repoRoot,
      ["status", "--porcelain=v1", "-z", "-uall", "--", ...scopes],
      "stage preview",
      { quiet: true },
    );
    this.ensureGitSuccess(result, "stage preview");
    return parsePorcelainV1Z(result.stdout)
      .filter((entry) => entry.tracked || cfg.gitSync.includeUntracked)
      .map((entry) => ({
        status: this.gitNameStatusForEntry(entry),
        path: entry.path,
        originalPath: entry.originalPath,
      }));
  }

  private gitNameStatusForEntry(entry: GitStatusEntry): string {
    if (entry.conflicted) {
      return "U";
    }
    if (entry.untracked) {
      return "A";
    }
    const status = entry.index.trim() || entry.worktree.trim();
    return status || "M";
  }

  private defaultGitStageScopes(repoRoot: string, cfg: TConfig): string[] {
    const scopes = Array.from(new Set(loadProjectSourceGraph(cfg.projectRoot).locations.map((location) => {
      if (!isPathInside(location, repoRoot)) {
        throw new Error(`Project source path is outside the Git repository: ${location}`);
      }
      return path.relative(repoRoot, location).split(path.sep).join("/") || ".";
    })));
    scopes.sort((left, right) => left.length - right.length || left.localeCompare(right));
    return scopes.filter((scope, index) => {
      const key = process.platform === "win32" ? scope.toLowerCase() : scope;
      return !scopes.slice(0, index).some((parent) => {
        const parentKey = process.platform === "win32" ? parent.toLowerCase() : parent;
        return parentKey === "." || key === parentKey || key.startsWith(`${parentKey}/`);
      });
    });
  }

  private gitEntryMatchesScopes(entry: GitStatusEntry, scopes: string[]): boolean {
    const matches = (filePath: string | undefined): boolean => {
      if (!filePath) {
        return false;
      }
      const value = process.platform === "win32" ? filePath.toLowerCase() : filePath;
      return scopes.some((scope) => {
        const key = process.platform === "win32" ? scope.toLowerCase() : scope;
        return key === "." || value === key || value.startsWith(`${key}/`);
      });
    };
    return matches(entry.path) || matches(entry.originalPath);
  }

  private async confirmGitCommitAndPush(staged: GitNameStatusEntry[], state: GitRepoState, cfg: TConfig): Promise<void> {
    if (!cfg.gitSync.confirmBeforePush) {
      return;
    }
    const deleted = staged.filter((entry) => entry.status.includes("D")).length;
    const summary = `${staged.length} staged file(s)${deleted > 0 ? `, including ${deleted} deletion(s)` : ""}. Push target: ${state.remote}/${this.resolveGitBranch(cfg, state)}.`;
    const picked = await vscode.window.showWarningMessage(
      `${summary}\n\nUntracked files are ${cfg.gitSync.includeUntracked ? "included by setting" : "excluded by default"}.`,
      { modal: true },
      "Commit & Push",
    );
    if (picked !== "Commit & Push") {
      throw new Error("Git commit & push cancelled.");
    }
  }

  private async confirmGitPush(message: string, cfg: TConfig): Promise<void> {
    if (!cfg.gitSync.confirmBeforePush) {
      return;
    }
    const picked = await vscode.window.showWarningMessage(message, { modal: true }, "Push");
    if (picked !== "Push") {
      throw new Error("Git push cancelled.");
    }
  }

  private async gitCommitMessage(cfg: TConfig, branch: string): Promise<string> {
    const value = await vscode.window.showInputBox({
      title: "Git Commit Message",
      value: buildCommitMessage(cfg.gitSync.commitMessageTemplate, branch),
      prompt: "Commit message for the selected Renium changes",
      ignoreFocusOut: true,
      validateInput: (input) => input.trim().length === 0 ? "Commit message is required." : undefined,
    });
    const message = value?.trim() ?? "";
    if (!message) {
      throw new Error("Git commit cancelled because no commit message was provided.");
    }
    return message;
  }

  private async pushGitBranch(cfg: TConfig, repoRoot: string, remote: string, branch: string, setUpstream: boolean): Promise<void> {
    const args = setUpstream
      ? ["push", "-u", remote, `HEAD:${branch}`]
      : ["push", remote, `HEAD:${branch}`];
    const result = await this.runGitCommand(cfg, repoRoot, args, "push");
    this.ensureGitSuccess(result, "push");
  }

  private validateBranchName(value: string): string | undefined {
    const branch = value.trim();
    if (!branch) {
      return "Branch name is required.";
    }
    if (/\s/.test(branch) || branch.startsWith("-") || branch.includes("..") || branch.includes("~") || branch.includes("^") || branch.includes(":")) {
      return "Branch name contains invalid characters.";
    }
    return undefined;
  }

  private logGitState(state: GitRepoState): void {
    const counts = state.view.counts;
    this.host.output.appendLine(`[git-sync] repo=${state.repoRoot ?? "not connected"}`);
    this.host.output.appendLine(`[git-sync] branch=${state.branch ?? "detached"} upstream=${state.upstream ?? "none"} remote=${state.remote ?? "none"}`);
    if (state.remoteUrl) {
      this.host.output.appendLine(`[git-sync] remoteUrl=${redactRemoteUrl(state.remoteUrl)}`);
    }
    this.host.output.appendLine(`[git-sync] ahead=${state.ahead} behind=${state.behind} changed=${counts.total} staged=${counts.staged} unstaged=${counts.unstaged} untracked=${counts.untracked} conflicts=${counts.conflicted}`);
    for (const entry of state.entries.slice(0, 40)) {
      this.host.output.appendLine(`[git-sync] ${entry.kind.padEnd(10)} ${entry.path}`);
    }
    if (state.entries.length > 40) {
      this.host.output.appendLine(`[git-sync] ... ${state.entries.length - 40} more file(s)`);
    }
  }

}
