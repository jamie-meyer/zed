#![cfg_attr(any(test, feature = "test-support"), allow(dead_code))]

use std::{
    collections::HashSet,
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::OnceLock,
};

#[cfg(not(any(test, feature = "test-support")))]
use std::{collections::HashMap, time::Duration};

use anyhow::{Context as _, Result, bail};
use async_process::Command;
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use task::Shell;
use ui::AgentThreadStatus;
#[cfg(not(any(test, feature = "test-support")))]
use util::ResultExt as _;

use crate::terminal_thread_metadata_store::{
    TERMINAL_THREAD_TMUX_SERVER_NAME, TerminalThreadKind, TerminalThreadProvider,
    terminal_thread_tmux_session_name,
};
#[cfg(not(any(test, feature = "test-support")))]
use crate::terminal_thread_metadata_store::{
    TerminalThreadMetadataStore, TerminalThreadStatusStore, terminal_title_for_persistence,
};
use crate::{
    TerminalId,
    agent_panel::{codex_status_from_title, terminal_status_from_process_and_title},
};

#[cfg(not(any(test, feature = "test-support")))]
const TMUX_POLL_INTERVAL: Duration = Duration::from_secs(1);
static TMUX_PROGRAM: OnceLock<PathBuf> = OnceLock::new();

pub(crate) struct TmuxSessionManager {
    _poll_task: Task<()>,
}

struct GlobalTmuxSessionManager {
    _manager: Entity<TmuxSessionManager>,
}

