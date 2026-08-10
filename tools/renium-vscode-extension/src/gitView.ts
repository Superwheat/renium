type GitViewEntry = {
  path: string;
  originalPath?: string;
  kind: string;
  staged: boolean;
  unstaged: boolean;
  untracked: boolean;
  conflicted: boolean;
  deleted: boolean;
};

export type GitViewState = {
  ok: boolean;
  message?: string;
  trusted: boolean;
  projectRoot?: string;
  repoRoot?: string;
  connected: boolean;
  branch?: string;
  upstream?: string;
  remote?: string;
  remoteUrl?: string;
  remoteWebUrl?: string;
  ahead: number;
  behind: number;
  counts: {
    total: number;
    tracked: number;
    staged: number;
    unstaged: number;
    untracked: number;
    ignored: number;
    conflicted: number;
    deleted: number;
  };
  entries: GitViewEntry[];
  lastUpdated: string;
};

export function emptyGitViewState(
  projectRoot: string,
  trusted: boolean,
  message?: string,
): GitViewState {
  return {
    ok: false,
    message,
    trusted,
    projectRoot,
    connected: false,
    ahead: 0,
    behind: 0,
    counts: {
      total: 0,
      tracked: 0,
      staged: 0,
      unstaged: 0,
      untracked: 0,
      ignored: 0,
      conflicted: 0,
      deleted: 0,
    },
    entries: [],
    lastUpdated: new Date().toISOString(),
  };
}

type GitViewContext = {
  projectRoot: string;
};

export type GitViewActions = {
  refresh: (context: GitViewContext & { fetch?: boolean }) => Promise<GitViewState>;
  runAction: (action: string, context: GitViewContext) => Promise<void>;
  openOutput: () => void;
  openDiff: (path: string, context: GitViewContext) => Promise<void>;
};
