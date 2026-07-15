---
title: Parallel Codex Worktrees - Zed
description: Run native CLI Codex sessions across isolated Git worktrees without leaving Zed.
---

# Parallel Codex Worktrees

The Worktree Sidebar organizes native CLI Codex terminals by Git worktree. Each worktree has its own checkout and terminal sessions, so multiple Codex tasks can run concurrently without editing the same files.

Open the Worktree Sidebar with the worktree button in the Codex panel toolbar or {#kb multi_workspace::ToggleWorkspaceSidebar}.

Use **Panel Layout > Agentic** from the user menu in the title bar (or the {#action workspace::UseAgenticLayout} action) to place the Codex panel and Worktree Sidebar on the left, with the Project Panel, Git Panel, and other panels on the right. Use **Panel Layout > Classic** (or {#action workspace::UseClassicLayout}) to restore the editor-oriented layout.

## Worktree Sidebar {#threads-sidebar}

Each main or linked worktree gets a header with its branch, path, session count, and aggregate running or attention state. The filter button can limit the list to worktrees currently open in the window.

Codex and regular terminal sessions appear beneath their worktree with a terminal icon. Click one to activate its workspace and terminal.

To focus the sidebar without toggling it, use {#kb multi_workspace::FocusWorkspaceSidebar}. To search terminals, press {#kb agents_sidebar::FocusSidebarFilter} while the sidebar is focused.

### Switching Sessions {#switching-threads}

Click any session in the sidebar to switch to its terminal. For quick switching without opening the sidebar, press {#kb agents_sidebar::ToggleThreadSwitcher} to cycle forward through recent sessions, or hold `Shift` while pressing that binding to go backward.

## Running Multiple Sessions {#running-multiple-threads}

Each Codex terminal runs independently. Hover over a worktree and click `+` to start another Codex session in that checkout, or use {#action agents_sidebar::NewThreadInGroup}.

The `+` menu in the Codex panel also provides **Terminal** for a regular interactive shell. The worktree workflow does not use ACP or Zed's built-in agent harness.

## Multiple Projects {#multiple-projects}

The Worktree Sidebar can hold multiple projects at once. Each project contains its own worktrees and terminal sessions.

To add another project, click the **Add Project** button in the sidebar bottom bar. The popover lists recent projects and provides **Add Local Folders** and **Add Remote Folder** actions.

### Multi-Root Folder Projects {#multi-root-folder-projects}

A project can contain multiple folders. Add them from the sidebar's **Add Local Folders** action, the title-bar project picker, or **Add Folders to Project** in the Project Panel.

## Worktree Isolation {#worktree-isolation}

Create a worktree from a project header in the sidebar or from the title-bar worktree picker. Zed creates an isolated checkout, copies configured local files, runs setup hooks, opens the checkout, waits for its initial scan, and starts native Codex in a terminal.

New worktrees begin in a detached HEAD state. Use the branch picker to create a branch or check out an existing one.

Use [`.worktreeinclude`](../git.md#worktree-setup-files) to copy local configuration into new worktrees before indexing. Configure [`create_worktree` and `remove_worktree` task hooks](../tasks.md#hooks) for setup and teardown commands.

After Codex finishes, review the diff and merge it through your normal Git workflow. Removing a worktree runs configured teardown hooks before deleting the checkout.

## See Also {#see-also}

- [Terminal Threads](./terminal-threads.md): How native CLI sessions work
- [Git Worktrees](../git.md#git-worktrees): Create and manage isolated checkouts
- [Tasks](../tasks.md#hooks): Configure worktree setup and teardown hooks