impl Global for GlobalTmuxSessionManager {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionSnapshot {
    terminal_id: TerminalId,
    pane_id: String,
    command: String,
    title: String,
    working_directory: Option<String>,
    dead: bool,
    activity: Option<u64>,
    screen: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PaneActivityState {
    activity: Option<u64>,
    needs_follow_up_capture: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TmuxSessionRecoveryMetadata {
    pub worktree_path: Option<PathBuf>,
    pub kind: TerminalThreadKind,
    pub provider: Option<TerminalThreadProvider>,
}

impl TmuxSessionManager {
    pub(crate) fn init_global(cx: &mut App) {
        if cx.has_global::<GlobalTmuxSessionManager>() {
            return;
        }

        let manager = cx.new(Self::new);
        cx.set_global(GlobalTmuxSessionManager { _manager: manager });
    }

    fn new(cx: &mut Context<Self>) -> Self {
        #[cfg(any(test, feature = "test-support"))]
        let poll_task = {
            let _ = cx;
            Task::ready(())
        };

        #[cfg(not(any(test, feature = "test-support")))]
        let poll_task = cx.spawn(async move |this, cx| {
            let mut activity_by_terminal_id = HashMap::new();
            let mut last_poll_error = None;
            loop {
                let terminal_state = cx.update(|cx| {
                    let metadata_store = TerminalThreadMetadataStore::try_global(cx)?;
                    let status_store = TerminalThreadStatusStore::global(cx);
                    let metadata_store = metadata_store.read(cx);
                    metadata_store.entries().next()?;
                    let status_store = status_store.read(cx);
                    let codex_terminal_ids = metadata_store
                        .entries()
                        .filter(|entry| metadata_uses_codex(entry))
                        .map(|entry| entry.terminal_id)
                        .collect::<HashSet<_>>();
                    let running_terminal_ids = codex_terminal_ids
                        .iter()
                        .copied()
                        .filter(|terminal_id| {
                            status_store.status(*terminal_id) == AgentThreadStatus::Running
                        })
                        .collect::<HashSet<_>>();
                    Some((codex_terminal_ids, running_terminal_ids))
                });
                if let Some((codex_terminal_ids, running_terminal_ids)) = terminal_state {
                    let snapshots = list_sessions(
                        &codex_terminal_ids,
                        &running_terminal_ids,
                        &mut activity_by_terminal_id,
                    )
                    .await;
                    this.update(cx, |_, cx| match snapshots {
                        Ok(snapshots) => {
                            last_poll_error = None;
                            apply_snapshots(snapshots, cx);
                        }
                        Err(error) => {
                            let error = format!("{error:#}");
                            if last_poll_error.as_deref() != Some(error.as_str()) {
                                log::warn!("failed to poll tmux terminal sessions: {error}");
                                last_poll_error = Some(error);
                            }
                        }
                    })
                    .log_err();
                }
                cx.background_executor().timer(TMUX_POLL_INTERVAL).await;
            }
        });

        Self {
            _poll_task: poll_task,
        }
    }
}

pub(crate) fn attach_shell(terminal_id: TerminalId) -> Shell {
    Shell::WithArguments {
        program: TMUX_PROGRAM
            .get()
            .map(|program| program.to_string_lossy().into_owned())
            .unwrap_or_else(|| "tmux".to_string()),
        args: vec![
            "-u".to_string(),
            "-L".to_string(),
            TERMINAL_THREAD_TMUX_SERVER_NAME.to_string(),
            "attach-session".to_string(),
            "-t".to_string(),
            terminal_thread_tmux_session_name(terminal_id),
        ],
        title_override: Some("Codex".to_string()),
    }
}

pub(crate) async fn ensure_session(
    terminal_id: TerminalId,
    working_directory: Option<&Path>,
    recovery_metadata: &TmuxSessionRecoveryMetadata,
) -> Result<bool> {
    let name = terminal_thread_tmux_session_name(terminal_id);
    if has_session(&name).await? {
        configure_session(&name, terminal_id, recovery_metadata).await?;
        return Ok(false);
    }

    let mut command = tmux_command().await?;
    command.args(["new-session", "-d", "-s", &name]);
    if let Some(working_directory) = working_directory {
        command.arg("-c").arg(working_directory);
    }

    let output = command
        .output()
        .await
        .with_context(|| "failed to start tmux; install tmux and ensure it is available in PATH")?;
    if !output.status.success() {
        if has_session(&name).await.unwrap_or(false) {
            return Ok(false);
        }
        bail!(
            "failed to create tmux session {name}: {}",
            command_error(&output)
        );
    }

    if let Err(error) = configure_session(&name, terminal_id, recovery_metadata).await {
        if let Err(cleanup_error) = kill_session(terminal_id).await {
            log::warn!(
                "failed to clean up tmux session {name} after configuration error: {cleanup_error:#}"
            );
        }
        return Err(error);
    }

    Ok(true)
}

async fn configure_session(
    name: &str,
    terminal_id: TerminalId,
    recovery_metadata: &TmuxSessionRecoveryMetadata,
) -> Result<()> {
    set_server_option("extended-keys", "always").await?;
    set_server_option("extended-keys-format", "csi-u").await?;
    ensure_server_option_contains("terminal-features", "xterm*:extkeys").await?;
    for (option, value) in recovery_session_options(terminal_id, recovery_metadata) {
        set_session_option(name, option, &value).await?;
    }
    set_session_option(name, "mouse", "on").await?;
    set_session_option(name, "remain-on-exit", "on").await?;
    set_session_option(name, "status-right", "%Y-%m-%d  %H:%M ").await?;
    Ok(())
}

fn recovery_session_options(
    terminal_id: TerminalId,
    recovery_metadata: &TmuxSessionRecoveryMetadata,
) -> Vec<(&'static str, String)> {
    vec![
        ("@zed-managed", "1".to_string()),
        ("@zed-terminal-id", terminal_id.to_key_string()),
        (
            "@zed-worktree-path",
            recovery_metadata
                .worktree_path
                .as_deref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
        (
            "@zed-terminal-kind",
            recovery_metadata.kind.as_str().to_string(),
        ),
        (
            "@zed-provider",
            recovery_metadata
                .provider
                .map(TerminalThreadProvider::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
    ]
}

async fn set_server_option(option: &str, value: &str) -> Result<()> {
    let output = tmux_command()
        .await?
        .args(["set-option", "-s", option, value])
        .output()
        .await
        .with_context(|| format!("failed to configure tmux server option {option}"))?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "failed to configure tmux server option {option}: {}",
            command_error(&output)
        )
    }
}

async fn ensure_server_option_contains(option: &str, value: &str) -> Result<()> {
    let output = tmux_command()
        .await?
        .args(["show-options", "-sv", option])
        .output()
        .await
        .with_context(|| format!("failed to query tmux server option {option}"))?;
    if !output.status.success() {
        bail!(
            "failed to query tmux server option {option}: {}",
            command_error(&output)
        );
    }
    if server_option_contains(&String::from_utf8_lossy(&output.stdout), value) {
        return Ok(());
    }

    let value = format!(",{value}");
    let output = tmux_command()
        .await?
        .args(["set-option", "-sa", option, &value])
        .output()
        .await
        .with_context(|| format!("failed to extend tmux server option {option}"))?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "failed to extend tmux server option {option}: {}",
            command_error(&output)
        )
    }
}

fn server_option_contains(current: &str, value: &str) -> bool {
    current
        .split([',', '\n'])
        .any(|entry| entry.trim() == value)
}

pub(crate) async fn kill_session(terminal_id: TerminalId) -> Result<()> {
    let name = terminal_thread_tmux_session_name(terminal_id);
    let output = tmux_command()
        .await?
        .args(["kill-session", "-t", &name])
        .output()
        .await
        .with_context(|| "failed to run tmux while closing terminal session")?;

    if output.status.success() || is_missing_session_output(&output) {
        Ok(())
    } else {
        bail!(
            "failed to close tmux session {name}: {}",
            command_error(&output)
        )
    }
}

async fn has_session(name: &str) -> Result<bool> {
    let output = tmux_command()
        .await?
        .args(["has-session", "-t", name])
        .output()
        .await
        .with_context(|| "failed to run tmux; install tmux and ensure it is available in PATH")?;
    Ok(output.status.success())
}

async fn set_session_option(name: &str, option: &str, value: &str) -> Result<()> {
    let output = tmux_command()
        .await?
        .args(["set-option", "-t", name, option, value])
        .output()
        .await
        .with_context(|| format!("failed to configure tmux option {option}"))?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "failed to configure tmux session {name}: {}",
            command_error(&output)
        )
    }
}

