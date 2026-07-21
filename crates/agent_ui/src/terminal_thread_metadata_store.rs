use std::path::{Path, PathBuf};
#[cfg(not(any(test, feature = "test-support")))]
use std::time::Duration;

use anyhow::{Context as _, bail};
use chrono::{DateTime, Utc};
use collections::{HashMap, HashSet};
use db::{
    sqlez::{
        bindable::Column, domain::Domain, statement::Statement,
        thread_safe_connection::ThreadSafeConnection,
    },
    sqlez_macros::sql,
};
use futures::{FutureExt, future::Shared};
use gpui::{AppContext as _, Entity, EventEmitter, Global, Task};
use remote::{RemoteConnectionOptions, same_remote_connection_identity};
use ui::{AgentThreadStatus, App, Context, SharedString};
use util::ResultExt as _;
use workspace::PathList;

use crate::{TerminalId, thread_metadata_store::WorktreePaths};

pub(crate) const TERMINAL_THREAD_TMUX_SERVER_NAME: &str = "zed-terminal-threads";

pub(crate) fn terminal_thread_tmux_session_name(terminal_id: TerminalId) -> String {
    format!("zed-{}", terminal_id.to_key_string())
}

pub fn init(cx: &mut App) {
    TerminalThreadMetadataStore::init_global(cx);
    TerminalThreadStatusStore::init_global(cx);
}

struct GlobalTerminalThreadMetadataStore(Entity<TerminalThreadMetadataStore>);
impl Global for GlobalTerminalThreadMetadataStore {}

#[cfg(any(test, feature = "test-support"))]
pub struct TestTerminalMetadataDbName(pub String);
#[cfg(any(test, feature = "test-support"))]
impl Global for TestTerminalMetadataDbName {}

#[cfg(any(test, feature = "test-support"))]
impl TestTerminalMetadataDbName {
    pub fn global(cx: &App) -> String {
        cx.try_global::<Self>()
            .map(|global| global.0.clone())
            .unwrap_or_else(|| {
                let thread = std::thread::current();
                let test_name = thread.name().unwrap_or("unknown_test");
                format!("TERMINAL_THREAD_METADATA_DB_{}", test_name)
            })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TerminalThreadMetadata {
    pub terminal_id: TerminalId,
    pub title: SharedString,
    pub custom_title: Option<SharedString>,
    pub created_at: DateTime<Utc>,
    pub worktree_paths: WorktreePaths,
    pub remote_connection: Option<RemoteConnectionOptions>,
    pub working_directory: Option<PathBuf>,
    pub initial_command: Option<String>,
    pub registry: TerminalThreadRegistryMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalThreadKind {
    Agent,
    Shell,
}

impl TerminalThreadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Shell => "shell",
        }
    }

