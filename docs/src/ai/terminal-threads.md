---
title: Terminal Threads - Zed
description: Run agent CLIs and TUIs directly in terminal-backed threads in Zed.
---

# Terminal Threads

Terminal Threads are terminal-backed sessions in the [Worktree Sidebar](./parallel-agents.md#threads-sidebar). Zed starts native CLI Codex in this surface by default.

Terminal Threads are different from [External Agents](./external-agents.md). External Agents integrate with Zed through ACP and render as agent threads. Terminal Threads run the native command-line tool in a terminal that Zed organizes as a thread.

## What Zed Owns {#what-zed-owns}

Zed owns the session surface:

- the terminal-backed session in the Worktree Sidebar
- grouping sessions by Git worktree
- switching, monitoring, and organizing terminal sessions

## What the CLI Owns {#what-the-cli-owns}

The CLI or TUI running inside the terminal owns its own:

- authentication
- model/provider configuration
- subscriptions or API keys
- tool configuration
- skills and instruction files
- MCP configuration

Zed Agent profiles, Zed Agent tool permissions, Zed Skills, and Zed Agent MCP settings do not automatically apply to Terminal Threads.

## Opening Codex {#opening-a-terminal-thread}

Click `+` in the Codex panel or beside a worktree in the Worktree Sidebar. Zed creates a real terminal in that worktree and starts the native `codex` CLI. Codex uses its own terminal UI, configuration, authentication, and instruction files; it does not use ACP or Zed's built-in agent harness.

Open the `+` menu and choose **Terminal** when you need a regular shell instead. Creating a new worktree also opens it and starts Codex automatically.

You can open as many sessions as you like. Each gets its own entry beneath its worktree.

## Running a Command Automatically {#terminal-thread-init-command}

The `agent.terminal_init_command` setting applies to regular Terminal sessions. Codex sessions always start `codex` directly. To run another command automatically in regular Terminal sessions, set:

```json [settings]
{
  "agent": {
    "terminal_init_command": "claude"
  }
}
```

The command is sent to the shell as if you had typed it, so it is interpreted by your configured shell—including on Windows and in remote or WSL projects—and the terminal remains a regular interactive shell after the command exits. It runs when creating a new Terminal Thread and when recreating a saved Terminal Thread after reopening a project.

You can also configure this from the Settings UI under **AI**, via the "Terminal Thread Init Command" field.

## Terminal Thread Titles {#terminal-thread-titles}

The terminal title in the toolbar updates automatically to reflect the running shell or process. You can also set a custom name by clicking the title or the pencil icon that appears on hover.

## Notifications {#terminal-thread-notifications}

When a terminal produces a bell character while not in focus, Zed notifies you the same way it does when an agent finishes: with a visual pop-up and an optional sound. Clicking the notification brings the terminal into focus and clears the indicator.

The same `agent.notify_when_agent_waiting` and `agent.play_sound_when_agent_done` settings apply.

## Closing Terminal Threads {#closing-terminal-threads}

Unlike agent threads, Terminal Threads are closed rather than archived. They do not go to Thread History. To close one, hover over it in the Threads Sidebar and click the **×** button, or select it and press {#kb agent::ArchiveSelectedThread}.

## CLI/TUI Setup Notes {#cli-setup}

Some agent CLIs and TUIs can send terminal signals, such as bell notifications or title updates, that Zed uses to show useful context in the sidebar.

### Claude Code Notifications {#claude-code-notifications}

Claude Code can notify you when it finishes a task or pauses for permission. To enable this, set `preferredNotifChannel` to `"terminal_bell"` in your Claude Code user settings:

```json
{
  "preferredNotifChannel": "terminal_bell"
}
```

You can also set this from within Claude Code by running `/config`, selecting `Local Notifications`, and choosing `Terminal Bell`.

> If you run Claude Code inside tmux, bell notifications may not reach the outer terminal unless passthrough is enabled. Add this to `~/.tmux.conf`:
>
> ```
> set -g allow-passthrough on
> ```

For more, see the [Claude Code documentation](https://code.claude.com/docs/en/terminal-config).

### Amp Notifications {#amp-notifications}

Amp updates terminal titles automatically and can also notify you when it needs your attention. To enable notifications in Zed Terminal Threads, add `AMP_FORCE_BEL=1` to your terminal environment settings:

```json [settings]
{
  "terminal": {
    "env": {
      "AMP_FORCE_BEL": "1"
    }
  }
}
```

Restart Amp after adding the environment variable.

### OpenCode Notifications {#opencode-notifications}

OpenCode can update terminal titles automatically. For Zed notifications, add an OpenCode plugin that emits a terminal bell when OpenCode needs your attention.

Create `.opencode/plugins/zed-bell.js` in your project, or `~/.config/opencode/plugins/zed-bell.js` to use it globally:

```js
export const ZedBell = async () => {
  return {
    event: async ({ event }) => {
      if (event.type === "session.idle" || event.type === "permission.asked") {
        process.stdout.write("\x07");
      }
    },
  };
};
```

Restart OpenCode after adding the plugin.

### Pi Notifications {#pi-notifications}

Pi can use an extension to emit a notification when it finishes a turn. Create `.pi/extensions/zed-bell.ts` in your project, or `~/.pi/agent/extensions/zed-bell.ts` to use it globally:

```ts
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
  pi.on("agent_end", async () => {
    process.stdout.write("\x07");
  });
}
```

Restart Pi after adding the extension, or run `/reload` if the extension is in one of Pi's auto-discovered extension locations.

### Codex Terminal Titles {#codex-terminal-titles}

Codex updates the terminal title as it works. Zed maps that title to the same sidebar states used by integrated agent threads: a spinner while Codex is working, a warning when Codex needs approval or user input, and an idle terminal icon when the turn is complete. Worktree and project headers show the aggregate state when their sessions are collapsed.

To configure this from within Codex, run `/title` and use the picker to choose which fields appear and in what order. Codex saves the selection to `tui.terminal_title` in `~/.codex/config.toml`. You can also edit it directly:

```toml
[tui]
terminal_title = ["activity", "project-name", "run-state", "thread-title"]
```

Keep `activity` in the title selection if you want Zed to distinguish active work from approval prompts. Codex's default title selection already includes it.

## Credentials and Remote Projects {#credentials-and-remote-projects}

Credentials come from the terminal session and the CLI/TUI running inside it.

In remote projects, the CLI may read the remote shell environment and remote config files. In local Terminal Threads, it reads the local shell environment and local config files. Zed does not copy API keys from LLM provider settings into Terminal Threads.

## When to Use Terminal Threads {#when-to-use-terminal-threads}

Use Terminal Threads when:

- you want the tool's native CLI/TUI experience
- no ACP integration exists
- you want subscription behavior owned by the CLI
- you want the CLI to use its own native config files

For ACP-integrated agents, see [External Agents](./external-agents.md).
