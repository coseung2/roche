use std::collections::HashMap;

use crate::{
    herdr::HerdrSessionSnapshot,
    models::{ProjectId, Session, SessionId},
    sidebar,
};

#[derive(Debug, Default)]
pub struct AppState {
    pub sessions: HashMap<String, Session>,
}

impl AppState {
    pub fn apply_snapshot(&mut self, project_id: ProjectId, snapshot: Vec<HerdrSessionSnapshot>) {
        self.sessions.clear();

        for item in snapshot {
            self.sessions.insert(
                item.pane_id.clone(),
                Session {
                    id: SessionId::new(),
                    project_id,
                    task_id: None,
                    agent_name: item.agent_name,
                    runtime_status: item.status,
                    worktree_path: item.worktree_path,
                    recent_activity: item.recent_activity,
                    changed_files: 0,
                    attention_required: false,
                    updated_at: item.updated_at,
                },
            );
        }
    }

    pub fn sidebar_sessions(&self) -> Vec<Session> {
        let mut sessions = self.sessions.values().cloned().collect::<Vec<_>>();
        sidebar::sort_sessions(&mut sessions);
        sessions
    }
}