    fn from_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "agent" => Ok(Self::Agent),
            "shell" => Ok(Self::Shell),
            _ => bail!("invalid terminal thread kind {value:?}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalThreadProvider {
    Codex,
}

impl TerminalThreadProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
        }
    }

    fn from_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "codex" => Ok(Self::Codex),
            _ => bail!("invalid terminal thread provider {value:?}"),
        }
    }

    pub fn from_launch_metadata(
        initial_command: Option<&str>,
        custom_title: Option<&str>,
    ) -> Option<Self> {
        let executable = initial_command
            .and_then(|command| command.split_whitespace().next())
            .map(|executable| executable.trim_matches(['\'', '"']))
            .and_then(|executable| Path::new(executable).file_name())
            .and_then(|executable| executable.to_str());

        executable
            .is_some_and(|executable| executable == "codex" || executable.starts_with("codex-"))
            .then_some(Self::Codex)
            .or_else(|| {
                (initial_command.is_none() && custom_title == Some("Codex")).then_some(Self::Codex)
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalThreadLifecycleState {
    Attached,
    Detached,
    Stopping,
}

impl TerminalThreadLifecycleState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Attached => "attached",
            Self::Detached => "detached",
            Self::Stopping => "stopping",
        }
    }

    fn from_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "attached" => Ok(Self::Attached),
            "detached" => Ok(Self::Detached),
            "stopping" => Ok(Self::Stopping),
            _ => bail!("invalid terminal thread lifecycle state {value:?}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TerminalThreadRegistryMetadata {
    pub kind: TerminalThreadKind,
    pub provider: Option<TerminalThreadProvider>,
    pub tmux_server_name: String,
    pub tmux_session_name: String,
    pub provider_session_id: Option<String>,
    pub lifecycle_state: TerminalThreadLifecycleState,
    pub last_attached_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

impl TerminalThreadRegistryMetadata {
    pub fn for_launch(
        terminal_id: TerminalId,
        initial_command: Option<&str>,
        custom_title: Option<&str>,
    ) -> Self {
        let provider = TerminalThreadProvider::from_launch_metadata(initial_command, custom_title);
        let kind = if provider.is_some() {
            TerminalThreadKind::Agent
        } else {
            TerminalThreadKind::Shell
        };
        Self::detached(terminal_id, kind, provider)
    }

    pub fn detached(
        terminal_id: TerminalId,
        kind: TerminalThreadKind,
        provider: Option<TerminalThreadProvider>,
    ) -> Self {
        Self {
            kind,
            provider,
            tmux_server_name: TERMINAL_THREAD_TMUX_SERVER_NAME.to_string(),
            tmux_session_name: terminal_thread_tmux_session_name(terminal_id),
            provider_session_id: None,
            lifecycle_state: TerminalThreadLifecycleState::Detached,
            last_attached_at: None,
            updated_at: Utc::now(),
        }
    }

    pub fn mark_attached(&mut self) {
        let now = Utc::now();
        self.lifecycle_state = TerminalThreadLifecycleState::Attached;
        self.last_attached_at = Some(now);
        self.updated_at = now;
    }

    pub fn mark_detached(&mut self) {
        self.lifecycle_state = TerminalThreadLifecycleState::Detached;
        self.updated_at = Utc::now();
    }

    pub fn mark_stopping(&mut self) {
        self.lifecycle_state = TerminalThreadLifecycleState::Stopping;
        self.updated_at = Utc::now();
    }
}

impl TerminalThreadMetadata {
    pub fn folder_paths(&self) -> &PathList {
        self.worktree_paths.folder_path_list()
    }

    pub fn main_worktree_paths(&self) -> &PathList {
        self.worktree_paths.main_worktree_path_list()
    }

    pub fn display_title(&self) -> SharedString {
        compose_terminal_thread_title(
            self.title.as_ref(),
            self.custom_title.as_ref().map(|title| title.as_ref()),
        )
    }
}

pub(crate) fn compose_terminal_thread_title(
    terminal_title: &str,
    custom_title: Option<&str>,
) -> SharedString {
    let Some(custom_title) = custom_title.filter(|title| !title.trim().is_empty()) else {
        return SharedString::from(terminal_title.to_string());
    };

    if let Some(prefix) = terminal_title_prefix(terminal_title) {
        SharedString::from(format!("{prefix}{custom_title}"))
    } else {
        SharedString::from(custom_title.to_string())
    }
}

pub(crate) fn terminal_title_without_prefix(title: &str) -> &str {
    terminal_title_prefix(title)
        .map(|prefix| &title[prefix.len()..])
        .unwrap_or(title)
}

pub(crate) fn terminal_title_for_persistence(title: &str) -> SharedString {
    const ACTION_REQUIRED_PREFIXES: [&str; 2] = ["[ ! ] Action Required", "[ . ] Action Required"];

    if ACTION_REQUIRED_PREFIXES
        .iter()
        .any(|prefix| title.starts_with(prefix))
    {
        return title
            .split_once(" | ")
            .map(|(_, title)| SharedString::from(title.to_string()))
            .unwrap_or_default();
    }

    SharedString::from(terminal_title_without_prefix(title).to_string())
}

pub fn terminal_title_prefix(title: &str) -> Option<&str> {
    let mut prefix_byte_len = 0;
    let mut saw_prefix_character = false;
    let mut saw_whitespace_after_prefix = false;

    let mut chars = title.chars().peekable();
    while let Some(character) = chars.next() {
        if character.is_alphanumeric() {
            return None;
        }

        if character.is_whitespace() {
            if !saw_prefix_character {
                return None;
            }

            prefix_byte_len += character.len_utf8();
            saw_whitespace_after_prefix = true;

            while let Some(character) = chars.peek() {
                if !character.is_whitespace() {
                    break;
                }

                prefix_byte_len += character.len_utf8();
                chars.next();
            }

            break;
        }

        saw_prefix_character = true;
        prefix_byte_len += character.len_utf8();
    }

    if saw_whitespace_after_prefix {
        Some(&title[..prefix_byte_len])
    } else {
        None
    }
}

pub struct TerminalThreadStatusStore {
    statuses: HashMap<TerminalId, AgentThreadStatus>,
    running_status_frames: HashMap<TerminalId, u8>,
    running_status_animation_active: bool,
    _running_status_animation_task: Option<Task<()>>,
}

const RUNNING_STATUS_FRAME_COUNT: u8 = 8;
#[cfg(not(any(test, feature = "test-support")))]
const RUNNING_STATUS_FRAME_INTERVAL: Duration = Duration::from_millis(125);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalThreadStatusChanged {
    pub terminal_id: TerminalId,
    pub previous_status: Option<AgentThreadStatus>,
    pub status: AgentThreadStatus,
}

struct GlobalTerminalThreadStatusStore(Entity<TerminalThreadStatusStore>);

impl Global for GlobalTerminalThreadStatusStore {}
impl EventEmitter<TerminalThreadStatusChanged> for TerminalThreadStatusStore {}

impl TerminalThreadStatusStore {
    pub fn init_global(cx: &mut App) {
        if cx.has_global::<GlobalTerminalThreadStatusStore>() {
            return;
        }

        let store = cx.new(|_| Self {
            statuses: HashMap::default(),
            running_status_frames: HashMap::default(),
            running_status_animation_active: false,
            _running_status_animation_task: None,
        });
        cx.set_global(GlobalTerminalThreadStatusStore(store));
    }

    pub fn global(cx: &mut App) -> Entity<Self> {
        Self::init_global(cx);
        cx.global::<GlobalTerminalThreadStatusStore>().0.clone()
    }

    pub fn status(&self, terminal_id: TerminalId) -> AgentThreadStatus {
        self.statuses.get(&terminal_id).copied().unwrap_or_default()
    }

    pub fn running_status_phase(&self, terminal_id: TerminalId) -> f32 {
        let frame = self
            .running_status_frames
            .get(&terminal_id)
            .copied()
            .unwrap_or_default();
        f32::from(frame) / f32::from(RUNNING_STATUS_FRAME_COUNT)
    }

    pub fn any_running_status_phase(&self) -> Option<f32> {
        self.running_status_frames
            .values()
            .next()
            .map(|frame| f32::from(*frame) / f32::from(RUNNING_STATUS_FRAME_COUNT))
    }

    pub fn set_status(
        &mut self,
        terminal_id: TerminalId,
        status: AgentThreadStatus,
        cx: &mut Context<Self>,
    ) {
        let previous_status = self.statuses.get(&terminal_id).copied();
        if previous_status == Some(status) {
            return;
        }

        self.statuses.insert(terminal_id, status);
        if status == AgentThreadStatus::Running {
            self.running_status_frames.insert(terminal_id, 0);
            self.start_running_status_animation(cx);
        } else {
            self.running_status_frames.remove(&terminal_id);
        }
        cx.emit(TerminalThreadStatusChanged {
            terminal_id,
            previous_status,
            status,
        });
        cx.notify();
    }

    pub fn remove(&mut self, terminal_id: TerminalId, cx: &mut Context<Self>) {
        let removed_status = self.statuses.remove(&terminal_id).is_some();
        let removed_frame = self.running_status_frames.remove(&terminal_id).is_some();
        if removed_status || removed_frame {
            cx.notify();
        }
    }

    fn start_running_status_animation(&mut self, cx: &mut Context<Self>) {
        if self.running_status_animation_active {
            return;
        }

        self.running_status_animation_active = true;

        #[cfg(any(test, feature = "test-support"))]
        {
            let _ = cx;
        }

        #[cfg(not(any(test, feature = "test-support")))]
        {
            self._running_status_animation_task = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(RUNNING_STATUS_FRAME_INTERVAL)
                        .await;
                    let Ok(should_continue) = this.update(cx, |this, cx| {
                        let should_continue = this.advance_running_status_frames(cx);
                        if !should_continue {
                            this.running_status_animation_active = false;
                        }
                        should_continue
                    }) else {
                        break;
                    };
                    if !should_continue {
                        break;
                    }
                }
            }));
        }
    }

    fn advance_running_status_frames(&mut self, cx: &mut Context<Self>) -> bool {
        if self.running_status_frames.is_empty() {
            return false;
        }

        for frame in self.running_status_frames.values_mut() {
            *frame = (*frame + 1) % RUNNING_STATUS_FRAME_COUNT;
        }
        cx.notify();
        true
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn advance_running_status_frames_for_test(&mut self, cx: &mut Context<Self>) {
        self.advance_running_status_frames(cx);
    }
}

