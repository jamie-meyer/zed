use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use askpass::AskPassDelegate;
use collections::HashSet;
use fs::{CopyOptions, Fs};
use futures::{FutureExt as _, StreamExt as _, select_biased, stream::FuturesUnordered};
use gpui::{
    AsyncWindowContext, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Task, TaskExt, WeakEntity,
};
use project::git_store::Repository;
use project::project_settings::ProjectSettings;
use project::trusted_worktrees::{PathTrust, TrustedWorktrees};
use project::{Project, WorktreeId};
use remote::RemoteConnectionOptions;
use settings::Settings;
use task::TaskHook;
use ui::prelude::*;
use workspace::{
    ActiveWorktreeCreationPhase, MultiWorkspace, OpenMode, PreviousWorkspaceState, ToastView,
    Workspace, dock::DockPosition,
};
use zed_actions::NewWorktreeBranchTarget;

use git::repository::{FetchOptions, Remote};
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use util::ResultExt as _;

use crate::askpass_modal::AskPassModal;
use crate::git_panel::{open_output, show_error_toast};
use crate::worktree_names;

const WORKTREE_INCLUDE_FILE: &str = ".worktreeinclude";

pub struct RemoveWorktreeTaskContext {
    workspace: WeakEntity<Workspace>,
    task_source_worktree_id: WorktreeId,
    main_git_worktree: Option<PathBuf>,
}

impl RemoveWorktreeTaskContext {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        task_source_worktree_id: WorktreeId,
        main_git_worktree: Option<PathBuf>,
    ) -> Self {
        Self {
            workspace,
            task_source_worktree_id,
            main_git_worktree,
        }
    }
}

pub async fn run_remove_worktree_tasks(
    context: RemoveWorktreeTaskContext,
    worktree_path: PathBuf,
    cx: &mut AsyncWindowContext,
) -> anyhow::Result<()> {
    let Some(workspace) = context.workspace.upgrade() else {
        return Ok(());
    };
    let task = workspace.update_in(cx, |workspace, window, cx| {
        workspace.run_worktree_tasks_for_path(
            TaskHook::RemoveWorktree,
            context.task_source_worktree_id,
            worktree_path,
            context.main_git_worktree,
            window,
            cx,
        )
    })?;
    task.await
}

/// A remote-tracking branch reference parsed into its remote and branch parts,
/// e.g. `origin/main` -> remote `origin`, branch `main`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteBranchName {
    pub remote_name: String,
    pub branch_name: String,
}

impl RemoteBranchName {
    pub fn parse(name: &str) -> Option<Self> {
        let name = name.strip_prefix("refs/remotes/").unwrap_or(name);
        let (remote_name, branch_name) = name.split_once('/')?;
        if remote_name.is_empty() || branch_name.is_empty() {
            return None;
        }
        Some(Self {
            remote_name: remote_name.to_string(),
            branch_name: branch_name.to_string(),
        })
    }

    pub fn display_name(&self) -> String {
        format!("{}/{}", self.remote_name, self.branch_name)
    }
}

/// A "create new worktree" option offered to the user. The set of targets is
/// derived from repository state by [`worktree_create_targets`] so that the
/// worktree picker and the sidebar's new-thread menu stay in sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorktreeCreateTarget {
    CurrentBranch,
    DefaultBranch(RemoteBranchName),
}

impl WorktreeCreateTarget {
    pub fn branch_target(&self) -> NewWorktreeBranchTarget {
        match self {
            WorktreeCreateTarget::CurrentBranch => NewWorktreeBranchTarget::CurrentBranch,
            WorktreeCreateTarget::DefaultBranch(default_branch) => {
                NewWorktreeBranchTarget::RemoteBranch {
                    remote_name: default_branch.remote_name.clone(),
                    branch_name: default_branch.branch_name.clone(),
                }
            }
        }
    }

    pub fn branch_label(
        &self,
        has_multiple_repositories: bool,
        current_branch_name: Option<&str>,
    ) -> String {
        match self {
            WorktreeCreateTarget::DefaultBranch(default_branch) => default_branch.display_name(),
            WorktreeCreateTarget::CurrentBranch => {
                if has_multiple_repositories {
                    "current branches".to_string()
                } else {
                    current_branch_name.unwrap_or("HEAD").to_string()
                }
            }
        }
    }
}

/// Determines which "create new worktree" options to surface for the given
/// repository state: prefer the remote default branch when it differs from the
/// current branch, and otherwise offer the current branch.
pub fn worktree_create_targets(
    has_multiple_repositories: bool,
    default_branch: Option<RemoteBranchName>,
    current_branch_name: Option<&str>,
) -> Vec<WorktreeCreateTarget> {
    if has_multiple_repositories {
        return vec![WorktreeCreateTarget::CurrentBranch];
    }
    let Some(default_branch) = default_branch else {
        return vec![WorktreeCreateTarget::CurrentBranch];
    };
    let is_different =
        current_branch_name.is_none_or(|current| current != default_branch.branch_name);
    let mut targets = vec![WorktreeCreateTarget::DefaultBranch(default_branch)];
    if is_different {
        targets.push(WorktreeCreateTarget::CurrentBranch);
    }
    targets
}

const WORKTREE_CREATION_TIMEOUT: Duration = Duration::from_secs(120);
const WORKTREE_ROLLBACK_TIMEOUT: Duration = Duration::from_secs(30);
const WORKTREE_WORKSPACE_OPEN_TIMEOUT: Duration = Duration::from_secs(60);
const WORKTREE_INITIAL_SCAN_TIMEOUT: Duration = Duration::from_secs(30);
const WORKTREE_REPOSITORY_BARRIER_TIMEOUT: Duration = Duration::from_secs(15);

/// Whether a worktree operation is creating a new one or switching to an
/// existing one. Controls whether the source workspace's state (dock layout,
/// open files, agent panel draft) is inherited by the destination.
enum WorktreeOperation {
    Create,
    Switch,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RemoteBranchFetchMode {
    Fetch,
    UseLocal,
}

impl RemoteBranchFetchMode {
    fn should_fetch(self) -> bool {
        matches!(self, Self::Fetch)
    }
}

#[derive(Debug)]
struct WorktreeFetchError {
    remote_name: String,
    branch_name: String,
    source: anyhow::Error,
}

impl WorktreeFetchError {
    fn remote_branch_name(&self) -> String {
        format!("{}/{}", self.remote_name, self.branch_name)
    }

    fn output(&self) -> String {
        format!("git fetch {} failed:\n{:#}", self.remote_name, self.source)
    }
}

impl fmt::Display for WorktreeFetchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "git fetch {} failed while creating worktree from {}: {}",
            self.remote_name,
            self.remote_branch_name(),
            self.source
        )
    }
}

impl Error for WorktreeFetchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

struct WorktreeFetchFailedToast {
    workspace: WeakEntity<Workspace>,
    worktree_name: Option<String>,
    branch_target: NewWorktreeBranchTarget,
    focused_dock: Option<DockPosition>,
    remote_branch_name: String,
    operation: SharedString,
    output: String,
    focus_handle: FocusHandle,
}

