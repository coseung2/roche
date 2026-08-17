use crate::models::{AgentRuntimeStatus, Session};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SidebarPriority {
    NeedsAttention,
    Failed,
    Working,
    Verifying,
    Idle,
    Done,
}

pub fn priority(session: &Session) -> SidebarPriority {
    if session.attention_required || session.runtime_status == AgentRuntimeStatus::Blocked {
        return SidebarPriority::NeedsAttention;
    }

    match session.runtime_status {
        AgentRuntimeStatus::Working => SidebarPriority::Working,
        AgentRuntimeStatus::Idle | AgentRuntimeStatus::Unknown => SidebarPriority::Idle,
        AgentRuntimeStatus::Done => SidebarPriority::Done,
        AgentRuntimeStatus::Blocked => SidebarPriority::NeedsAttention,
    }
}

pub fn sort_sessions(sessions: &mut [Session]) {
    sessions.sort_by(|a, b| {
        priority(a)
            .cmp(&priority(b))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.agent_name.cmp(&b.agent_name))
    });
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::models::{ProjectId, SessionId};

    fn session(name: &str, status: AgentRuntimeStatus, attention_required: bool) -> Session {
        Session {
            id: SessionId::new(),
            project_id: ProjectId::new(),
            task_id: None,
            agent_name: name.into(),
            runtime_status: status,
            worktree_path: None,
            recent_activity: None,
            changed_files: 0,
            attention_required,
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn blocked_sessions_sort_before_working_sessions() {
        let mut sessions = vec![
            session("worker", AgentRuntimeStatus::Working, false),
            session("blocked", AgentRuntimeStatus::Blocked, false),
        ];

        sort_sessions(&mut sessions);
        assert_eq!(sessions[0].agent_name, "blocked");
    }
}