pub struct TerminalThreadMetadataStore {
    db: TerminalThreadMetadataDb,
    terminals: HashMap<TerminalId, TerminalThreadMetadata>,
    terminals_by_paths: HashMap<PathList, HashSet<TerminalId>>,
    terminals_by_main_paths: HashMap<PathList, HashSet<TerminalId>>,
    reload_task: Option<Shared<Task<()>>>,
    pending_terminal_ops_tx: async_channel::Sender<DbOperation>,
    _db_operations_task: Task<()>,
}

#[derive(Debug, PartialEq)]
enum DbOperation {
    Upsert(TerminalThreadMetadata),
    Delete(TerminalId),
}

impl DbOperation {
    fn id(&self) -> TerminalId {
        match self {
            DbOperation::Upsert(metadata) => metadata.terminal_id,
            DbOperation::Delete(terminal_id) => *terminal_id,
        }
    }
}

impl TerminalThreadMetadataStore {
    #[cfg(not(any(test, feature = "test-support")))]
    pub fn init_global(cx: &mut App) {
        if cx.has_global::<GlobalTerminalThreadMetadataStore>() {
            return;
        }

        let db = TerminalThreadMetadataDb::global(cx);
        let terminal_store = cx.new(|cx| Self::new(db, cx));
        cx.set_global(GlobalTerminalThreadMetadataStore(terminal_store));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn init_global(cx: &mut App) {
        let db_name = TestTerminalMetadataDbName::global(cx);
        let db = gpui::block_on(db::open_test_db::<TerminalThreadMetadataDb>(&db_name));
        let terminal_store = cx.new(|cx| Self::new(TerminalThreadMetadataDb(db), cx));
        cx.set_global(GlobalTerminalThreadMetadataStore(terminal_store));
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalTerminalThreadMetadataStore>()
            .map(|store| store.0.clone())
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalTerminalThreadMetadataStore>().0.clone()
    }

    pub fn entry(&self, terminal_id: TerminalId) -> Option<&TerminalThreadMetadata> {
        self.terminals.get(&terminal_id)
    }

    pub fn entries(&self) -> impl Iterator<Item = &TerminalThreadMetadata> + '_ {
        self.terminals.values()
    }

    pub fn reload_task(&self) -> Shared<Task<()>> {
        self.reload_task
            .clone()
            .unwrap_or_else(|| Task::ready(()).shared())
    }