impl WorktreeFetchFailedToast {
    fn new(
        workspace: WeakEntity<Workspace>,
        worktree_name: Option<String>,
        branch_target: NewWorktreeBranchTarget,
        focused_dock: Option<DockPosition>,
        fetch_error: &WorktreeFetchError,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        Self {
            workspace,
            worktree_name,
            branch_target,
            focused_dock,
            remote_branch_name: fetch_error.remote_branch_name(),
            operation: format!("fetch {}", fetch_error.remote_name).into(),
            output: fetch_error.output(),
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Focusable for WorktreeFetchFailedToast {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for WorktreeFetchFailedToast {}

impl ToastView for WorktreeFetchFailedToast {
    fn action(&self) -> Option<workspace::ToastAction> {
        None
    }

    fn auto_dismiss(&self) -> bool {
        false
    }
}

impl Render for WorktreeFetchFailedToast {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let workspace_for_retry = self.workspace.clone();
        let worktree_name = self.worktree_name.clone();
        let branch_target = self.branch_target.clone();
        let focused_dock = self.focused_dock;

        let workspace_for_log = self.workspace.clone();
        let operation = self.operation.clone();
        let output = self.output.clone();

        h_flex()
            .id("worktree-fetch-failed-toast")
            .elevation_3(cx)
            .gap_2()
            .py_1p5()
            .pl_2p5()
            .pr_1p5()
            .flex_none()
            .bg(cx.theme().colors().surface_background)
            .shadow_lg()
            .child(
                Icon::new(IconName::XCircle)
                    .size(IconSize::Small)
                    .color(Color::Error),
            )
            .child(Label::new(format!(
                "git fetch failed for {}",
                self.remote_branch_name
            )))
            .child(
                Button::new(
                    "use-local-worktree-base",
                    format!("Use local {}", self.remote_branch_name),
                )
                .color(Color::Muted)
                .on_click(cx.listener(move |_, _event, window, cx| {
                    cx.emit(DismissEvent);
                    if let Some(workspace) = workspace_for_retry.upgrade() {
                        workspace.update(cx, |workspace, cx| {
                            let task = create_worktree_workspace_inner(
                                workspace,
                                &zed_actions::CreateWorktree {
                                    worktree_name: worktree_name.clone(),
                                    branch_target: branch_target.clone(),
                                },
                                window,
                                focused_dock,
                                RemoteBranchFetchMode::UseLocal,
                                // User-initiated retry of a foreground create.
                                true,
                                cx,
                            );
                            task.detach_and_log_err(cx);
                        });
                    }
                })),
            )
            .child(
                Button::new("view-worktree-fetch-log", "Show Error Logs")
                    .color(Color::Muted)
                    .on_click(cx.listener(move |_, _event, window, cx| {
                        cx.emit(DismissEvent);
                        let output = output.clone();
                        let operation = operation.clone();
                        workspace_for_log
                            .update(cx, move |workspace, cx| {
                                open_output(operation, workspace, &output, window, cx)
                            })
                            .ok();
                    })),
            )
            .child(
                IconButton::new("dismiss-worktree-fetch-failed-toast", IconName::Close)
                    .shape(ui::IconButtonShape::Square)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .on_click(cx.listener(|_, _event, _window, cx| {
                        cx.emit(DismissEvent);
                    })),
            )
    }
}

/// Classifies the project's visible worktrees into git-managed repositories
/// and non-git paths. Each unique repository is returned only once.
pub fn classify_worktrees(
    project: &Project,
    cx: &gpui::App,
) -> (Vec<Entity<Repository>>, Vec<PathBuf>) {
    let repositories = project.repositories(cx).clone();
    let mut git_repos: Vec<Entity<Repository>> = Vec::new();
    let mut non_git_paths: Vec<PathBuf> = Vec::new();
    let mut seen_repo_ids = HashSet::default();

    for worktree in project.visible_worktrees(cx) {
        let wt_path = worktree.read(cx).abs_path();

        let matching_repo = repositories
            .iter()
            .filter_map(|(id, repo)| {
                let work_dir = repo.read(cx).work_directory_abs_path.clone();
                if wt_path.starts_with(work_dir.as_ref()) {
                    Some((*id, repo.clone(), work_dir.as_ref().components().count()))
                } else {
                    None
                }
            })
            .max_by(
                |(left_id, _left_repo, left_depth), (right_id, _right_repo, right_depth)| {
                    left_depth
                        .cmp(right_depth)
                        .then_with(|| left_id.cmp(right_id))
                },
            );

        if let Some((id, repo, _)) = matching_repo {
            if seen_repo_ids.insert(id) {
                git_repos.push(repo);
            }
        } else {
            non_git_paths.push(wt_path.to_path_buf());
        }
    }

    (git_repos, non_git_paths)
}

/// Resolves a branch target into the ref the new worktree should be based on.
/// Returns `None` for `CurrentBranch`, meaning "use the current HEAD".
pub fn resolve_worktree_branch_target(branch_target: &NewWorktreeBranchTarget) -> Option<String> {
    match branch_target {
        NewWorktreeBranchTarget::CurrentBranch => None,
        NewWorktreeBranchTarget::ExistingBranch { name } => Some(name.clone()),
        NewWorktreeBranchTarget::RemoteBranch {
            remote_name,
            branch_name,
        } => Some(format!("refs/remotes/{remote_name}/{branch_name}")),
    }
}

fn remote_branch_to_fetch(branch_target: &NewWorktreeBranchTarget) -> Option<(&str, &str)> {
    match branch_target {
        NewWorktreeBranchTarget::RemoteBranch {
            remote_name,
            branch_name,
        } => Some((remote_name, branch_name)),
        NewWorktreeBranchTarget::CurrentBranch | NewWorktreeBranchTarget::ExistingBranch { .. } => {
            None
        }
    }
}

fn create_worktree_askpass_delegate(
    workspace: WeakEntity<Workspace>,
    operation: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> AskPassDelegate {
    let operation = operation.into();
    let window = window.window_handle();
    AskPassDelegate::new(&mut cx.to_async(), move |prompt, tx, cx| {
        window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.toggle_modal(window, cx, |window, cx| {
                        AskPassModal::new(operation.clone(), prompt.into(), tx, window, cx)
                    });
                })
            })
            .ok();
    })
}

async fn fetch_remote_for_worktree_base(
    git_repos: &[Entity<Repository>],
    remote_name: String,
    askpass_delegates: Vec<AskPassDelegate>,
    cx: &mut AsyncWindowContext,
) -> anyhow::Result<()> {
    if askpass_delegates.len() != git_repos.len() {
        return Err(anyhow!(
            "Unable to fetch {remote_name}: missing credential prompt delegate"
        ));
    }

    let fetches = cx.update(|_, cx| {
        git_repos
            .iter()
            .cloned()
            .zip(askpass_delegates)
            .map(|(repo, askpass)| {
                repo.update(cx, |repo, cx| {
                    repo.fetch(
                        FetchOptions::Remote(Remote {
                            name: remote_name.clone().into(),
                        }),
                        askpass,
                        cx,
                    )
                })
            })
            .collect::<Vec<_>>()
    })?;

    for fetch in futures::future::join_all(fetches).await {
        fetch??;
    }

    Ok(())
}

/// Kicks off an async git-worktree creation for each repository. Returns:
///
/// - `creation_infos`: a vec of `(repo, new_path, receiver)` tuples.
/// - `path_remapping`: `(old_work_dir, new_worktree_path)` pairs for remapping editor tabs.
///
/// Multiple entries in `git_repos` can be linked worktrees of the *same*
/// underlying repository (e.g. a project that has both the main checkout and
/// one of its linked worktrees open as separate Zed worktrees). Those entries
/// resolve to the same target path via [`Repository::path_for_new_linked_worktree`],
/// so we create the new worktree only once and remap every contributing
/// work directory onto it. Without this dedup, the second `git worktree add`
/// fails with "already exists".
fn start_worktree_creations(
    git_repos: &[Entity<Repository>],
    worktree_name: Option<String>,
    existing_worktree_names: &[String],
    existing_worktree_paths: &HashSet<PathBuf>,
    base_ref: Option<String>,
    worktree_directory_setting: &str,
    rng: &mut impl rand::Rng,
    cx: &mut gpui::App,
) -> anyhow::Result<(
    Vec<(
        Entity<Repository>,
        PathBuf,
        futures::channel::oneshot::Receiver<anyhow::Result<()>>,
    )>,
    Vec<(PathBuf, PathBuf)>,
    String,
)> {
    let mut creation_infos = Vec::new();
    let mut path_remapping = Vec::new();
    let mut scheduled_paths: HashSet<PathBuf> = HashSet::default();

    let worktree_name = worktree_name.unwrap_or_else(|| {
        let existing_refs: Vec<&str> = existing_worktree_names.iter().map(|s| s.as_str()).collect();
        worktree_names::generate_worktree_name(&existing_refs, rng)
            .unwrap_or_else(|| "worktree".to_string())
    });

    for repo in git_repos {
        let (work_dir, new_path, receiver) = repo.update(cx, |repo, _cx| {
            let new_path =
                repo.path_for_new_linked_worktree(&worktree_name, worktree_directory_setting)?;
            if existing_worktree_paths.contains(&new_path) {
                anyhow::bail!("A worktree already exists at {}", new_path.display());
            }
            let work_dir = repo.work_directory_abs_path.clone();
            // Only the first repo that resolves to a given target path
            // actually creates the worktree; subsequent linked worktrees of
            // the same repository just contribute a path remapping.
            let receiver = if scheduled_paths.contains(&new_path) {
                None
            } else {
                let target = git::repository::CreateWorktreeTarget::Detached {
                    base_sha: base_ref.clone(),
                };
                Some(repo.create_worktree(target, new_path.clone()))
            };
            anyhow::Ok((work_dir, new_path, receiver))
        })?;
        path_remapping.push((work_dir.to_path_buf(), new_path.clone()));
        if let Some(receiver) = receiver {
            scheduled_paths.insert(new_path.clone());
            creation_infos.push((repo.clone(), new_path, receiver));
        }
    }

    Ok((creation_infos, path_remapping, worktree_name))
}

async fn copy_worktree_included_files(
    path_remapping: &[(PathBuf, PathBuf)],
    fs: Arc<dyn Fs>,
) -> anyhow::Result<()> {
    for (source_root, destination_root) in path_remapping {
        copy_worktree_included_files_for_root(source_root, destination_root, fs.clone()).await?;
    }
    Ok(())
}