#[cfg(not(any(test, feature = "test-support")))]
async fn list_sessions(
    codex_terminal_ids: &HashSet<TerminalId>,
    running_terminal_ids: &HashSet<TerminalId>,
    activity_by_terminal_id: &mut HashMap<TerminalId, PaneActivityState>,
) -> Result<Vec<SessionSnapshot>> {
    let format = "#{@zed-terminal-id}\t#{pane_id}\t#{pane_current_command}\t#{pane_title}\t#{pane_current_path}\t#{pane_dead}\t#{window_activity}";
    let output = tmux_command()
        .await?
        .args([
            "list-panes",
            "-a",
            "-f",
            "#{&&:#{window_active},#{pane_active}}",
            "-F",
            format,
        ])
        .output()
        .await
        .with_context(|| "failed to query tmux terminal sessions")?;
    if !output.status.success() {
        if is_missing_session_output(&output) {
            return Ok(Vec::new());
        }
        bail!("failed to query tmux sessions: {}", command_error(&output));
    }

    let mut snapshots = parse_snapshots(&String::from_utf8_lossy(&output.stdout));
    let current_terminal_ids = snapshots
        .iter()
        .map(|snapshot| snapshot.terminal_id)
        .collect::<HashSet<_>>();
    activity_by_terminal_id.retain(|terminal_id, _| current_terminal_ids.contains(terminal_id));

    for snapshot in &mut snapshots {
        let previous_activity = activity_by_terminal_id.get(&snapshot.terminal_id).copied();
        let activity_changed =
            previous_activity.is_none_or(|previous| previous.activity != snapshot.activity);
        if !should_capture_codex_screen(
            snapshot,
            codex_terminal_ids,
            running_terminal_ids,
            previous_activity,
        ) {
            activity_by_terminal_id.insert(
                snapshot.terminal_id,
                PaneActivityState {
                    activity: snapshot.activity,
                    needs_follow_up_capture: false,
                },
            );
            continue;
        }

        match capture_pane(&snapshot.pane_id).await {
            Ok(screen) => {
                snapshot.screen = Some(screen);
                activity_by_terminal_id.insert(
                    snapshot.terminal_id,
                    PaneActivityState {
                        activity: snapshot.activity,
                        needs_follow_up_capture: activity_changed,
                    },
                );
            }
            Err(error) => {
                log::debug!(
                    "failed to capture tmux pane {} for status detection: {error:#}",
                    snapshot.pane_id
                );
            }
        }
    }
    Ok(snapshots)
}

fn should_capture_codex_screen(
    snapshot: &SessionSnapshot,
    codex_terminal_ids: &HashSet<TerminalId>,
    running_terminal_ids: &HashSet<TerminalId>,
    previous_activity: Option<PaneActivityState>,
) -> bool {
    !snapshot.dead
        && (codex_terminal_ids.contains(&snapshot.terminal_id)
            || is_codex_command(&snapshot.command))
        && codex_status_from_title(&snapshot.title) == AgentThreadStatus::Completed
        && (running_terminal_ids.contains(&snapshot.terminal_id)
            || previous_activity.is_none_or(|previous| {
                previous.activity != snapshot.activity || previous.needs_follow_up_capture
            }))
}