    pub fn entries_for_path<'a>(
        &'a self,
        path_list: &PathList,
        remote_connection: Option<&'a RemoteConnectionOptions>,
    ) -> impl Iterator<Item = &'a TerminalThreadMetadata> + 'a {
        self.terminals_by_paths
            .get(path_list)
            .into_iter()
            .flatten()
            .filter_map(|id| self.terminals.get(id))
            .filter(move |terminal| {
                same_remote_connection_identity(
                    terminal.remote_connection.as_ref(),
                    remote_connection,
                )
            })
    }

    pub fn entries_for_main_worktree_path<'a>(
        &'a self,
        path_list: &PathList,
        remote_connection: Option<&'a RemoteConnectionOptions>,
    ) -> impl Iterator<Item = &'a TerminalThreadMetadata> + 'a {
        self.terminals_by_main_paths
            .get(path_list)
            .into_iter()
            .flatten()
            .filter_map(|id| self.terminals.get(id))
            .filter(move |terminal| {
                same_remote_connection_identity(
                    terminal.remote_connection.as_ref(),
                    remote_connection,
                )
            })
    }

    pub fn path_is_referenced_by_terminal(
        &self,
        terminal_id: Option<TerminalId>,
        path: &Path,
        remote_connection: Option<&RemoteConnectionOptions>,
    ) -> bool {
        self.entries().any(|terminal| {
            Some(terminal.terminal_id) != terminal_id
                && same_remote_connection_identity(
                    terminal.remote_connection.as_ref(),
                    remote_connection,
                )
                && terminal
                    .folder_paths()
                    .paths()
                    .iter()
                    .any(|folder_path| folder_path.as_path() == path)
        })
    }

    pub fn save(&mut self, metadata: TerminalThreadMetadata, cx: &mut Context<Self>) {
        self.save_internal(metadata);
        cx.notify();
    }

    pub fn mark_stopping(&mut self, terminal_id: TerminalId, cx: &mut Context<Self>) -> bool {
        let Some(mut metadata) = self.terminals.get(&terminal_id).cloned() else {
            return false;
        };
        if metadata.registry.lifecycle_state == TerminalThreadLifecycleState::Stopping {
            return true;
        }
        metadata.registry.mark_stopping();
        self.save_internal(metadata);
        cx.notify();
        true
    }

    pub fn change_worktree_paths(
        &mut self,
        current_folder_paths: &PathList,
        remote_connection: Option<&RemoteConnectionOptions>,
        mutate: impl Fn(&mut WorktreePaths),
        cx: &mut Context<Self>,
    ) {
        let terminal_ids: Vec<_> = self
            .terminals_by_paths
            .get(current_folder_paths)
            .into_iter()
            .flatten()
            .filter(|id| {
                self.terminals.get(id).is_some_and(|terminal| {
                    same_remote_connection_identity(
                        terminal.remote_connection.as_ref(),
                        remote_connection,
                    )
                })
            })
            .copied()
            .collect();

        if terminal_ids.is_empty() {
            return;
        }

        for terminal_id in terminal_ids {
            if let Some(mut terminal) = self.terminals.get(&terminal_id).cloned() {
                mutate(&mut terminal.worktree_paths);
                self.save_internal(terminal);
            }
        }

        cx.notify();
    }

    fn save_internal(&mut self, mut metadata: TerminalThreadMetadata) {
        if let Some(existing) = self.terminals.get(&metadata.terminal_id) {
            if existing.registry.lifecycle_state == TerminalThreadLifecycleState::Stopping {
                metadata.registry.lifecycle_state = TerminalThreadLifecycleState::Stopping;
                metadata.registry.updated_at = metadata
                    .registry
                    .updated_at
                    .max(existing.registry.updated_at);
            }

            if existing.folder_paths() != metadata.folder_paths()
                && let Some(ids) = self.terminals_by_paths.get_mut(existing.folder_paths())
            {
                ids.remove(&metadata.terminal_id);
            }

            if existing.main_worktree_paths() != metadata.main_worktree_paths()
                && let Some(ids) = self
                    .terminals_by_main_paths
                    .get_mut(existing.main_worktree_paths())
            {
                ids.remove(&metadata.terminal_id);
            }
        }

        self.cache_terminal_metadata(metadata.clone());
        self.pending_terminal_ops_tx
            .try_send(DbOperation::Upsert(metadata))
            .log_err();
    }

    fn cache_terminal_metadata(&mut self, metadata: TerminalThreadMetadata) {
        self.terminals
            .insert(metadata.terminal_id, metadata.clone());

        self.terminals_by_paths
            .entry(metadata.folder_paths().clone())
            .or_default()
            .insert(metadata.terminal_id);

        if !metadata.main_worktree_paths().is_empty() {
            self.terminals_by_main_paths
                .entry(metadata.main_worktree_paths().clone())
                .or_default()
                .insert(metadata.terminal_id);
        }
    }

    pub fn delete(&mut self, terminal_id: TerminalId, cx: &mut Context<Self>) {
        if let Some(terminal) = self.terminals.remove(&terminal_id) {
            if let Some(ids) = self.terminals_by_paths.get_mut(terminal.folder_paths()) {
                ids.remove(&terminal_id);
            }
            if !terminal.main_worktree_paths().is_empty()
                && let Some(ids) = self
                    .terminals_by_main_paths
                    .get_mut(terminal.main_worktree_paths())
            {
                ids.remove(&terminal_id);
            }
        }
        self.pending_terminal_ops_tx
            .try_send(DbOperation::Delete(terminal_id))
            .log_err();
        cx.notify();
    }

    fn new(db: TerminalThreadMetadataDb, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::unbounded();
        let _db_operations_task = cx.background_spawn({
            let db = db.clone();
            async move {
                while let Ok(first_update) = rx.recv().await {
                    let mut updates = vec![first_update];
                    while let Ok(update) = rx.try_recv() {
                        updates.push(update);
                    }
                    let updates = Self::dedup_db_operations(updates);
                    for operation in updates {
                        match operation {
                            DbOperation::Upsert(metadata) => {
                                db.save(metadata).await.log_err();
                            }
                            DbOperation::Delete(terminal_id) => {
                                db.delete(terminal_id).await.log_err();
                            }
                        }
                    }
                }
            }
        });

        let mut this = Self {
            db,
            terminals: HashMap::default(),
            terminals_by_paths: HashMap::default(),
            terminals_by_main_paths: HashMap::default(),
            reload_task: None,
            pending_terminal_ops_tx: tx,
            _db_operations_task,
        };
        this.reload(cx);
        this
    }

    fn dedup_db_operations(operations: Vec<DbOperation>) -> Vec<DbOperation> {
        let mut ops = HashMap::default();
        for operation in operations.into_iter().rev() {
            if ops.contains_key(&operation.id()) {
                continue;
            }
            ops.insert(operation.id(), operation);
        }
        ops.into_values().collect()
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let db = self.db.clone();
        self.reload_task = Some(
            cx.spawn(async move |this, cx| {
                let rows = cx
                    .background_spawn(async move {
                        db.list()
                            .context("Failed to fetch terminal thread metadata")
                    })
                    .await
                    .log_err()
                    .unwrap_or_default();

                this.update(cx, |this, cx| {
                    this.terminals.clear();
                    this.terminals_by_paths.clear();
                    this.terminals_by_main_paths.clear();

                    for row in rows {
                        this.cache_terminal_metadata(row);
                    }

                    cx.notify();
                })
                .ok();
            })
            .shared(),
        );
    }
}

