# Durable Terminal Registry

## Goal

Make terminal-backed agent sessions durable across Zed restarts without treating
the Agent panel's in-memory terminal list as the source of truth. Closing a view,
stopping a process, removing a worktree, and restarting Zed must remain distinct
operations.

## Product invariants

- A worktree is a selectable surface independent of its terminal sessions.
- Selecting a worktree activates its workspace and editor; it does not create or
  select a Codex session.
- Selecting a terminal explicitly attaches to that terminal.
- Closing the active terminal selects another terminal only when it belongs to
  the same worktree.
- Closing the last terminal in the active worktree hides the Agent panel and
  leaves the worktree active.
- The empty built-in Agent draft is never used as a fallback for a terminal
  session.
- Closing a terminal view detaches it. Stopping a terminal kills its tmux session.
- Removing a worktree tears down the tmux sessions owned by that worktree.
- Restarting Zed does not kill managed tmux sessions or start missing sessions.

## Phase 0: Correct selection and close behavior

1. Limit terminal-close fallback selection to the terminal's worktree.
2. When no same-worktree terminal remains, close the Agent panel, focus the
   workspace center, and clear the active terminal selection.
3. Make worktree selection close the Agent panel and focus the workspace center,
   including when the worktree is already active.
4. Remove the terminal-close path that creates an empty Agent draft.
5. Add GPUI tests for last-terminal close, same-worktree fallback,
   cross-worktree isolation, and worktree selection.

## Phase 1: Extend the existing metadata store

Use `TerminalThreadMetadataStore` as the registry owner rather than adding a
second database. Extend each record with the durable identity and intent needed
for reconciliation:

- terminal kind and provider
- tmux server and session names
- optional provider session identifier
- desired lifecycle state (`attached`, `detached`, or `stopping`)
- last attachment time and update time

Running, waiting, completed, and error remain live observations derived from
tmux output. They must not be persisted as authoritative process state.

Mirror only the minimum recovery identity into tmux options: terminal ID,
worktree path, terminal kind, and provider. SQLite remains the richer catalog;
tmux remains the process authority.

## Phase 2: Reconcile on startup and workspace changes

Reconcile registry rows and tmux sessions without automatically attaching every
terminal:

| Registry | tmux | Result |
|---|---|---|
| present | present | Adopt the live session and make it attachable |
| present | absent | Mark missing/resumable; do not auto-start |
| absent | present | Surface as recoverable orphan after validating ownership |
| stopping | present | Retry cleanup and never resurrect it |

Run reconciliation off the UI thread, coalesce updates, and notify the sidebar
only when the computed terminal set or status changes.

## Phase 3: Separate view and process actions

Expose unambiguous actions:

- **Close View** detaches and preserves the registry record and tmux session.
- **Stop Session** kills tmux, waits for confirmation, and deletes the record.
- **Reattach** opens the Agent panel and attaches to the existing tmux session.
- **Forget Missing Session** deletes a registry-only record.

Normal tab close should use **Close View**. Worktree teardown should use **Stop
Session** for every owned terminal before removing the checkout.

## Failure handling

- A failed tmux attach leaves the record visible with a retryable error.
- A failed kill leaves the record in `stopping` so the next reconciliation can
  retry.
- Invalid or foreign tmux metadata is never adopted automatically.
- Duplicate registry rows for one terminal ID are resolved deterministically and
  reported.
- Missing worktree paths do not force workspaces to open during reconciliation.

## Verification

- Unit-test registry migration, reconciliation pairs, and lifecycle transitions.
- GPUI-test selection behavior with two worktrees and multiple terminals.
- Restart Zed with attached and detached sessions and verify no process loss.
- Crash Zed, relaunch, and reattach to the same tmux pane and scrollback.
- Remove a worktree and verify only its owned sessions are killed.
- Confirm an inactive session changing status updates the sidebar without
  opening its workspace or Agent panel.

## Out of scope

- Restoring arbitrary non-managed shell terminals.
- tmux-resurrect integration.
- Supporting both direct PTYs and tmux-backed sessions.
- Persisting terminal render state as a substitute for tmux scrollback.