async fn copy_worktree_included_files_for_root(
    source_root: &Path,
    destination_root: &Path,
    fs: Arc<dyn Fs>,
) -> anyhow::Result<()> {
    let include_path = source_root.join(WORKTREE_INCLUDE_FILE);
    if !fs.is_file(&include_path).await {
        return Ok(());
    }

    let contents = fs
        .load(&include_path)
        .await
        .with_context(|| format!("loading {}", include_path.display()))?;
    let matcher = build_worktree_include_matcher(source_root, &include_path, &contents)?;
    let scan_roots = worktree_include_scan_roots(&contents)?;
    let mut pending_paths = scan_roots
        .into_iter()
        .map(|path| source_root.join(path))
        .collect::<Vec<_>>();

    while let Some(source_path) = pending_paths.pop() {
        if source_path.starts_with(destination_root) {
            continue;
        }

        let Some(metadata) = fs
            .metadata(&source_path)
            .await
            .with_context(|| format!("reading metadata for {}", source_path.display()))?
        else {
            continue;
        };
        let relative_path = source_path.strip_prefix(source_root).with_context(|| {
            format!(
                "{} is outside worktree root {}",
                source_path.display(),
                source_root.display()
            )
        })?;
        if relative_path
            .components()
            .next()
            .is_some_and(|component| component.as_os_str() == ".git")
        {
            continue;
        }

        let is_included = matcher
            .matched_path_or_any_parents(&source_path, metadata.is_dir)
            .is_ignore();
        let destination_path = destination_root.join(relative_path);

        if metadata.is_symlink {
            if is_included && fs.metadata(&destination_path).await?.is_none() {
                ensure_worktree_include_parent(destination_root, relative_path, fs.as_ref())
                    .await?;
                let target = fs
                    .read_link(&source_path)
                    .await
                    .with_context(|| format!("reading symlink {}", source_path.display()))?;
                fs.create_symlink(&destination_path, target)
                    .await
                    .with_context(|| format!("copying symlink {}", source_path.display()))?;
            }
            continue;
        }

        if metadata.is_dir {
            if is_included && fs.metadata(&destination_path).await?.is_none() {
                ensure_worktree_include_parent(destination_root, relative_path, fs.as_ref())
                    .await?;
                fs.create_dir(&destination_path)
                    .await
                    .with_context(|| format!("creating {}", destination_path.display()))?;
            }

            let mut entries = fs
                .read_dir(&source_path)
                .await
                .with_context(|| format!("reading {}", source_path.display()))?;
            while let Some(entry) = entries.next().await {
                pending_paths
                    .push(entry.with_context(|| format!("reading {}", source_path.display()))?);
            }
            continue;
        }

        if !is_included || fs.metadata(&destination_path).await?.is_some() {
            continue;
        }
        if metadata.is_fifo {
            anyhow::bail!(
                "Cannot copy FIFO {} selected by {}",
                source_path.display(),
                include_path.display()
            );
        }

        ensure_worktree_include_parent(destination_root, relative_path, fs.as_ref()).await?;
        fs.copy_file(
            &source_path,
            &destination_path,
            CopyOptions {
                overwrite: false,
                ignore_if_exists: true,
            },
        )
        .await
        .with_context(|| {
            format!(
                "copying {} to {}",
                source_path.display(),
                destination_path.display()
            )
        })?;
    }

    Ok(())
}

fn build_worktree_include_matcher(
    source_root: &Path,
    include_path: &Path,
    contents: &str,
) -> anyhow::Result<Gitignore> {
    let mut builder = GitignoreBuilder::new(source_root);
    for line in contents.lines() {
        builder.add_line(Some(include_path.to_path_buf()), line)?;
    }
    Ok(builder.build()?)
}

fn worktree_include_scan_roots(contents: &str) -> anyhow::Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    for line in contents.lines() {
        let mut pattern = line.trim();
        if pattern.is_empty() || pattern.starts_with('#') || pattern.starts_with('!') {
            continue;
        }
        pattern = pattern
            .strip_prefix("\\#")
            .or_else(|| pattern.strip_prefix("\\!"))
            .unwrap_or(pattern)
            .trim_start_matches('/');
        if pattern.is_empty() {
            continue;
        }

        let components = pattern.split('/').filter(|component| !component.is_empty());
        if components.clone().any(|component| component == "..") {
            anyhow::bail!(".worktreeinclude patterns cannot leave the worktree root");
        }

        if !pattern.contains('/') || pattern.contains('\\') {
            roots.push(PathBuf::new());
            continue;
        }

        let mut root = PathBuf::new();
        for component in components {
            if component.contains(['*', '?', '[', ']']) {
                break;
            }
            root.push(component);
        }
        roots.push(root);
    }

    roots.sort_by_key(|path| path.components().count());
    let mut deduplicated_roots: Vec<PathBuf> = Vec::new();
    for root in roots {
        if deduplicated_roots
            .iter()
            .any(|existing| root.starts_with(existing))
        {
            continue;
        }
        deduplicated_roots.push(root);
    }
    Ok(deduplicated_roots)
}

async fn ensure_worktree_include_parent(
    destination_root: &Path,
    relative_path: &Path,
    fs: &dyn Fs,
) -> anyhow::Result<()> {
    let Some(relative_parent) = relative_path.parent() else {
        return Ok(());
    };
    let mut destination_parent = destination_root.to_path_buf();
    for component in relative_parent.components() {
        destination_parent.push(component);
        match fs.metadata(&destination_parent).await? {
            Some(metadata) if metadata.is_dir => {}
            Some(_) => anyhow::bail!(
                "Cannot create worktree setup directory because {} is not a directory",
                destination_parent.display()
            ),
            None => fs
                .create_dir(&destination_parent)
                .await
                .with_context(|| format!("creating {}", destination_parent.display()))?,
        }
    }
    Ok(())
}

/// Waits for every in-flight worktree creation to complete. If any
/// creation fails, all successfully-created worktrees are rolled back
/// (removed) so the project isn't left in a half-migrated state.
pub async fn await_and_rollback_on_failure(
    creation_infos: Vec<(
        Entity<Repository>,
        PathBuf,
        futures::channel::oneshot::Receiver<anyhow::Result<()>>,
    )>,
    fs: Arc<dyn Fs>,
    cx: &mut AsyncWindowContext,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut created_paths: Vec<PathBuf> = Vec::new();
    let repos_and_paths: Vec<(Entity<Repository>, PathBuf)> = creation_infos
        .iter()
        .map(|(repo, path, _)| (repo.clone(), path.clone()))
        .collect();
    let mut first_error: Option<anyhow::Error> = None;

    let mut pending_creations = FuturesUnordered::new();
    for (_repo, new_path, receiver) in creation_infos {
        pending_creations.push(async move {
            let result = match receiver.await {
                Ok(result) => result,
                Err(canceled) => Err(anyhow!("Worktree creation was canceled: {canceled}")),
            };
            (new_path, result)
        });
    }

    let mut creation_timed_out = false;
    if !pending_creations.is_empty() {
        let creation_timeout = cx
            .background_executor()
            .timer(WORKTREE_CREATION_TIMEOUT)
            .fuse();
        futures::pin_mut!(creation_timeout);

        loop {
            select_biased! {
                creation = pending_creations.next().fuse() => {
                    let Some((new_path, result)) = creation else {
                        break;
                    };
                    match result {
                        Ok(()) => created_paths.push(new_path),
                        Err(err) => {
                            if first_error.is_none() {
                                first_error = Some(err);
                            }
                        }
                    }
                }
                _ = creation_timeout => {
                    creation_timed_out = true;
                    let paths = repos_and_paths
                        .iter()
                        .map(|(_, path)| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    first_error.get_or_insert_with(|| {
                        anyhow!(
                            "Timed out after {} seconds while creating worktrees: {paths}",
                            WORKTREE_CREATION_TIMEOUT.as_secs()
                        )
                    });
                    break;
                }
            }
        }
    }

    if first_error.is_some() && !creation_timed_out {
        while let Some((new_path, result)) = pending_creations.next().await {
            match result {
                Ok(()) => created_paths.push(new_path),
                Err(err) => {
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            }
        }
    }

    let Some(err) = first_error else {
        return Ok(created_paths);
    };

    let rollback_failures = rollback_worktrees(&repos_and_paths, fs, cx).await;
    let mut error_message = format!("Failed to create worktree: {err}");
    if !rollback_failures.is_empty() {
        error_message.push_str("\n\nFailed to clean up: ");
        error_message.push_str(&rollback_failures.join(", "));
    }
    Err(anyhow!(error_message))
}

async fn rollback_worktrees(
    repos_and_paths: &[(Entity<Repository>, PathBuf)],
    fs: Arc<dyn Fs>,
    cx: &mut AsyncWindowContext,
) -> Vec<String> {
    let mut rollback_futures = Vec::new();
    for (rollback_repo, rollback_path) in repos_and_paths {
        let receiver = cx
            .update(|_, cx| {
                rollback_repo.update(cx, |repo, _cx| {
                    repo.remove_worktree(rollback_path.clone(), true)
                })
            })
            .ok();

        rollback_futures.push((rollback_path.clone(), receiver));
    }

    let mut rollback_failures = Vec::new();
    for (path, receiver_opt) in rollback_futures {
        let mut git_remove_failed = false;

        if let Some(receiver) = receiver_opt {
            let rollback = receiver.fuse();
            let timeout = cx
                .background_executor()
                .timer(WORKTREE_ROLLBACK_TIMEOUT)
                .fuse();
            futures::pin_mut!(rollback);
            futures::pin_mut!(timeout);
            select_biased! {
                result = rollback => {
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(rollback_err)) => {
                            log::error!(
                                "git worktree remove failed for {}: {rollback_err}",
                                path.display()
                            );
                            git_remove_failed = true;
                        }
                        Err(canceled) => {
                            log::error!(
                                "git worktree remove failed for {}: {canceled}",
                                path.display()
                            );
                            git_remove_failed = true;
                        }
                    }
                }
                _ = timeout => {
                    log::error!(
                        "git worktree remove timed out after {} seconds for {}",
                        WORKTREE_ROLLBACK_TIMEOUT.as_secs(),
                        path.display()
                    );
                    git_remove_failed = true;
                }
            }
        } else {
            log::error!(
                "failed to dispatch git worktree remove for {}",
                path.display()
            );
            git_remove_failed = true;
        }

        if git_remove_failed {
            if let Err(fs_err) = fs
                .remove_dir(
                    &path,
                    fs::RemoveOptions {
                        recursive: true,
                        ignore_if_not_exists: true,
                    },
                )
                .await
            {
                let msg = format!("{}: failed to remove directory: {fs_err}", path.display());
                log::error!("{}", msg);
                rollback_failures.push(msg);
            }
        }
    }
    rollback_failures
}