struct TerminalThreadMetadataDb(ThreadSafeConnection);

impl Domain for TerminalThreadMetadataDb {
    const NAME: &str = stringify!(TerminalThreadMetadataDb);

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE IF NOT EXISTS sidebar_terminal_threads(
                terminal_id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                custom_title TEXT,
                created_at TEXT NOT NULL,
                working_directory TEXT,
                folder_paths TEXT,
                folder_paths_order TEXT,
                main_worktree_paths TEXT,
                main_worktree_paths_order TEXT,
                remote_connection TEXT
            ) STRICT;
        ),
        sql!(
            ALTER TABLE sidebar_terminal_threads
            ADD COLUMN initial_command TEXT;
        ),
        sql!(
            ALTER TABLE sidebar_terminal_threads
            ADD COLUMN terminal_kind TEXT NOT NULL DEFAULT "shell"
            CHECK(terminal_kind IN ("agent", "shell"));
        ),
        sql!(
            ALTER TABLE sidebar_terminal_threads
            ADD COLUMN provider TEXT
            CHECK(provider IS NULL OR provider IN ("codex"));
        ),
        sql!(
            ALTER TABLE sidebar_terminal_threads
            ADD COLUMN tmux_server_name TEXT NOT NULL DEFAULT "zed-terminal-threads";
        ),
        sql!(
            ALTER TABLE sidebar_terminal_threads
            ADD COLUMN tmux_session_name TEXT;
        ),
        sql!(
            ALTER TABLE sidebar_terminal_threads
            ADD COLUMN provider_session_id TEXT;
        ),
        sql!(
            ALTER TABLE sidebar_terminal_threads
            ADD COLUMN lifecycle_state TEXT NOT NULL DEFAULT "detached"
            CHECK(lifecycle_state IN ("attached", "detached", "stopping"));
        ),
        sql!(
            ALTER TABLE sidebar_terminal_threads
            ADD COLUMN last_attached_at TEXT;
        ),
        sql!(
            ALTER TABLE sidebar_terminal_threads
            ADD COLUMN updated_at TEXT;
        ),
        sql!(
            UPDATE sidebar_terminal_threads
            SET terminal_kind = "agent", provider = "codex"
            WHERE initial_command = "codex"
               OR (initial_command IS NULL AND custom_title = "Codex");
        ),
        sql!(
            UPDATE sidebar_terminal_threads
            SET tmux_session_name = "zed-" || terminal_id,
                updated_at = created_at
            WHERE tmux_session_name IS NULL OR updated_at IS NULL;
        ),
    ];
}

db::static_connection!(TerminalThreadMetadataDb, []);

impl TerminalThreadMetadataDb {
    pub fn list(&self) -> anyhow::Result<Vec<TerminalThreadMetadata>> {
        self.select::<TerminalThreadMetadata>(
            "SELECT terminal_id, title, custom_title, created_at, \
            working_directory, folder_paths, folder_paths_order, main_worktree_paths, \
            main_worktree_paths_order, remote_connection, initial_command, terminal_kind, \
            provider, tmux_server_name, tmux_session_name, provider_session_id, lifecycle_state, \
            last_attached_at, updated_at \
            FROM sidebar_terminal_threads \
            ORDER BY created_at DESC",
        )?()
    }

