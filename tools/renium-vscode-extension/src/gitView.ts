export type GitViewEntry = {
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

export type GitViewActions = {
  refresh: (options?: { fetch?: boolean }) => Promise<GitViewState>;
  runAction: (action: string) => Promise<void>;
  openOutput: () => void;
  openDiff: (path: string) => Promise<void>;
};