/// Propagates worktree trust from the source workspace to the new workspace.
/// If the source project's worktrees are all trusted, the new worktree paths
/// will also be trusted automatically.
fn maybe_propagate_worktree_trust(
    source_workspace: &WeakEntity<Workspace>,
    new_workspace: &Entity<Workspace>,
    paths: &[PathBuf],
    cx: &mut AsyncWindowContext,
) {
    cx.update(|_, cx| {
        if ProjectSettings::get_global(cx).session.trust_all_worktrees {
            return;
        }
        let source_is_trusted = source_workspace
            .upgrade()
            .map(|workspace| {
                let source_worktree_store = workspace.read(cx).project().read(cx).worktree_store();
                !TrustedWorktrees::has_restricted_worktrees(&source_worktree_store, cx)
            })
            .unwrap_or(false);

        if !source_is_trusted {
            return;
        }

        let worktree_store = new_workspace.read(cx).project().read(cx).worktree_store();
        let paths_to_trust: HashSet<_> = paths
            .iter()
            .filter_map(|path| {
                let (worktree, _) = worktree_store.read(cx).find_worktree(path, cx)?;
                Some(PathTrust::Worktree(worktree.read(cx).id()))
            })
            .collect();

        if !paths_to_trust.is_empty() {
            if let Some(trusted_store) = TrustedWorktrees::try_get_global(cx) {
                trusted_store.update(cx, |store, cx| {
                    store.trust(&worktree_store, paths_to_trust, cx);
                });
            }
        }
    })
    .ok();

    // After trust propagation, refresh the security modal on the new workspace
    // so it dismisses itself if there are no more restricted worktrees.
    cx.update(|window, cx| {
        new_workspace.update(cx, |workspace, cx| {
            workspace.show_worktree_trust_security_modal(false, window, cx);
        });
    })
    .ok();
}

/// Handles the `CreateWorktree` action generically, without any agent panel involvement.
/// Creates a new git worktree, opens the workspace, restores layout and files.
/// Errors are surfaced to the user via toasts; the new workspace handle is
/// discarded. Use [`create_worktree_workspace`] when you need the resulting
/// workspace (e.g., the `create_thread` agent tool spawns a thread in it).
pub fn handle_create_worktree(
    workspace: &mut Workspace,
    action: &zed_actions::CreateWorktree,
    window: &mut gpui::Window,
    fallback_focused_dock: Option<DockPosition>,
    cx: &mut gpui::Context<Workspace>,
) {
    let task = create_worktree_workspace_inner(
        workspace,
        action,
        window,
        fallback_focused_dock,
        RemoteBranchFetchMode::Fetch,
        // The user explicitly asked to create a worktree, so foreground it.
        true,
        cx,
    );
    task.detach_and_log_err(cx);
}

/// Outcome of [`create_worktree_workspace`].
pub struct CreatedWorktreeWorkspace {
    /// The newly opened workspace.
    pub workspace: Entity<Workspace>,
    /// True when the project contained more than one Zed worktree backed by
    /// the same underlying git repository, so they were consolidated into a
    /// single new worktree (they resolve to the same target path). Callers
    /// that care — like the `create_thread` agent tool — can use this to warn
    /// that the result may not reflect every source worktree's state.
    pub consolidated_worktrees: bool,
}

/// Same as [`handle_create_worktree`], but returns a `Task` that resolves to
/// the new workspace once worktree creation and post-open setup are
/// complete. The caller receives errors as `Result`s and is expected to
/// handle them. Note that a small set of early failures (no git repositories,
/// disconnected remote, mid-creation `git fetch` failure) still surface a
/// toast on the source workspace so the user understands why the action
/// didn't take effect; the same error is also returned to the caller.
///
/// Used by the `create_thread` agent tool to spawn a sibling thread inside
/// the newly-opened workspace.
///
/// The new workspace is opened in the **background** (added as a retained
/// tab without switching to it or moving focus), and it's a clean checkout
/// rather than inheriting the source workspace's open files and dock layout.
/// This mirrors how the agent's non-worktree threads are created in the
/// background rather than yanking the user away from what they're doing.
pub fn create_worktree_workspace(
    workspace: &mut Workspace,
    action: &zed_actions::CreateWorktree,
    window: &mut gpui::Window,
    fallback_focused_dock: Option<DockPosition>,
    cx: &mut gpui::Context<Workspace>,
) -> Task<anyhow::Result<CreatedWorktreeWorkspace>> {
    create_worktree_workspace_inner(
        workspace,
        action,
        window,
        fallback_focused_dock,
        RemoteBranchFetchMode::Fetch,
        // Agent-created worktree workspaces open in the background.
        false,
        cx,
    )
}