    pub async fn save(&self, row: TerminalThreadMetadata) -> anyhow::Result<()> {
        let terminal_id = row.terminal_id.to_key_string();
        let title = row.title.to_string();
        let custom_title = row.custom_title.as_ref().map(ToString::to_string);
        let created_at = row.created_at.to_rfc3339();
        let working_directory = row
            .working_directory
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let serialized = row.folder_paths().serialize();
        let (folder_paths, folder_paths_order) = if row.folder_paths().is_empty() {
            (None, None)
        } else {
            (Some(serialized.paths), Some(serialized.order))
        };
        let main_serialized = row.main_worktree_paths().serialize();
        let (main_worktree_paths, main_worktree_paths_order) =
            if row.main_worktree_paths().is_empty() {
                (None, None)
            } else {
                (Some(main_serialized.paths), Some(main_serialized.order))
            };
        let remote_connection = row
            .remote_connection
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("serialize terminal thread remote connection")?;
        let initial_command = row.initial_command;
        let terminal_kind = row.registry.kind.as_str();
        let provider = row.registry.provider.map(TerminalThreadProvider::as_str);
        let tmux_server_name = row.registry.tmux_server_name;
        let tmux_session_name = row.registry.tmux_session_name;
        let provider_session_id = row.registry.provider_session_id;
        let lifecycle_state = row.registry.lifecycle_state.as_str();
        let last_attached_at = row
            .registry
            .last_attached_at
            .map(|timestamp| timestamp.to_rfc3339());
        let updated_at = row.registry.updated_at.to_rfc3339();

        self.write(move |conn| {
            let sql = "INSERT INTO sidebar_terminal_threads(terminal_id, title, custom_title, created_at, working_directory, folder_paths, folder_paths_order, main_worktree_paths, main_worktree_paths_order, remote_connection, initial_command, terminal_kind, provider, tmux_server_name, tmux_session_name, provider_session_id, lifecycle_state, last_attached_at, updated_at) \
                       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19) \
                       ON CONFLICT(terminal_id) DO UPDATE SET \
                           title = excluded.title, \
                           custom_title = excluded.custom_title, \
                           created_at = excluded.created_at, \
                           working_directory = excluded.working_directory, \
                           folder_paths = excluded.folder_paths, \
                           folder_paths_order = excluded.folder_paths_order, \
                           main_worktree_paths = excluded.main_worktree_paths, \
                           main_worktree_paths_order = excluded.main_worktree_paths_order, \
                           remote_connection = excluded.remote_connection, \
                           initial_command = excluded.initial_command, \
                           terminal_kind = excluded.terminal_kind, \
                           provider = excluded.provider, \
                           tmux_server_name = excluded.tmux_server_name, \
                           tmux_session_name = excluded.tmux_session_name, \
                           provider_session_id = excluded.provider_session_id, \
                           lifecycle_state = excluded.lifecycle_state, \
                           last_attached_at = excluded.last_attached_at, \
                           updated_at = excluded.updated_at";
            let mut stmt = Statement::prepare(conn, sql)?;
            let mut i = stmt.bind(&terminal_id, 1)?;
            i = stmt.bind(&title, i)?;
            i = stmt.bind(&custom_title, i)?;
            i = stmt.bind(&created_at, i)?;
            i = stmt.bind(&working_directory, i)?;
            i = stmt.bind(&folder_paths, i)?;
            i = stmt.bind(&folder_paths_order, i)?;
            i = stmt.bind(&main_worktree_paths, i)?;
            i = stmt.bind(&main_worktree_paths_order, i)?;
            i = stmt.bind(&remote_connection, i)?;
            i = stmt.bind(&initial_command, i)?;
            i = stmt.bind(&terminal_kind, i)?;
            i = stmt.bind(&provider, i)?;
            i = stmt.bind(&tmux_server_name, i)?;
            i = stmt.bind(&tmux_session_name, i)?;
            i = stmt.bind(&provider_session_id, i)?;
            i = stmt.bind(&lifecycle_state, i)?;
            i = stmt.bind(&last_attached_at, i)?;
            stmt.bind(&updated_at, i)?;
            stmt.exec()
        })
        .await
    }

    pub async fn delete(&self, terminal_id: TerminalId) -> anyhow::Result<()> {
        let terminal_id = terminal_id.to_key_string();
        self.write(move |conn| {
            let mut stmt = Statement::prepare(
                conn,
                "DELETE FROM sidebar_terminal_threads WHERE terminal_id = ?",
            )?;
            stmt.bind(&terminal_id, 1)?;
            stmt.exec()
        })
        .await
    }
}

