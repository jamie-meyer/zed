use std::{
    path::{Path, PathBuf},
    rc::Rc,
};

use chrono::{DateTime, Utc};
use collections::HashMap;
use gpui::{App, AppContext as _, Context, Entity, EntityId, Global, SharedString, Window};
use project::ProjectGroupKey;

pub type FocusWorktreeActivitySource = Rc<dyn Fn(&mut Window, &mut App) -> bool + 'static>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorktreeActivityStatus {
    Running,
    Working,
    Finished,
    Attention,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WorktreeActivityKey {
    pub project_group_key: ProjectGroupKey,
    pub worktree_path: PathBuf,
}

#[derive(Clone)]
pub struct WorktreeActivity {
    pub source_id: EntityId,
    pub key: WorktreeActivityKey,
    pub agent_label: SharedString,
    pub source_label: SharedString,
    pub status: WorktreeActivityStatus,
    pub updated_at: DateTime<Utc>,
    pub acknowledged: bool,
    pub focus_source: FocusWorktreeActivitySource,
}

impl WorktreeActivity {
    pub fn tooltip(&self) -> String {
        match self.status {
            WorktreeActivityStatus::Running => format!("{} running in terminal", self.agent_label),
            WorktreeActivityStatus::Working => format!("{} working in terminal", self.agent_label),
            WorktreeActivityStatus::Finished => {
                format!("{} finished in terminal", self.agent_label)
            }
            WorktreeActivityStatus::Attention => {
                format!("{} needs attention", self.agent_label)
            }
        }
    }
}

pub struct WorktreeActivityUpdate {
    pub source_id: EntityId,
    pub key: WorktreeActivityKey,
    pub agent_label: SharedString,
    pub source_label: SharedString,
    pub status: WorktreeActivityStatus,
    pub focus_source: FocusWorktreeActivitySource,
}

pub struct WorktreeActivityStore {
    activities_by_source: HashMap<EntityId, WorktreeActivity>,
}

struct GlobalWorktreeActivityStore(Entity<WorktreeActivityStore>);

impl Global for GlobalWorktreeActivityStore {}

impl WorktreeActivityStore {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(store) = cx.try_global::<GlobalWorktreeActivityStore>() {
            store.0.clone()
        } else {
            let store = cx.new(|_| Self {
                activities_by_source: HashMap::default(),
            });
            cx.set_global(GlobalWorktreeActivityStore(store.clone()));
            store
        }
    }

    pub fn update_activity(&mut self, update: WorktreeActivityUpdate, cx: &mut Context<Self>) {
        let mut changed = true;
        if let Some(activity) = self.activities_by_source.get_mut(&update.source_id) {
            changed = activity.key != update.key
                || activity.agent_label != update.agent_label
                || activity.source_label != update.source_label
                || activity.status != update.status
                || activity.acknowledged;

            activity.key = update.key;
            activity.agent_label = update.agent_label;
            activity.source_label = update.source_label;
            activity.status = update.status;
            activity.acknowledged = false;
            activity.focus_source = update.focus_source;
            if changed {
                activity.updated_at = Utc::now();
            }
        } else {
            self.activities_by_source.insert(
                update.source_id,
                WorktreeActivity {
                    source_id: update.source_id,
                    key: update.key,
                    agent_label: update.agent_label,
                    source_label: update.source_label,
                    status: update.status,
                    updated_at: Utc::now(),
                    acknowledged: false,
                    focus_source: update.focus_source,
                },
            );
        }

        if changed {
            cx.notify();
        }
    }

    pub fn mark_running(&mut self, source_id: EntityId, cx: &mut Context<Self>) {
        let Some(activity) = self.activities_by_source.get_mut(&source_id) else {
            return;
        };

        if activity.status != WorktreeActivityStatus::Working {
            return;
        }

        activity.status = WorktreeActivityStatus::Running;
        cx.notify();
    }

    pub fn mark_finished(&mut self, source_id: EntityId, cx: &mut Context<Self>) {
        let Some(activity) = self.activities_by_source.get_mut(&source_id) else {
            return;
        };

        if activity.status == WorktreeActivityStatus::Finished && !activity.acknowledged {
            return;
        }

        activity.status = WorktreeActivityStatus::Finished;
        activity.acknowledged = false;
        activity.updated_at = Utc::now();
        cx.notify();
    }

    pub fn mark_attention(&mut self, source_id: EntityId, cx: &mut Context<Self>) {
        let Some(activity) = self.activities_by_source.get_mut(&source_id) else {
            return;
        };

        if activity.status == WorktreeActivityStatus::Attention && !activity.acknowledged {
            return;
        }

        activity.status = WorktreeActivityStatus::Attention;
        activity.acknowledged = false;
        activity.updated_at = Utc::now();
        cx.notify();
    }

    pub fn acknowledge_source(
        &mut self,
        source_id: EntityId,
        still_running: bool,
        cx: &mut Context<Self>,
    ) {
        if still_running {
            let Some(activity) = self.activities_by_source.get_mut(&source_id) else {
                return;
            };

            if matches!(
                activity.status,
                WorktreeActivityStatus::Running | WorktreeActivityStatus::Working
            ) && !activity.acknowledged
            {
                return;
            }

            activity.status = WorktreeActivityStatus::Running;
            activity.acknowledged = false;
            activity.updated_at = Utc::now();
            cx.notify();
        } else if self.activities_by_source.remove(&source_id).is_some() {
            cx.notify();
        }
    }

    pub fn remove_source(&mut self, source_id: EntityId, cx: &mut Context<Self>) {
        if self.activities_by_source.remove(&source_id).is_some() {
            cx.notify();
        }
    }

    pub fn activity_for_worktree(
        &self,
        project_group_key: &ProjectGroupKey,
        worktree_path: &Path,
    ) -> Option<WorktreeActivity> {
        self.activities_for_worktree(project_group_key, worktree_path)
            .into_iter()
            .next()
    }

    pub fn activities_for_worktree(
        &self,
        project_group_key: &ProjectGroupKey,
        worktree_path: &Path,
    ) -> Vec<WorktreeActivity> {
        let mut activities = self
            .activities_by_source
            .values()
            .filter(|activity| {
                !activity.acknowledged
                    && activity.key.project_group_key == *project_group_key
                    && activity.key.worktree_path == worktree_path
            })
            .cloned()
            .collect::<Vec<_>>();
        activities.sort_by(|left, right| {
            activity_priority(right.status)
                .cmp(&activity_priority(left.status))
                .then_with(|| right.updated_at.cmp(&left.updated_at))
        });
        activities
    }
}

fn activity_priority(status: WorktreeActivityStatus) -> usize {
    match status {
        WorktreeActivityStatus::Running => 0,
        WorktreeActivityStatus::Working => 1,
        WorktreeActivityStatus::Finished => 2,
        WorktreeActivityStatus::Attention => 3,
    }
}
