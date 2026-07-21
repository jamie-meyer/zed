# Graphite Support

## Goal

Add worktree-aware Graphite visibility and actions only when the current branch
is tracked by Graphite. Preserve the terminal-first workflow and never move a
different branch into the current worktree implicitly.

## Product invariants

- Graphite UI is absent when `gt` is unavailable, the repository is not
  initialized, the current branch is untracked, or detection fails.
- Zed never runs `gt track` automatically.
- Read-only topology is the first deliverable.
- Branch navigation respects linked worktrees.
- Mutating Graphite commands run visibly in managed terminals.
- Only one mutating Graphite operation runs per repository at a time.
- Git remains the repository authority; Graphite supplies stack relationships
  and operations.

## Phase 1: CLI adapter and tracked-branch detection

Create one repository-scoped Graphite store. Resolve `gt` using the same login
shell environment available to managed terminals. Execute commands with color
and paging disabled, apply timeouts, capture stderr, and parse through a
version-aware adapter with fixture tests.

For the current branch, query Graphite branch information and distinguish:

- tracked
- untracked
- Graphite unavailable or uninitialized
- command or parse error

Only the positive tracked state enables Graphite UI. Negative and error states
must not leave empty controls or placeholders in the sidebar.

## Phase 2: Read-only stack UI

For a tracked current branch, display reliable stack information:

- parent and children
- position in the stack
- restack requirement
- pull-request state when the installed CLI exposes it reliably

Cache topology per repository. Refresh after branch changes, relevant Git
changes, and completion of a Zed-launched Graphite command. Debounce passive
refreshes and avoid polling inactive repositories.

## Phase 3: Worktree-aware navigation

Selecting a stack branch follows this order:

1. If its workspace is open, activate it.
2. If its linked worktree exists but is closed, open that worktree workspace.
3. If it has no linked worktree, offer **Open in New Worktree**.

Never checkout the selected branch into the current worktree as an implicit
navigation side effect. Surface conflicts such as a branch checked out in
another worktree instead of trying to bypass Git's worktree protections.

## Phase 4: Terminal-first actions

Add explicit actions for the stable workflows users need most:

- modify
- restack
- sync
- submit

Launch each command in a managed terminal rooted in the correct worktree so its
output, prompts, and recovery are visible. Feed completion back into the
Graphite store and Git status refresh. Do not create a parallel hidden command
runner for mutations.

## Phase 5: Stack creation

Defer stacked branch creation until read-only topology and navigation have been
tested with multiple linked worktrees. Define creation semantics explicitly for
branch name, parent, worktree path, setup hooks, failure rollback, and which new
workspace becomes active.

## Failure handling

- CLI timeout or parse failure hides Graphite controls and records a diagnostic.
- A mutating command failure stays visible in its terminal and does not alter
  cached topology until a successful refresh.
- CLI version changes are handled by adapter selection, not scattered string
  parsing.
- Repository operations are serialized to avoid overlapping `sync`, `restack`,
  and `submit` mutations.
- A deleted or moved worktree invalidates its navigation target without changing
  the stack.

## Verification

- Fixture-test tracked, untracked, unavailable, malformed, and multiple-version
  CLI outputs.
- Verify Graphite UI never appears on an untracked branch.
- Test navigation to an open workspace, a closed linked worktree, and a branch
  without a worktree.
- Test that navigation never changes the current worktree's checked-out branch.
- Run mutations from two worktrees and verify repository serialization.
- Verify successful command completion refreshes stack and Git state.
- Verify failures remain inspectable in the managed terminal.

## Out of scope

- GitHub-native stacked pull requests independent of Graphite.
- jj support.
- Reimplementing Graphite stack metadata inside Zed.
- Hidden background execution for mutating Graphite commands.
