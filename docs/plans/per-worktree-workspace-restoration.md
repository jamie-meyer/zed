# Per-Worktree Workspace Restoration

## Goal

Restore each worktree's editor layout and explicit active surface across Zed
restarts while opening inactive worktrees lazily. Build on Zed's existing
`WorkspaceDb` serialization rather than introducing another layout store.

## Existing foundation

Zed already serializes pane trees, open items, the active item and pane, pinned
items, docks, window state, and root paths. Opening a workspace for an exact root
path already looks up that saved workspace. `MultiWorkspace` also retains open
workspace entities during one process lifetime.

The missing durable state is primarily which worktree was last active within a
project group and whether its selected surface was the worktree or an explicit
terminal.

## Product invariants

- Every linked worktree has an independent editor layout.
- Switching worktrees in one Zed process preserves both layouts.
- Restarting Zed restores the last active worktree immediately.
- Other worktrees reopen lazily when selected, using their exact saved root
  state.
- A saved worktree surface restores with the Agent panel hidden.
- A saved terminal surface reattaches only to that explicit live terminal.
- A missing terminal falls back to the worktree surface, never an empty Agent
  draft and never a terminal from another worktree.
- Worktree teardown deletes its saved workspace state so a future checkout at
  the same path cannot inherit stale tabs and docks.

## Phase 1: Persist navigation identity

Extend multi-workspace serialization with:

- last active worktree identity per project group
- active surface as either `worktree` or `terminal(<terminal-id>)`

Use a stable repository/worktree identity where available and retain the root
path as a recovery fallback. Do not serialize entity IDs or weak entity handles.

## Phase 2: Restore the active worktree

1. Resolve the saved project group and worktree.
2. Open its exact root set through the existing workspace restore path.
3. Restore panes, items, docks, and focus using `WorkspaceDb`.
4. Apply the saved active surface only after terminal-registry reconciliation:
   - `worktree`: keep the Agent panel closed and focus the center pane.
   - live terminal: attach and focus it.
   - missing terminal: select the worktree surface.

Restoration must not create a new Codex process.

## Phase 3: Lazy restoration

Keep inactive worktrees represented in the sidebar without constructing their
projects, language servers, buffers, or Agent panels. On first selection, open
the exact worktree roots and let the existing database restore its workspace.

Record restored workspaces in the current `MultiWorkspace` so subsequent
switches are entity activation rather than reconstruction.

## Phase 4: Teardown and invalidation

When a worktree is removed:

1. stop its managed terminal sessions
2. close its workspace
3. remove its serialized workspace record and active-surface reference
4. remove the checkout

If the checkout disappears externally, retain enough metadata to explain the
missing worktree but do not reopen or index it.

## Failure handling

- Corrupt workspace state falls back to a clean editor workspace and reports
  the restoration error.
- Missing files are skipped by the existing item restore path.
- A moved worktree may be matched by stable identity; ambiguous matches require
  an explicit choice.
- Restoration never blocks the UI on terminal or tmux discovery.

## Verification

- Create three worktrees with distinct tabs, splits, active files, and dock
  layouts; switch repeatedly and verify isolation.
- Restart with the worktree surface active and verify no Agent panel appears.
- Restart with a terminal active and verify only that terminal reattaches.
- Stop the saved active terminal before restart and verify worktree fallback.
- Keep two worktrees inactive at startup and verify their projects and language
  servers are not started until selection.
- Remove and recreate a worktree at the same path and verify stale layout state
  is not reused.

## Out of scope

- Sharing language-server processes across worktrees.
- Restoring arbitrary transient modals or menus.
- Eagerly opening every saved worktree at startup.