fn parse_snapshots(output: &str) -> Vec<SessionSnapshot> {
    let mut snapshots = Vec::new();
    for line in output.lines() {
        let mut fields = line.splitn(7, '\t');
        let Some(terminal_id) = fields.next().filter(|field| !field.is_empty()) else {
            continue;
        };
        let Some(pane_id) = fields.next().filter(|field| !field.is_empty()) else {
            continue;
        };
        let Some(command) = fields.next() else {
            continue;
        };
        let Some(title) = fields.next() else {
            continue;
        };
        let Some(working_directory) = fields.next() else {
            continue;
        };
        let Some(dead) = fields.next() else {
            continue;
        };
        let Some(activity) = fields.next() else {
            continue;
        };
        let Ok(terminal_id) = TerminalId::from_key_string(terminal_id) else {
            continue;
        };

        snapshots.push(SessionSnapshot {
            terminal_id,
            pane_id: pane_id.to_string(),
            command: command.to_string(),
            title: title.to_string(),
            working_directory: (!working_directory.is_empty())
                .then(|| working_directory.to_string()),
            dead: dead == "1",
            activity: activity.parse().ok(),
            screen: None,
        });
    }
    snapshots
}

#[cfg(not(any(test, feature = "test-support")))]
async fn capture_pane(pane_id: &str) -> Result<String> {
    let output = tmux_command()
        .await?
        .args(["capture-pane", "-p", "-t", pane_id])
        .output()
        .await
        .with_context(|| format!("failed to capture tmux pane {pane_id}"))?;
    if !output.status.success() {
        bail!(
            "failed to capture tmux pane {pane_id}: {}",
            command_error(&output)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(not(any(test, feature = "test-support")))]
fn apply_snapshots(snapshots: Vec<SessionSnapshot>, cx: &mut App) {
    let snapshots_by_id: HashMap<_, _> = snapshots
        .into_iter()
        .map(|snapshot| (snapshot.terminal_id, snapshot))
        .collect();

    let metadata_store = TerminalThreadMetadataStore::try_global(cx);
    let metadata = metadata_store
        .as_ref()
        .map(|store| store.read(cx).entries().cloned().collect::<Vec<_>>())
        .unwrap_or_default();

    let status_store = TerminalThreadStatusStore::global(cx);
    for entry in metadata {
        let snapshot = snapshots_by_id.get(&entry.terminal_id);
        let is_codex_session = metadata_uses_codex(&entry)
            || snapshot.is_some_and(|snapshot| is_codex_command(&snapshot.command));
        let previous_status = status_store.read(cx).status(entry.terminal_id);
        let status = status_for_snapshot(snapshot, is_codex_session, previous_status);
        if status != previous_status {
            log::debug!(
                "tmux terminal {} status changed from {:?} to {:?}, title={:?}",
                entry.terminal_id.to_key_string(),
                previous_status,
                status,
                snapshot.map(|snapshot| snapshot.title.as_str())
            );
        }

        status_store.update(cx, |store, cx| {
            store.set_status(entry.terminal_id, status, cx);
        });

        let Some(snapshot) = snapshot else {
            continue;
        };
        let title = terminal_title_for_persistence(&snapshot.title);
        let working_directory = snapshot
            .working_directory
            .as_ref()
            .map(std::path::PathBuf::from);
        if entry.title == title && entry.working_directory == working_directory {
            continue;
        }

        if let Some(metadata_store) = metadata_store.as_ref() {
            let mut updated = entry;
            updated.title = title;
            updated.working_directory = working_directory;
            metadata_store.update(cx, |store, cx| store.save(updated, cx));
        }
    }
}

fn status_for_snapshot(
    snapshot: Option<&SessionSnapshot>,
    is_codex_session: bool,
    previous_status: AgentThreadStatus,
) -> AgentThreadStatus {
    let Some(snapshot) = snapshot else {
        return AgentThreadStatus::Completed;
    };
    if snapshot.dead {
        return AgentThreadStatus::Error;
    }

    if is_codex_session {
        let title_status = codex_status_from_title(&snapshot.title);
        if title_status != AgentThreadStatus::Completed {
            return title_status;
        }

        return snapshot
            .screen
            .as_deref()
            .and_then(codex_status_from_screen)
            .unwrap_or(previous_status);
    }

    let title_status = terminal_status_from_process_and_title(
        Some(snapshot.command.as_str()),
        snapshot.title.as_str(),
    )
    .unwrap_or(AgentThreadStatus::Completed);
    title_status
}

fn is_codex_command(command: &str) -> bool {
    command == "codex" || command.starts_with("codex-")
}

#[cfg(not(any(test, feature = "test-support")))]
fn metadata_uses_codex(
    metadata: &crate::terminal_thread_metadata_store::TerminalThreadMetadata,
) -> bool {
    metadata.registry.provider == Some(TerminalThreadProvider::Codex)
}

fn codex_status_from_screen(screen: &str) -> Option<AgentThreadStatus> {
    const APPROVAL_MARKERS: [&str; 6] = [
        "Would you like to run the following command?",
        "Would you like to grant these permissions?",
        "Would you like to make the following edits?",
        "Yes, proceed",
        "No, and tell Codex what to do differently",
        "enter to submit answer",
    ];

    if APPROVAL_MARKERS
        .iter()
        .any(|marker| screen.contains(marker))
        || screen.contains("ctrl + j to submit answer")
        || screen.contains("enter to submit all")
    {
        return Some(AgentThreadStatus::WaitingForConfirmation);
    }

    if screen.contains("to interrupt") {
        return Some(AgentThreadStatus::Running);
    }

    let has_idle_composer = screen
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(4)
        .any(|line| line.trim_start().starts_with('›'));
    if has_idle_composer {
        return Some(AgentThreadStatus::Completed);
    }

    (screen.contains(">_ OpenAI Codex") || screen.contains("Starting Codex"))
        .then_some(AgentThreadStatus::Running)
}

async fn tmux_command() -> Result<Command> {
    let mut command = Command::new(tmux_program().await?);
    command.args(["-u", "-L", TERMINAL_THREAD_TMUX_SERVER_NAME]);
    Ok(command)
}

async fn tmux_program() -> Result<PathBuf> {
    if let Some(program) = TMUX_PROGRAM.get() {
        return Ok(program.clone());
    }

    let (program, source) = match which::which("tmux") {
        Ok(program) => (program, "inherited PATH"),
        Err(inherited_path_error) => {
            let shell = util::get_system_shell();
            let environment = util::shell_env::capture(&shell, &[], paths::home_dir())
                .await
                .with_context(|| {
                    format!(
                        "failed to load the login-shell environment while locating tmux after inherited PATH lookup failed: {inherited_path_error}"
                    )
                })?;
            let search_path = environment
                .get("PATH")
                .context("login-shell environment did not contain PATH while locating tmux")?;
            find_tmux_in_path(search_path.as_ref()).with_context(|| {
                format!(
                    "tmux was not found in the inherited PATH or login-shell PATH; install tmux and ensure it is available to {shell}"
                )
            })
            .map(|program| (program, "login-shell PATH"))?
        }
    };

    let program = TMUX_PROGRAM.get_or_init(|| program);
    log::info!(
        "resolved tmux executable from {source}: {}",
        program.display()
    );
    Ok(program.clone())
}

fn find_tmux_in_path(search_path: &OsStr) -> Option<PathBuf> {
    which::which_in("tmux", Some(search_path), paths::home_dir()).ok()
}

fn is_missing_session_output(output: &std::process::Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr.contains("no server running")
        || stderr.contains("error connecting")
        || stderr.contains("can't find session")
        || stderr.contains("no sessions")
}

fn command_error(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let message = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if message.is_empty() {
        format!("tmux exited with {}", output.status)
    } else {
        message.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn finds_tmux_in_explicit_path() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("failed to create temporary directory");
        let executable = directory.path().join("tmux");
        std::fs::write(&executable, "#!/bin/sh\n").expect("failed to create fake tmux executable");
        let mut permissions = std::fs::metadata(&executable)
            .expect("failed to read fake tmux metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions)
            .expect("failed to make fake tmux executable");

        assert_eq!(
            find_tmux_in_path(directory.path().as_os_str()),
            Some(executable)
        );
    }

    #[test]
    fn attach_forces_utf8_for_gui_launches_without_a_locale() {
        let terminal_id = TerminalId::new();
        let Shell::WithArguments { args, .. } = attach_shell(terminal_id) else {
            panic!("tmux attach should use a shell with arguments");
        };

        assert_eq!(
            args,
            [
                "-u",
                "-L",
                TERMINAL_THREAD_TMUX_SERVER_NAME,
                "attach-session",
                "-t",
                terminal_thread_tmux_session_name(terminal_id).as_str(),
            ]
        );
    }

    #[test]
    fn recovery_options_include_worktree_and_agent_identity() {
        let terminal_id = TerminalId::new();
        let options = recovery_session_options(
            terminal_id,
            &TmuxSessionRecoveryMetadata {
                worktree_path: Some(PathBuf::from("/project-feature")),
                kind: TerminalThreadKind::Agent,
                provider: Some(TerminalThreadProvider::Codex),
            },
        );

        assert_eq!(
            options,
            vec![
                ("@zed-managed", "1".to_string()),
                ("@zed-terminal-id", terminal_id.to_key_string()),
                ("@zed-worktree-path", "/project-feature".to_string()),
                ("@zed-terminal-kind", "agent".to_string()),
                ("@zed-provider", "codex".to_string()),
            ]
        );
    }

    #[test]
    fn server_option_list_values_are_added_idempotently() {
        assert!(server_option_contains(
            "xterm*:RGB,xterm*:extkeys",
            "xterm*:extkeys"
        ));
        assert!(server_option_contains(
            "xterm*:RGB, xterm*:extkeys\n",
            "xterm*:extkeys"
        ));
        assert!(server_option_contains(
            "xterm*:RGB\nxterm*:extkeys\n",
            "xterm*:extkeys"
        ));
        assert!(!server_option_contains("xterm*:RGB", "xterm*:extkeys"));
    }

    #[test]
    fn parses_managed_tmux_panes_and_skips_unmanaged_panes() {
        let terminal_id = TerminalId::new();
        let output = format!(
            "\t%1\tvim\tuser pane\t/tmp\t0\t1\nnot-a-uuid\t%2\tfish\tstale\t/tmp\t0\t2\n{}\t%3\tcodex\t[ ! ] Action Required | zed\t/project\t0\t3\n",
            terminal_id.to_key_string()
        );

        let snapshots = parse_snapshots(&output);
        assert_eq!(
            snapshots,
            vec![SessionSnapshot {
                terminal_id,
                pane_id: "%3".to_string(),
                command: "codex".to_string(),
                title: "[ ! ] Action Required | zed".to_string(),
                working_directory: Some("/project".to_string()),
                dead: false,
                activity: Some(3),
                screen: None,
            }]
        );
    }

    #[test]
    fn live_non_agent_pane_is_not_an_error() {
        let terminal_id = TerminalId::new();
        let snapshot = SessionSnapshot {
            terminal_id,
            pane_id: "%1".to_string(),
            command: "fish".to_string(),
            title: "project".to_string(),
            working_directory: Some("/project".to_string()),
            dead: false,
            activity: Some(1),
            screen: None,
        };

        assert_eq!(
            status_for_snapshot(Some(&snapshot), false, AgentThreadStatus::Completed),
            AgentThreadStatus::Completed
        );
    }

    #[test]
    fn missing_pane_is_idle_and_dead_pane_is_an_error() {
        assert_eq!(
            status_for_snapshot(None, true, AgentThreadStatus::Running),
            AgentThreadStatus::Completed
        );

        let mut snapshot = SessionSnapshot {
            terminal_id: TerminalId::new(),
            pane_id: "%1".to_string(),
            command: "codex-aarch64-a".to_string(),
            title: "project".to_string(),
            working_directory: Some("/project".to_string()),
            dead: true,
            activity: Some(1),
            screen: None,
        };
        assert_eq!(
            status_for_snapshot(Some(&snapshot), true, AgentThreadStatus::Completed),
            AgentThreadStatus::Error
        );

        snapshot.dead = false;
        assert_eq!(
            status_for_snapshot(Some(&snapshot), true, AgentThreadStatus::Completed),
            AgentThreadStatus::Completed
        );
    }

    #[test]
    fn detects_observed_codex_titles_when_tmux_reports_a_shell() {
        let terminal_id = TerminalId::new();
        let snapshot = |title: &str| SessionSnapshot {
            terminal_id,
            pane_id: "%1".to_string(),
            command: "fish".to_string(),
            title: title.to_string(),
            working_directory: Some("/project".to_string()),
            dead: false,
            activity: Some(1),
            screen: None,
        };

        assert_eq!(
            status_for_snapshot(Some(&snapshot("⠸ zed")), true, AgentThreadStatus::Completed),
            AgentThreadStatus::Running
        );
        assert_eq!(
            status_for_snapshot(
                Some(&snapshot("[ ! ] Action Required | zed")),
                true,
                AgentThreadStatus::Completed
            ),
            AgentThreadStatus::WaitingForConfirmation
        );
        assert_eq!(
            status_for_snapshot(Some(&snapshot("zed")), true, AgentThreadStatus::Completed),
            AgentThreadStatus::Completed
        );
    }

    #[test]
    fn animations_off_uses_activity_gated_screen_status() {
        let terminal_id = TerminalId::new();
        let mut snapshot = SessionSnapshot {
            terminal_id,
            pane_id: "%1".to_string(),
            command: "codex-aarch64-a".to_string(),
            title: "zed".to_string(),
            working_directory: Some("/project".to_string()),
            dead: false,
            activity: Some(10),
            screen: None,
        };
        let codex_terminal_ids = HashSet::from_iter([terminal_id]);
        let no_running_terminal_ids = HashSet::new();

        assert!(should_capture_codex_screen(
            &snapshot,
            &codex_terminal_ids,
            &no_running_terminal_ids,
            None
        ));
        assert!(should_capture_codex_screen(
            &snapshot,
            &codex_terminal_ids,
            &no_running_terminal_ids,
            Some(PaneActivityState {
                activity: Some(10),
                needs_follow_up_capture: true,
            })
        ));
        assert!(!should_capture_codex_screen(
            &snapshot,
            &codex_terminal_ids,
            &no_running_terminal_ids,
            Some(PaneActivityState {
                activity: Some(10),
                needs_follow_up_capture: false,
            })
        ));
        assert!(should_capture_codex_screen(
            &snapshot,
            &codex_terminal_ids,
            &HashSet::from_iter([terminal_id]),
            Some(PaneActivityState {
                activity: Some(10),
                needs_follow_up_capture: false,
            })
        ));

        snapshot.screen = Some(
            "› Think for at least 8 seconds without tools.\n\
             \n\
             Working (0s • esc to interrupt)\n"
                .to_string(),
        );
        assert_eq!(
            status_for_snapshot(Some(&snapshot), true, AgentThreadStatus::Completed),
            AgentThreadStatus::Running
        );

        snapshot.screen = None;
        assert_eq!(
            status_for_snapshot(Some(&snapshot), true, AgentThreadStatus::Running),
            AgentThreadStatus::Running,
            "an unchanged pane must retain its previous status without another capture"
        );

        snapshot.activity = Some(16);
        snapshot.screen = Some(
            "• NO_ANIMATION_DONE\n\
             \n\
             › Use /skills to list available skills\n\
             \n\
               gpt-5.6-sol max · ~/project\n"
                .to_string(),
        );
        assert_eq!(
            status_for_snapshot(Some(&snapshot), true, AgentThreadStatus::Running),
            AgentThreadStatus::Completed
        );
    }

    #[test]
    fn animations_off_detects_approval_from_screen() {
        let snapshot = SessionSnapshot {
            terminal_id: TerminalId::new(),
            pane_id: "%1".to_string(),
            command: "codex-aarch64-a".to_string(),
            title: "zed".to_string(),
            working_directory: Some("/project".to_string()),
            dead: false,
            activity: Some(10),
            screen: Some(
                "Would you like to run the following command?\n\
                 \n\
                 › 1. Yes, proceed (y)\n\
                   3. No, and tell Codex what to do differently (esc)\n"
                    .to_string(),
            ),
        };

        assert_eq!(
            status_for_snapshot(Some(&snapshot), true, AgentThreadStatus::Running),
            AgentThreadStatus::WaitingForConfirmation
        );
    }

    #[test]
    fn shell_after_codex_exit_is_completed() {
        let snapshot = SessionSnapshot {
            terminal_id: TerminalId::new(),
            pane_id: "%1".to_string(),
            command: "fish".to_string(),
            title: "~/project".to_string(),
            working_directory: Some("/project".to_string()),
            dead: false,
            activity: Some(1),
            screen: Some("z@host ~/project>\n".to_string()),
        };

        assert_eq!(
            status_for_snapshot(Some(&snapshot), true, AgentThreadStatus::Completed),
            AgentThreadStatus::Completed
        );
    }
}