fn create_worktree_workspace_inner(
    workspace: &mut Workspace,
    action: &zed_actions::CreateWorktree,
    window: &mut gpui::Window,
    fallback_focused_dock: Option<DockPosition>,
    remote_branch_fetch_mode: RemoteBranchFetchMode,
    activate: bool,
    cx: &mut gpui::Context<Workspace>,
) -> Task<anyhow::Result<CreatedWorktreeWorkspace>> {
    let project = workspace.project().clone();

    if project.read(cx).repositories(cx).is_empty() {
        return Task::ready(Err(anyhow!(
            "create_worktree: no git repository in the project"
        )));
    }
    if project.read(cx).is_via_collab() {
        return Task::ready(Err(anyhow!(
            "create_worktree: not supported in collab projects"
        )));
    }

    // Guard against concurrent creation. We treat a concurrent creation as
    // a hard error here so the caller can surface it; the user-facing
    // wrapper [`handle_create_worktree`] swallows the error via
    // `detach_and_log_err`, matching the pre-existing silent return.
    if workspace.has_active_worktree_operation() {
        return Task::ready(Err(anyhow!("A worktree operation is already in progress")));
    }

    let previous_state =
        workspace.capture_state_for_worktree_switch(window, fallback_focused_dock, cx);
    let workspace_handle = workspace.weak_handle();
    let window_handle = window.window_handle().downcast::<MultiWorkspace>();
    let remote_connection_options = project.read(cx).remote_connection_options(cx);

    let (git_repos, non_git_paths) = classify_worktrees(project.read(cx), cx);

    if git_repos.is_empty() {
        let toast_workspace = cx.entity();
        show_error_toast(
            toast_workspace,
            "worktree create",
            anyhow!("No git repositories found in the project"),
            cx,
        );
        return Task::ready(Err(anyhow!("No git repositories found in the project")));
    }

    if remote_connection_options.is_some() {
        let is_disconnected = project
            .read(cx)
            .remote_client()
            .is_some_and(|client| client.read(cx).is_disconnected());
        if is_disconnected {
            let toast_workspace = cx.entity();
            show_error_toast(
                toast_workspace,
                "worktree create",
                anyhow!("Cannot create worktree: remote connection is not active"),
                cx,
            );
            return Task::ready(Err(anyhow!(
                "Cannot create worktree: remote connection is not active"
            )));
        }
    }

    let worktree_name = action.worktree_name.clone();
    let branch_target = action.branch_target.clone();
    let fetch_askpass_delegates = if remote_branch_fetch_mode.should_fetch() {
        remote_branch_to_fetch(&branch_target)
            .map(|(remote_name, _branch_name)| {
                git_repos
                    .iter()
                    .map(|_| {
                        create_worktree_askpass_delegate(
                            workspace_handle.clone(),
                            format!("git fetch {remote_name}"),
                            window,
                            cx,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let display_name: SharedString = worktree_name
        .as_deref()
        .unwrap_or("worktree")
        .to_string()
        .into();

    let operation_id = workspace.start_active_worktree_creation(display_name, false, cx);

    cx.spawn_in(window, async move |_workspace_entity, mut cx| {
        let result = do_create_worktree(
            git_repos,
            non_git_paths,
            worktree_name.clone(),
            branch_target.clone(),
            fetch_askpass_delegates,
            remote_branch_fetch_mode,
            previous_state,
            workspace_handle.clone(),
            window_handle,
            remote_connection_options,
            activate,
            operation_id,
            &mut cx,
        )
        .await;

        if let Err(err) = &result {
            log::error!("Failed to create worktree: {err}");
            workspace_handle
                .update(cx, |workspace, cx| {
                    workspace.clear_active_worktree_creation(operation_id, cx);
                    if let Some(fetch_error) = err.downcast_ref::<WorktreeFetchError>() {
                        let toast = cx.new(|cx| {
                            WorktreeFetchFailedToast::new(
                                workspace.weak_handle(),
                                worktree_name,
                                branch_target,
                                fallback_focused_dock,
                                fetch_error,
                                cx,
                            )
                        });
                        workspace.toggle_status_toast(toast, cx);
                    } else {
                        show_error_toast(cx.entity(), "worktree create", anyhow!("{err:#}"), cx);
                    }
                })
                .ok();
        }

        result
    })
}

pub fn handle_switch_worktree(
    workspace: &mut Workspace,
    action: &zed_actions::SwitchWorktree,
    window: &mut gpui::Window,
    fallback_focused_dock: Option<DockPosition>,
    cx: &mut gpui::Context<Workspace>,
) {
    let project = workspace.project().clone();

    if project.read(cx).repositories(cx).is_empty() {
        log::error!("switch_to_worktree: no git repository in the project");
        return;
    }
    if project.read(cx).is_via_collab() {
        log::error!("switch_to_worktree: not supported in collab projects");
        return;
    }

    if workspace.has_active_worktree_operation() {
        show_error_toast(
            cx.entity(),
            "worktree switch",
            anyhow!("A worktree operation is already in progress"),
            cx,
        );
        return;
    }

    let previous_state =
        workspace.capture_state_for_worktree_switch(window, fallback_focused_dock, cx);
    let workspace_handle = workspace.weak_handle();
    let window_handle = window.window_handle().downcast::<MultiWorkspace>();
    let remote_connection_options = project.read(cx).remote_connection_options(cx);

    let (git_repos, non_git_paths) = classify_worktrees(project.read(cx), cx);

    let git_repo_work_dirs: Vec<PathBuf> = git_repos
        .iter()
        .map(|repo| repo.read(cx).work_directory_abs_path.to_path_buf())
        .collect();

    let display_name: SharedString = action.display_name.clone().into();

    let operation_id = workspace.start_active_worktree_creation(display_name, true, cx);

    let worktree_path = action.path.clone();

    cx.spawn_in(window, async move |_workspace_entity, mut cx| {
        let result = do_switch_worktree(
            worktree_path,
            git_repo_work_dirs,
            non_git_paths,
            previous_state,
            workspace_handle.clone(),
            window_handle,
            remote_connection_options,
            operation_id,
            &mut cx,
        )
        .await;

        if let Err(err) = &result {
            log::error!("Failed to switch worktree: {err}");
            workspace_handle
                .update(cx, |workspace, cx| {
                    workspace.clear_active_worktree_creation(operation_id, cx);
                    show_error_toast(cx.entity(), "worktree switch", anyhow!("{err:#}"), cx);
                })
                .ok();
        }

        result
    })
    .detach_and_log_err(cx);
}

async fn do_create_worktree(
    git_repos: Vec<Entity<Repository>>,
    non_git_paths: Vec<PathBuf>,
    worktree_name: Option<String>,
    branch_target: NewWorktreeBranchTarget,
    fetch_askpass_delegates: Vec<AskPassDelegate>,
    remote_branch_fetch_mode: RemoteBranchFetchMode,
    previous_state: PreviousWorkspaceState,
    workspace: WeakEntity<Workspace>,
    window_handle: Option<gpui::WindowHandle<MultiWorkspace>>,
    remote_connection_options: Option<RemoteConnectionOptions>,
    activate: bool,
    operation_id: u64,
    cx: &mut AsyncWindowContext,
) -> anyhow::Result<CreatedWorktreeWorkspace> {
    // List existing worktrees from all repos to detect name collisions
    let worktree_receivers: Vec<_> = cx.update(|_, cx| {
        git_repos
            .iter()
            .map(|repo| repo.update(cx, |repo, _cx| repo.worktrees()))
            .collect()
    })?;
    let worktree_directory_setting = cx.update(|_, cx| {
        ProjectSettings::get_global(cx)
            .git
            .worktree_directory
            .clone()
    })?;

    let mut existing_worktree_names = Vec::new();
    let mut existing_worktree_paths = HashSet::default();
    for result in futures::future::join_all(worktree_receivers).await {
        match result {
            Ok(Ok(worktrees)) => {
                for worktree in worktrees {
                    if let Some(name) = worktree
                        .path
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                    {
                        existing_worktree_names.push(name.to_string());
                    }
                    existing_worktree_paths.insert(worktree.path.clone());
                }
            }
            Ok(Err(err)) => {
                Err::<(), _>(err).log_err();
            }
            Err(canceled) => {
                log::warn!("git worktree list request was canceled: {canceled}");
            }
        }
    }

    if remote_branch_fetch_mode.should_fetch()
        && let Some((remote_name, branch_name)) = remote_branch_to_fetch(&branch_target)
    {
        let remote_name = remote_name.to_string();
        let branch_name = branch_name.to_string();
        if let Err(error) = fetch_remote_for_worktree_base(
            &git_repos,
            remote_name.clone(),
            fetch_askpass_delegates,
            cx,
        )
        .await
        {
            return Err(WorktreeFetchError {
                remote_name,
                branch_name,
                source: error,
            }
            .into());
        }
    }

    let mut rng = rand::rng();

    let base_ref = resolve_worktree_branch_target(&branch_target);

    let (creation_infos, path_remapping, resolved_worktree_name) = cx.update(|_, cx| {
        start_worktree_creations(
            &git_repos,
            worktree_name,
            &existing_worktree_names,
            &existing_worktree_paths,
            base_ref,
            &worktree_directory_setting,
            &mut rng,
            cx,
        )
    })??;

    workspace
        .update(cx, |workspace, cx| {
            workspace.update_active_worktree_creation(
                operation_id,
                Some(resolved_worktree_name.into()),
                None,
                cx,
            );
        })
        .ok();

    let fs = cx.update(|_, cx| <dyn Fs>::global(cx))?;

    let creation_pairs: Vec<(Entity<Repository>, PathBuf)> = creation_infos
        .iter()
        .map(|(repo, path, _)| (repo.clone(), path.clone()))
        .collect();

    let created_paths = await_and_rollback_on_failure(creation_infos, fs.clone(), cx).await?;

    if remote_connection_options.is_none()
        && let Err(error) = copy_worktree_included_files(&path_remapping, fs.clone()).await
    {
        let rollback_failures = rollback_worktrees(&creation_pairs, fs, cx).await;
        let mut message = format!("Failed to copy worktree setup files: {error:#}");
        if !rollback_failures.is_empty() {
            message.push_str("\n\nFailed to clean up: ");
            message.push_str(&rollback_failures.join(", "));
        }
        return Err(anyhow!(message));
    }

    // Record each created worktree so thread archival can later verify that
    // Zed created it before deleting it from disk. Failures are non-fatal:
    // the worktree just won't be eligible for automatic archival.
    for (repo, path) in creation_pairs {
        crate::created_worktrees::record_created_worktree_for_repo(
            &repo,
            &path,
            remote_connection_options.as_ref(),
            cx,
        )
        .await;
    }

    // `path_remapping` has one entry per source git repo, while `created_paths`
    // has one per *unique* target worktree. When the former is larger, two or
    // more source repos were linked worktrees of the same underlying
    // repository and `start_worktree_creations` consolidated them.
    let consolidated_worktrees = path_remapping.len() > created_paths.len();

    workspace
        .update(cx, |workspace, cx| {
            workspace.update_active_worktree_creation(
                operation_id,
                None,
                Some(ActiveWorktreeCreationPhase::Loading),
                cx,
            );
        })
        .ok();

    let mut all_paths = created_paths;
    let has_non_git = !non_git_paths.is_empty();
    all_paths.extend(non_git_paths.iter().cloned());

    let workspace = open_worktree_workspace(
        all_paths,
        path_remapping,
        non_git_paths,
        has_non_git,
        previous_state,
        workspace,
        window_handle,
        remote_connection_options,
        WorktreeOperation::Create,
        activate,
        operation_id,
        cx,
    )
    .await?;

    Ok(CreatedWorktreeWorkspace {
        workspace,
        consolidated_worktrees,
    })
}

async fn do_switch_worktree(
    worktree_path: PathBuf,
    git_repo_work_dirs: Vec<PathBuf>,
    non_git_paths: Vec<PathBuf>,
    previous_state: PreviousWorkspaceState,
    workspace: WeakEntity<Workspace>,
    window_handle: Option<gpui::WindowHandle<MultiWorkspace>>,
    remote_connection_options: Option<RemoteConnectionOptions>,
    operation_id: u64,
    cx: &mut AsyncWindowContext,
) -> anyhow::Result<Entity<Workspace>> {
    let path_remapping: Vec<(PathBuf, PathBuf)> = git_repo_work_dirs
        .iter()
        .map(|work_dir| (work_dir.clone(), worktree_path.clone()))
        .collect();

    let mut all_paths = vec![worktree_path];
    let has_non_git = !non_git_paths.is_empty();
    all_paths.extend(non_git_paths.iter().cloned());

    open_worktree_workspace(
        all_paths,
        path_remapping,
        non_git_paths,
        has_non_git,
        previous_state,
        workspace,
        window_handle,
        remote_connection_options,
        WorktreeOperation::Switch,
        // Switching is always an explicit, foreground user action.
        true,
        operation_id,
        cx,
    )
    .await
}

async fn wait_for_initial_scan_or_timeout(
    workspace: &Entity<Workspace>,
    cx: &mut AsyncWindowContext,
) {
    let wait_for_scan = workspace
        .update(cx, |workspace, cx| {
            workspace.project().read(cx).wait_for_initial_scan(cx)
        })
        .fuse();
    let timeout = cx
        .background_executor()
        .timer(WORKTREE_INITIAL_SCAN_TIMEOUT)
        .fuse();
    futures::pin_mut!(wait_for_scan);
    futures::pin_mut!(timeout);

    select_biased! {
        _ = wait_for_scan => {}
        _ = timeout => {
            log::warn!(
                "timed out after {} seconds waiting for worktree initial scan",
                WORKTREE_INITIAL_SCAN_TIMEOUT.as_secs()
            );
        }
    }
}

async fn wait_for_repository_barriers_or_timeout(
    workspace: &Entity<Workspace>,
    cx: &mut AsyncWindowContext,
) {
    let barriers = workspace.update(cx, |workspace, cx| {
        let repos = workspace
            .project()
            .read(cx)
            .repositories(cx)
            .values()
            .cloned()
            .collect::<Vec<_>>();

        repos
            .into_iter()
            .map(|repo| repo.update(cx, |repo, _| repo.barrier()))
            .collect::<Vec<_>>()
    });

    if barriers.is_empty() {
        return;
    }

    let wait_for_barriers = async move {
        for result in futures::future::join_all(barriers).await {
            if let Err(err) = result {
                log::warn!("git repository barrier was canceled: {err}");
            }
        }
    }
    .fuse();
    let timeout = cx
        .background_executor()
        .timer(WORKTREE_REPOSITORY_BARRIER_TIMEOUT)
        .fuse();
    futures::pin_mut!(wait_for_barriers);
    futures::pin_mut!(timeout);

    select_biased! {
        _ = wait_for_barriers => {}
        _ = timeout => {
            log::warn!(
                "timed out after {} seconds waiting for worktree repository barriers",
                WORKTREE_REPOSITORY_BARRIER_TIMEOUT.as_secs()
            );
        }
    }
}

/// Core workspace opening logic shared by both create and switch flows.
/// Returns the newly opened workspace entity so callers can do post-open
/// work (e.g., the `create_thread` agent tool spawns a thread inside it).
async fn open_worktree_workspace(
    all_paths: Vec<PathBuf>,
    path_remapping: Vec<(PathBuf, PathBuf)>,
    non_git_paths: Vec<PathBuf>,
    has_non_git: bool,
    previous_state: PreviousWorkspaceState,
    workspace: WeakEntity<Workspace>,
    window_handle: Option<gpui::WindowHandle<MultiWorkspace>>,
    remote_connection_options: Option<RemoteConnectionOptions>,
    operation: WorktreeOperation,
    activate: bool,
    operation_id: u64,
    cx: &mut AsyncWindowContext,
) -> anyhow::Result<Entity<Workspace>> {
    let window_handle = window_handle
        .ok_or_else(|| anyhow!("No window handle available for workspace creation"))?;

    let focused_dock = previous_state.focused_dock;

    let is_creating_new_worktree = matches!(operation, WorktreeOperation::Create);

    // When `activate` is false the new workspace is opened in the background
    // (e.g. the agent's `create_thread` tool), so it should be a clean
    // checkout rather than inheriting the source workspace's open files and
    // dock layout. The state transfer only applies when we're foregrounding
    // a freshly-created worktree for the user.
    let transfer_state = is_creating_new_worktree && activate;

    let source_for_transfer = if transfer_state {
        Some(workspace.clone())
    } else {
        None
    };

    let (workspace_task, modal_workspace) =
        window_handle.update(cx, |multi_workspace, window, cx| {
            let path_list = util::path_list::PathList::new(&all_paths);
            let active_workspace = multi_workspace.workspace().clone();
            let modal_workspace = active_workspace.clone();

            let init: Option<
                Box<
                    dyn FnOnce(&mut Workspace, &mut gpui::Window, &mut gpui::Context<Workspace>)
                        + Send,
                >,
            > = if transfer_state {
                let dock_structure = previous_state.dock_structure;
                Some(Box::new(
                    move |workspace: &mut Workspace,
                          window: &mut gpui::Window,
                          cx: &mut gpui::Context<Workspace>| {
                        workspace.set_dock_structure(dock_structure, window, cx);
                    },
                ))
            } else {
                None
            };

            let task = multi_workspace.find_or_create_workspace_with_source_workspace(
                path_list,
                remote_connection_options,
                None,
                move |connection_options, window, cx| {
                    remote_connection::connect_with_modal(
                        &active_workspace,
                        connection_options,
                        window,
                        cx,
                    )
                },
                &[],
                init,
                OpenMode::Add,
                source_for_transfer.clone(),
                window,
                cx,
            );
            (task, modal_workspace)
        })?;

    let workspace_task = workspace_task.fuse();
    let workspace_open_timeout = cx
        .background_executor()
        .timer(WORKTREE_WORKSPACE_OPEN_TIMEOUT)
        .fuse();
    futures::pin_mut!(workspace_task);
    futures::pin_mut!(workspace_open_timeout);
    let result = select_biased! {
        result = workspace_task => result,
        _ = workspace_open_timeout => {
            workspace
                .update(cx, |ws, cx| {
                    ws.clear_active_worktree_creation(operation_id, cx);
                })
                .ok();
            Err(anyhow!(
                "Timed out after {} seconds waiting for the worktree workspace to open",
                WORKTREE_WORKSPACE_OPEN_TIMEOUT.as_secs()
            ))
        }
    };
    remote_connection::dismiss_connection_modal(&modal_workspace, cx);
    let new_workspace = result?;

    workspace
        .update(cx, |ws, cx| {
            ws.hide_active_worktree_creation(operation_id, cx);
        })
        .ok();

    let panels_task = new_workspace.update(cx, |workspace, _cx| workspace.take_panels_task());

    if let Some(task) = panels_task {
        cx.update(|_, cx| task.detach_and_log_err(cx)).ok();
    }

    wait_for_initial_scan_or_timeout(&new_workspace, cx).await;
    wait_for_repository_barriers_or_timeout(&new_workspace, cx).await;

    maybe_propagate_worktree_trust(&workspace, &new_workspace, &all_paths, cx);

    if transfer_state {
        window_handle.update(cx, |_multi_workspace, window, cx| {
            new_workspace.update(cx, |workspace, cx| {
                if has_non_git {
                    struct WorktreeCreationToast;
                    let toast_id =
                        workspace::notifications::NotificationId::unique::<WorktreeCreationToast>();
                    workspace.show_toast(
                        workspace::Toast::new(
                            toast_id,
                            "Some project folders are not git repositories. \
                             They were included as-is without creating a worktree.",
                        ),
                        cx,
                    );
                }

                // Remap every previously-open file path into the new worktree.
                let remap_path = |original_path: PathBuf| -> Option<PathBuf> {
                    let best_match = path_remapping
                        .iter()
                        .filter_map(|(old_root, new_root)| {
                            original_path.strip_prefix(old_root).ok().map(|relative| {
                                (old_root.components().count(), new_root.join(relative))
                            })
                        })
                        .max_by_key(|(depth, _)| *depth);

                    if let Some((_, remapped_path)) = best_match {
                        return Some(remapped_path);
                    }

                    for non_git in &non_git_paths {
                        if original_path.starts_with(non_git) {
                            return Some(original_path);
                        }
                    }
                    None
                };

                let remapped_active_path =
                    previous_state.active_file_path.and_then(|p| remap_path(p));

                let mut paths_to_open: Vec<PathBuf> = Vec::new();
                let mut seen = HashSet::default();
                for path in previous_state.open_file_paths {
                    if let Some(remapped) = remap_path(path) {
                        if remapped_active_path.as_ref() != Some(&remapped)
                            && seen.insert(remapped.clone())
                        {
                            paths_to_open.push(remapped);
                        }
                    }
                }

                if let Some(active) = &remapped_active_path {
                    if seen.insert(active.clone()) {
                        paths_to_open.push(active.clone());
                    }
                }

                if !paths_to_open.is_empty() {
                    let should_focus_center = focused_dock.is_none();
                    let open_task = workspace.open_paths(
                        paths_to_open,
                        workspace::OpenOptions {
                            focus: Some(false),
                            ..Default::default()
                        },
                        None,
                        window,
                        cx,
                    );
                    cx.spawn_in(window, async move |workspace, cx| {
                        for item in open_task.await.into_iter().flatten() {
                            item.log_err();
                        }
                        if should_focus_center {
                            workspace.update_in(cx, |workspace, window, cx| {
                                workspace.focus_center_pane(window, cx);
                            })?;
                        }
                        anyhow::Ok(())
                    })
                    .detach_and_log_err(cx);
                }
            });
        })?;
    }

    let setup_task = window_handle.update(cx, |multi_workspace, window, cx| {
        if activate {
            multi_workspace.activate(new_workspace.clone(), source_for_transfer, window, cx);
        } else {
            // Background open: register the new workspace as a retained tab
            // but leave the user where they are.
            multi_workspace.add_background_workspace(new_workspace.clone(), window, cx);
        }

        if is_creating_new_worktree {
            Some(new_workspace.update(cx, |workspace, cx| {
                let setup_task = workspace.run_create_worktree_tasks(window, cx);

                if activate && let Some(dock_position) = focused_dock {
                    let dock = workspace.dock_at_position(dock_position);
                    if let Some(panel) = dock.read(cx).active_panel() {
                        panel.panel_focus_handle(cx).focus(window, cx);
                    }
                }
                setup_task
            }))
        } else {
            None
        }
    })?;

    if let Some(setup_task) = setup_task
        && let Err(error) = setup_task.await
    {
        let message = format!("Failed to set up worktree: {error:#}");
        new_workspace.update(cx, |workspace, cx| {
            workspace.show_error(anyhow!(message.clone()), cx);
        });
        workspace
            .update(cx, |workspace, cx| {
                workspace.clear_active_worktree_creation(operation_id, cx);
            })
            .ok();
        return Err(anyhow!(message));
    }

    workspace
        .update(cx, |workspace, cx| {
            workspace.clear_active_worktree_creation(operation_id, cx);
        })
        .ok();

    Ok(new_workspace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::Fs;
    use gpui::{App, Task, TestAppContext};
    use language::language_settings::AllLanguageSettings;
    use project::project_settings::ProjectSettings;
    use project::task_store::{TaskSettingsLocation, TaskStore};
    use project::{FakeFs, WorktreeSettings};
    use serde_json::json;
    use settings::{SettingsLocation, SettingsStore};
    use std::path::{Path, PathBuf};
    use std::process::ExitStatus;
    use std::sync::Mutex;
    use task::SpawnInTerminal;
    use theme::LoadThemes;
    use util::path;
    use util::rel_path::rel_path;
    use workspace::{TerminalProvider, WorkspaceSettings};

    struct CountingTerminalProvider {
        spawned_task_labels: Arc<Mutex<Vec<String>>>,
    }

    struct RecordingTerminalProvider {
        spawned_tasks: Arc<Mutex<Vec<SpawnInTerminal>>>,
        fail: bool,
    }

    impl TerminalProvider for CountingTerminalProvider {
        fn spawn(
            &self,
            task: SpawnInTerminal,
            _window: &mut ui::Window,
            _cx: &mut App,
        ) -> Task<Option<anyhow::Result<ExitStatus>>> {
            self.spawned_task_labels
                .lock()
                .expect("terminal spawn mutex should not be poisoned")
                .push(task.label);
            Task::ready(Some(Ok(ExitStatus::default())))
        }
    }

    impl TerminalProvider for RecordingTerminalProvider {
        fn spawn(
            &self,
            task: SpawnInTerminal,
            _window: &mut ui::Window,
            _cx: &mut App,
        ) -> Task<Option<anyhow::Result<ExitStatus>>> {
            self.spawned_tasks
                .lock()
                .expect("terminal spawn mutex should not be poisoned")
                .push(task);
            if self.fail {
                Task::ready(Some(Err(anyhow!("terminal task failed"))))
            } else {
                Task::ready(Some(Ok(ExitStatus::default())))
            }
        }
    }

    fn init_test(cx: &mut TestAppContext) {
        zlog::init_test();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(LoadThemes::JustBase, cx);
            AllLanguageSettings::register(cx);
            editor::init(cx);
            ProjectSettings::register(cx);
            WorktreeSettings::register(cx);
            WorkspaceSettings::register(cx);
            TaskStore::init(None);
        });
    }

    fn install_counting_provider_and_worktree_hook(
        workspace: &Entity<Workspace>,
        spawned_task_labels: &Arc<Mutex<Vec<String>>>,
        main_project_root: &Path,
        hook_tasks_json: &str,
        cx: &mut App,
    ) {
        workspace.update(cx, |workspace, cx| {
            workspace.set_terminal_provider(CountingTerminalProvider {
                spawned_task_labels: spawned_task_labels.clone(),
            });

            let project = workspace.project().clone();
            let Some(worktree) = project.read(cx).worktrees(cx).next() else {
                return;
            };
            let worktree = worktree.read(cx);
            let worktree_id = worktree.id();
            let worktree_root = worktree.abs_path().to_path_buf();
            if worktree_root == main_project_root {
                return;
            }

            let Some(task_inventory) = project
                .read(cx)
                .task_store()
                .read(cx)
                .task_inventory()
                .cloned()
            else {
                return;
            };
            task_inventory.update(cx, |inventory, _| {
                inventory
                    .update_file_based_tasks(
                        TaskSettingsLocation::Worktree(SettingsLocation {
                            worktree_id,
                            path: rel_path(".zed"),
                        }),
                        Some(hook_tasks_json),
                    )
                    .expect("should inject create_worktree hook tasks for linked worktree");
            });
        });
    }

    #[gpui::test]
    async fn test_copy_worktree_included_files_without_overwriting_checkout(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/root"),
            json!({
                "source": {
                    ".git": {
                        "private": "do not copy",
                    },
                    ".worktreeinclude": ".env.local\nconfig/local/**\n!config/local/skip.txt\nlinks/tool\n.git/**\n",
                    ".env.local": "source secret",
                    "config": {
                        "local": {
                            "settings.json": "copied settings",
                            "skip.txt": "excluded",
                        },
                    },
                    "links": {},
                },
                "destination": {
                    ".git": {},
                    ".env.local": "checkout wins",
                },
            }),
        )
        .await;
        fs.create_symlink(
            path!("/root/source/links/tool").as_ref(),
            PathBuf::from("../config/local/settings.json"),
        )
        .await
        .expect("should create source symlink");

        copy_worktree_included_files_for_root(
            path!("/root/source").as_ref(),
            path!("/root/destination").as_ref(),
            fs.clone(),
        )
        .await
        .expect("included files should copy");

        assert_eq!(
            fs.load(path!("/root/destination/.env.local").as_ref())
                .await
                .expect("destination file should exist"),
            "checkout wins"
        );
        assert_eq!(
            fs.load(path!("/root/destination/config/local/settings.json").as_ref())
                .await
                .expect("included file should exist"),
            "copied settings"
        );
        assert!(
            !fs.is_file(path!("/root/destination/config/local/skip.txt").as_ref())
                .await
        );
        assert!(
            !fs.is_file(path!("/root/destination/.git/private").as_ref())
                .await
        );
        assert_eq!(
            fs.read_link(path!("/root/destination/links/tool").as_ref())
                .await
                .expect("included symlink should exist"),
            PathBuf::from("../config/local/settings.json")
        );
        assert!(worktree_include_scan_roots("../outside").is_err());
        assert!(worktree_include_scan_roots("..").is_err());
        assert_eq!(
            worktree_include_scan_roots("config/local/file\\ ")
                .expect("escaped patterns should be valid"),
            vec![PathBuf::new()]
        );
    }

    #[gpui::test]
    async fn test_remove_worktree_hook_uses_target_path_and_propagates_failure(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let hook_tasks_json = r#"[{"label":"teardown worktree","command":"echo","hide":"never","hooks":["remove_worktree"]}]"#;
        let fs = FakeFs::new(cx.background_executor.clone());
        cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    ".git": {},
                    ".zed": {
                        "tasks.json": hook_tasks_json,
                    },
                },
            }),
        )
        .await;

        let project_root = PathBuf::from(path!("/root/project"));
        let target_root = PathBuf::from(path!("/root/worktrees/feature"));
        let project = Project::test(fs, [project_root.as_path()], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let spawned_tasks = Arc::new(Mutex::new(Vec::new()));
        let worktree_id = workspace.update(cx, |workspace, cx| {
            workspace.set_terminal_provider(RecordingTerminalProvider {
                spawned_tasks: spawned_tasks.clone(),
                fail: true,
            });
            let worktree = project
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("project should have a worktree")
                .read(cx);
            let worktree_id = worktree.id();
            let inventory = project
                .read(cx)
                .task_store()
                .read(cx)
                .task_inventory()
                .cloned()
                .expect("task inventory should exist");
            inventory.update(cx, |inventory, _| {
                inventory
                    .update_file_based_tasks(
                        TaskSettingsLocation::Worktree(SettingsLocation {
                            worktree_id,
                            path: rel_path(".zed"),
                        }),
                        Some(hook_tasks_json),
                    )
                    .expect("should inject remove_worktree hook task");
            });
            worktree_id
        });

        let teardown_context =
            RemoveWorktreeTaskContext::new(workspace.downgrade(), worktree_id, Some(project_root));
        let mut async_window_context = cx.update(|window, cx| window.to_async(cx));
        let error = run_remove_worktree_tasks(
            teardown_context,
            target_root.clone(),
            &mut async_window_context,
        )
        .await
        .expect_err("terminal failure should block teardown");
        assert!(error.to_string().contains("terminal task failed"));
        let spawned_tasks = spawned_tasks
            .lock()
            .expect("terminal spawn mutex should not be poisoned");
        assert_eq!(spawned_tasks.len(), 1);
        assert_eq!(spawned_tasks[0].cwd.as_ref(), Some(&target_root));
    }

    #[gpui::test]
    async fn test_create_worktree_hook_does_not_run_when_switching_back_to_main_worktree(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let hook_tasks_json = r#"[{"label":"setup worktree","command":"echo","hide":"never","hooks":["create_worktree"]}]"#;
        let fs = FakeFs::new(cx.background_executor.clone());
        cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
        fs.insert_tree(
            "/root",
            json!({
                "project": {
                    ".git": {},
                    ".zed": {
                        "tasks.json": hook_tasks_json,
                    },
                    "src": {
                        "main.rs": "fn main() {}",
                    },
                },
            }),
        )
        .await;

        let main_project_root = PathBuf::from(path!("/root/project"));
        let project = Project::test(fs.clone(), [main_project_root.as_path()], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

        let spawned_task_labels = Arc::new(Mutex::new(Vec::new()));
        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.retain_active_workspace(cx);
            let active_workspace = multi_workspace.workspace().clone();
            install_counting_provider_and_worktree_hook(
                &active_workspace,
                &spawned_task_labels,
                &main_project_root,
                hook_tasks_json,
                cx,
            );
        });

        let main_workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        main_workspace.update_in(cx, |workspace, window, cx| {
            handle_create_worktree(
                workspace,
                &zed_actions::CreateWorktree {
                    worktree_name: Some("feature".to_string()),
                    branch_target: NewWorktreeBranchTarget::CurrentBranch,
                },
                window,
                None,
                cx,
            );
        });
        cx.run_until_parked();

        let active_workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        cx.update(|_, cx| {
            install_counting_provider_and_worktree_hook(
                &active_workspace,
                &spawned_task_labels,
                &main_project_root,
                hook_tasks_json,
                cx,
            );
        });
        active_workspace.update_in(cx, |workspace, window, cx| {
            workspace
                .run_create_worktree_tasks(window, cx)
                .detach_and_log_err(cx);
        });
        cx.run_until_parked();

        assert_eq!(
            spawned_task_labels
                .lock()
                .expect("terminal spawn mutex should not be poisoned")
                .as_slice(),
            ["setup worktree"],
            "create_worktree hook should run once for the created linked worktree"
        );

        active_workspace.update_in(cx, |workspace, window, cx| {
            handle_switch_worktree(
                workspace,
                &zed_actions::SwitchWorktree {
                    path: main_project_root.clone(),
                    display_name: "project".to_string(),
                },
                window,
                None,
                cx,
            );
        });
        cx.run_until_parked();

        assert_eq!(
            spawned_task_labels
                .lock()
                .expect("terminal spawn mutex should not be poisoned")
                .as_slice(),
            ["setup worktree"],
            "switching back to the main worktree should not rerun create_worktree hooks"
        );
    }

    #[gpui::test]
    async fn test_linked_worktree_inherits_trust_from_main_worktree(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            project::trusted_worktrees::init(collections::HashMap::default(), cx);
        });

        let fs = FakeFs::new(cx.background_executor.clone());
        cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
        fs.insert_tree(
            "/root",
            json!({
                "project": {
                    ".git": {},
                    "src": {
                        "main.rs": "fn main() {}",
                    },
                },
            }),
        )
        .await;

        let main_project_root = PathBuf::from(path!("/root/project"));
        let project =
            Project::test_with_worktree_trust(fs.clone(), [main_project_root.as_path()], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;

        // The main worktree starts restricted; trust it explicitly
        let worktree_store = project.read_with(cx, |project, _| project.worktree_store());
        let main_worktree_id = worktree_store.read_with(cx, |store, cx| {
            store
                .worktrees()
                .next()
                .map(|wt| wt.read(cx).id())
                .expect("should have a worktree")
        });
        let trusted_store = cx
            .read(|cx| project::trusted_worktrees::TrustedWorktrees::try_get_global(cx))
            .expect("trust store should exist");
        trusted_store.update(cx, |store, cx| {
            store.trust(
                &worktree_store,
                collections::HashSet::from_iter([project::trusted_worktrees::PathTrust::Worktree(
                    main_worktree_id,
                )]),
                cx,
            );
        });

        // Verify main worktree is now trusted
        let has_restricted = cx.read(|cx| {
            project::trusted_worktrees::TrustedWorktrees::has_restricted_worktrees(
                &worktree_store,
                cx,
            )
        });
        assert!(
            !has_restricted,
            "main worktree should be trusted after explicit trust"
        );

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.retain_active_workspace(cx);
        });

        // Create a linked worktree from the trusted main worktree
        let main_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        main_workspace.update_in(cx, |workspace, window, cx| {
            handle_create_worktree(
                workspace,
                &zed_actions::CreateWorktree {
                    worktree_name: Some("feature".to_string()),
                    branch_target: NewWorktreeBranchTarget::CurrentBranch,
                },
                window,
                None,
                cx,
            );
        });
        cx.run_until_parked();

        // The new workspace (linked worktree) should inherit trust
        let new_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let new_worktree_store =
            new_workspace.read_with(cx, |ws, cx| ws.project().read(cx).worktree_store());
        let new_has_restricted = cx.read(|cx| {
            project::trusted_worktrees::TrustedWorktrees::has_restricted_worktrees(
                &new_worktree_store,
                cx,
            )
        });
        assert!(
            !new_has_restricted,
            "linked worktree should inherit trust from the main worktree"
        );

        // The security modal should not be showing
        let has_modal = new_workspace.read_with(cx, |ws, cx| {
            ws.active_modal::<workspace::security_modal::SecurityModal>(cx)
                .is_some()
        });
        assert!(
            !has_modal,
            "security modal should not show for a linked worktree created from a trusted main worktree"
        );
    }

    #[test]
    fn test_remote_branch_name_parse() {
        assert_eq!(
            RemoteBranchName::parse("refs/remotes/origin/main"),
            Some(RemoteBranchName {
                remote_name: "origin".to_string(),
                branch_name: "main".to_string(),
            })
        );
        assert_eq!(
            RemoteBranchName::parse("upstream/feature/foo"),
            Some(RemoteBranchName {
                remote_name: "upstream".to_string(),
                branch_name: "feature/foo".to_string(),
            })
        );
        assert_eq!(RemoteBranchName::parse("main"), None);
        assert_eq!(RemoteBranchName::parse("origin/"), None);
    }

    #[test]
    fn test_worktree_create_targets() {
        let origin_main = RemoteBranchName {
            remote_name: "origin".to_string(),
            branch_name: "main".to_string(),
        };

        // Multiple repositories: only the current branch, regardless of default.
        assert_eq!(
            worktree_create_targets(true, Some(origin_main.clone()), Some("feature")),
            vec![WorktreeCreateTarget::CurrentBranch]
        );

        // Default branch differs from current: offer both, default first.
        assert_eq!(
            worktree_create_targets(false, Some(origin_main.clone()), Some("feature")),
            vec![
                WorktreeCreateTarget::DefaultBranch(origin_main.clone()),
                WorktreeCreateTarget::CurrentBranch,
            ]
        );

        // Current branch matches the default: only the default branch entry.
        assert_eq!(
            worktree_create_targets(false, Some(origin_main.clone()), Some("main")),
            vec![WorktreeCreateTarget::DefaultBranch(origin_main)]
        );

        // No default branch resolved: fall back to the current branch.
        assert_eq!(
            worktree_create_targets(false, None, Some("feature")),
            vec![WorktreeCreateTarget::CurrentBranch]
        );
    }

    #[test]
    fn test_worktree_create_target_branch_label() {
        let origin_main = RemoteBranchName {
            remote_name: "origin".to_string(),
            branch_name: "main".to_string(),
        };
        assert_eq!(
            WorktreeCreateTarget::DefaultBranch(origin_main).branch_label(false, Some("feature")),
            "origin/main"
        );
        assert_eq!(
            WorktreeCreateTarget::CurrentBranch.branch_label(false, Some("feature")),
            "feature"
        );
        // Detached HEAD falls back to "HEAD".
        assert_eq!(
            WorktreeCreateTarget::CurrentBranch.branch_label(false, None),
            "HEAD"
        );
        // Multiple repositories pluralize the current branch.
        assert_eq!(
            WorktreeCreateTarget::CurrentBranch.branch_label(true, Some("feature")),
            "current branches"
        );
    }
}