impl Column for TerminalThreadMetadata {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (terminal_id, next): (String, i32) = Column::column(statement, start_index)?;
        let (title, next): (String, i32) = Column::column(statement, next)?;
        let (custom_title, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (created_at, next): (String, i32) = Column::column(statement, next)?;
        let (working_directory, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (folder_paths_str, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (folder_paths_order_str, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (main_worktree_paths_str, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (main_worktree_paths_order_str, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (remote_connection_json, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (initial_command, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (terminal_kind, next): (String, i32) = Column::column(statement, next)?;
        let (provider, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (tmux_server_name, next): (String, i32) = Column::column(statement, next)?;
        let (tmux_session_name, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (provider_session_id, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (lifecycle_state, next): (String, i32) = Column::column(statement, next)?;
        let (last_attached_at, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (updated_at, next): (Option<String>, i32) = Column::column(statement, next)?;

        let folder_paths = folder_paths_str
            .map(|paths| {
                PathList::deserialize(&util::path_list::SerializedPathList {
                    paths,
                    order: folder_paths_order_str.unwrap_or_default(),
                })
            })
            .unwrap_or_default();

        let main_worktree_paths = main_worktree_paths_str
            .map(|paths| {
                PathList::deserialize(&util::path_list::SerializedPathList {
                    paths,
                    order: main_worktree_paths_order_str.unwrap_or_default(),
                })
            })
            .unwrap_or_default();

        let remote_connection = remote_connection_json
            .as_deref()
            .map(serde_json::from_str::<RemoteConnectionOptions>)
            .transpose()
            .context("deserialize terminal thread remote connection")?;

        let worktree_paths = WorktreePaths::from_path_lists(main_worktree_paths, folder_paths)
            .unwrap_or_else(|_| WorktreePaths::default());

        let terminal_id = TerminalId::from_key_string(&terminal_id)?;
        let created_at = DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc);
        let registry = TerminalThreadRegistryMetadata {
            kind: TerminalThreadKind::from_str(&terminal_kind)?,
            provider: provider
                .as_deref()
                .map(TerminalThreadProvider::from_str)
                .transpose()?,
            tmux_server_name,
            tmux_session_name: tmux_session_name
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| terminal_thread_tmux_session_name(terminal_id)),
            provider_session_id,
            lifecycle_state: TerminalThreadLifecycleState::from_str(&lifecycle_state)?,
            last_attached_at: last_attached_at
                .as_deref()
                .map(DateTime::parse_from_rfc3339)
                .transpose()?
                .map(|timestamp| timestamp.with_timezone(&Utc)),
            updated_at: updated_at
                .as_deref()
                .map(DateTime::parse_from_rfc3339)
                .transpose()?
                .map(|timestamp| timestamp.with_timezone(&Utc))
                .unwrap_or(created_at),
        };

        Ok((
            TerminalThreadMetadata {
                terminal_id,
                title: SharedString::from(title),
                custom_title: custom_title
                    .filter(|title| !title.trim().is_empty())
                    .map(SharedString::from),
                created_at,
                worktree_paths,
                remote_connection,
                working_directory: working_directory.map(PathBuf::from),
                initial_command,
                registry,
            },
            next,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use db::sqlez::{connection::Connection, domain::Migrator};
    use gpui::TestAppContext;
    use std::path::Path;

    struct LegacyTerminalThreadMetadataDb;

    impl Migrator for LegacyTerminalThreadMetadataDb {
        fn migrate(connection: &Connection) -> anyhow::Result<()> {
            connection.migrate(
                TerminalThreadMetadataDb::NAME,
                &TerminalThreadMetadataDb::MIGRATIONS[..2],
                &mut |_, _, _| false,
            )
        }
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            TerminalThreadMetadataStore::init_global(cx);
        });
        cx.run_until_parked();
    }

    fn metadata(title: &str, worktree_paths: WorktreePaths) -> TerminalThreadMetadata {
        let now = Utc::now();
        let terminal_id = TerminalId::new();
        TerminalThreadMetadata {
            terminal_id,
            title: SharedString::from(title.to_string()),
            custom_title: None,
            created_at: now,
            worktree_paths,
            remote_connection: None,
            working_directory: None,
            initial_command: None,
            registry: TerminalThreadRegistryMetadata::detached(
                terminal_id,
                TerminalThreadKind::Shell,
                None,
            ),
        }
    }

    #[test]
    fn infers_codex_provider_from_launch_metadata() {
        assert_eq!(
            TerminalThreadProvider::from_launch_metadata(Some("codex"), None),
            Some(TerminalThreadProvider::Codex)
        );
        assert_eq!(
            TerminalThreadProvider::from_launch_metadata(
                Some("/opt/homebrew/bin/codex --sandbox read-only"),
                None
            ),
            Some(TerminalThreadProvider::Codex)
        );
        assert_eq!(
            TerminalThreadProvider::from_launch_metadata(None, Some("Codex")),
            Some(TerminalThreadProvider::Codex)
        );
        assert_eq!(
            TerminalThreadProvider::from_launch_metadata(Some("fish"), Some("Codex")),
            None
        );
        assert_eq!(
            TerminalThreadProvider::from_launch_metadata(Some("echo codex"), None),
            None
        );
    }

    #[test]
    fn test_terminal_title_prefix_preserves_non_alphanumeric_prefixes() {
        assert_eq!(terminal_title_prefix("✳ Thinking"), Some("✳ "));
        assert_eq!(terminal_title_prefix(">>>   Thinking"), Some(">>>   "));
        assert_eq!(terminal_title_prefix("⠋ Running"), Some("⠋ "));
        assert_eq!(terminal_title_prefix("* Claude"), Some("* "));
        assert_eq!(terminal_title_prefix("✳Thinking"), None);
        assert_eq!(terminal_title_prefix("Thinking"), None);
        assert_eq!(terminal_title_prefix(" Thinking"), None);
        assert_eq!(terminal_title_prefix("✳"), None);
        assert_eq!(terminal_title_prefix("v1 Running"), None);
    }

    #[test]
    fn test_terminal_title_for_persistence_removes_live_activity() {
        assert_eq!(terminal_title_for_persistence("⠋ zed").as_ref(), "zed");
        assert_eq!(
            terminal_title_for_persistence("[ ! ] Action Required | zed").as_ref(),
            "zed"
        );
        assert_eq!(
            terminal_title_for_persistence("[ . ] Action Required | zed").as_ref(),
            "zed"
        );
        assert_eq!(terminal_title_for_persistence("zed").as_ref(), "zed");
    }

    #[test]
    fn test_terminal_thread_display_title_combines_raw_and_custom_titles() {
        let mut metadata = metadata(
            "⠋ Thinking",
            WorktreePaths::from_folder_paths(&PathList::default()),
        );
        metadata.custom_title = Some("Fix bug".into());
        assert_eq!(metadata.display_title().as_ref(), "⠋ Fix bug");

        metadata.title = "Thinking".into();
        assert_eq!(metadata.display_title().as_ref(), "Fix bug");
    }

    #[gpui::test]
    async fn test_durable_registry_fields_round_trip_through_database(cx: &mut TestAppContext) {
        init_test(cx);
        let mut metadata = metadata(
            "Codex",
            WorktreePaths::from_folder_paths(&PathList::default()),
        );
        metadata.initial_command = Some("codex --profile work".to_string());
        metadata.registry = TerminalThreadRegistryMetadata::for_launch(
            metadata.terminal_id,
            metadata.initial_command.as_deref(),
            metadata.custom_title.as_deref(),
        );
        metadata.registry.provider_session_id = Some("provider-session-1".to_string());
        metadata.registry.mark_attached();
        let expected_registry = metadata.registry.clone();
        let terminal_id = metadata.terminal_id;

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(metadata, cx);
            });
        });
        cx.run_until_parked();

        cx.update(|cx| {
            let rows = TerminalThreadMetadataStore::global(cx)
                .read(cx)
                .db
                .list()
                .expect("terminal metadata should load");
            let row = rows
                .into_iter()
                .find(|row| row.terminal_id == terminal_id)
                .expect("saved terminal metadata should exist");
            assert_eq!(row.initial_command.as_deref(), Some("codex --profile work"));
            assert_eq!(row.registry, expected_registry);
        });
    }

    #[gpui::test]
    async fn test_legacy_codex_row_migrates_to_detached_registry(cx: &mut TestAppContext) {
        let terminal_id = TerminalId::new();
        let created_at = Utc::now();
        let database_name = format!("legacy-terminal-registry-{terminal_id:?}");
        let legacy_db = db::open_test_db::<LegacyTerminalThreadMetadataDb>(&database_name).await;
        let terminal_id_string = terminal_id.to_key_string();
        let created_at_string = created_at.to_rfc3339();
        legacy_db
            .write(move |connection| {
                let mut statement = Statement::prepare(
                    connection,
                    "INSERT INTO sidebar_terminal_threads(terminal_id, title, custom_title, created_at, initial_command) VALUES (?1, ?2, ?3, ?4, ?5)",
                )?;
                let mut index = statement.bind(&terminal_id_string, 1)?;
                index = statement.bind(&"Codex", index)?;
                index = statement.bind(&Some("Codex"), index)?;
                index = statement.bind(&created_at_string, index)?;
                statement.bind(&Some("codex"), index)?;
                statement.exec()
            })
            .await
            .expect("legacy terminal metadata should be inserted");

        let migrated_db = db::open_test_db::<TerminalThreadMetadataDb>(&database_name).await;
        let rows = TerminalThreadMetadataDb(migrated_db)
            .list()
            .expect("migrated terminal metadata should load");
        let metadata = rows
            .into_iter()
            .find(|row| row.terminal_id == terminal_id)
            .expect("legacy terminal metadata should survive migration");

        assert_eq!(metadata.registry.kind, TerminalThreadKind::Agent);
        assert_eq!(
            metadata.registry.provider,
            Some(TerminalThreadProvider::Codex)
        );
        assert_eq!(
            metadata.registry.tmux_server_name,
            TERMINAL_THREAD_TMUX_SERVER_NAME
        );
        assert_eq!(
            metadata.registry.tmux_session_name,
            terminal_thread_tmux_session_name(terminal_id)
        );
        assert_eq!(
            metadata.registry.lifecycle_state,
            TerminalThreadLifecycleState::Detached
        );
        assert_eq!(metadata.registry.last_attached_at, None);
        assert_eq!(metadata.registry.updated_at, created_at);

        drop(legacy_db);
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_mark_stopping_persists_lifecycle_intent(cx: &mut TestAppContext) {
        init_test(cx);
        let metadata = metadata(
            "Codex",
            WorktreePaths::from_folder_paths(&PathList::default()),
        );
        let terminal_id = metadata.terminal_id;
        let mut stale_attached_metadata = metadata.clone();
        stale_attached_metadata.registry.mark_attached();

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(metadata, cx);
                assert!(store.mark_stopping(terminal_id, cx));
                store.save(stale_attached_metadata, cx);
            });
        });
        cx.run_until_parked();

        cx.update(|cx| {
            let store = TerminalThreadMetadataStore::global(cx);
            assert_eq!(
                store
                    .read(cx)
                    .entry(terminal_id)
                    .expect("terminal metadata should exist")
                    .registry
                    .lifecycle_state,
                TerminalThreadLifecycleState::Stopping
            );
            let row = store
                .read(cx)
                .db
                .list()
                .expect("terminal metadata should load")
                .into_iter()
                .find(|row| row.terminal_id == terminal_id)
                .expect("saved terminal metadata should exist");
            assert_eq!(
                row.registry.lifecycle_state,
                TerminalThreadLifecycleState::Stopping
            );
        });
    }

    #[gpui::test]
    async fn test_change_worktree_paths_reindexes_terminal_metadata(cx: &mut TestAppContext) {
        init_test(cx);

        let old_main_paths = PathList::new(&[Path::new("/repo")]);
        let old_folder_paths = PathList::new(&[Path::new("/repo-feature")]);
        let new_main_path = Path::new("/repo");
        let new_folder_path = Path::new("/repo-feature-renamed");
        let new_folder_paths = PathList::new(&[new_folder_path]);
        let metadata = metadata(
            "Dev Server",
            WorktreePaths::from_path_lists(old_main_paths.clone(), old_folder_paths.clone())
                .unwrap(),
        );
        let terminal_id = metadata.terminal_id;

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(metadata, cx);
            });
        });

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.change_worktree_paths(
                    &old_folder_paths,
                    None,
                    |paths| {
                        paths.add_path(new_main_path, new_folder_path);
                        paths.remove_folder_path(Path::new("/repo-feature"));
                    },
                    cx,
                );
            });
        });

        cx.update(|cx| {
            let store = TerminalThreadMetadataStore::global(cx);
            let store = store.read(cx);
            assert!(
                store
                    .entries_for_path(&old_folder_paths, None)
                    .next()
                    .is_none()
            );
            assert_eq!(
                store
                    .entries_for_path(&new_folder_paths, None)
                    .map(|entry| entry.terminal_id)
                    .collect::<Vec<_>>(),
                vec![terminal_id]
            );
            assert_eq!(
                store
                    .entry(terminal_id)
                    .unwrap()
                    .main_worktree_paths()
                    .paths(),
                old_main_paths.paths()
            );
        });
    }
}
