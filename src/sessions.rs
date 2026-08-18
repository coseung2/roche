use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRuntime {
    Unified,
    WebGpt,
    Codex,
}

impl SessionRuntime {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unified => "Unified",
            Self::WebGpt => "[WEB] GPT-5.6 Sol",
            Self::Codex => "Codex",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Running,
    WaitingOnWorkers,
    NeedsInput,
    Completed,
    Failed,
    Cancelled,
    Offline,
}

impl SessionStatus {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Idle | Self::Running | Self::WaitingOnWorkers | Self::NeedsInput
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: String,
    pub project_key: String,
    pub title: String,
    pub runtime: SessionRuntime,
    pub status: SessionStatus,
    pub parent_session_id: Option<String>,
    pub root_session_id: String,
    pub depth: u32,
    pub created_by_session_id: Option<String>,
    pub worker_ids: Vec<String>,
    pub created_at_ms: u128,
    pub updated_at_ms: u128,
}

impl AgentSession {
    pub fn is_worker(&self) -> bool {
        self.parent_session_id.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGraphEvent {
    pub sequence: u64,
    pub session_id: String,
    pub kind: String,
    pub message: String,
    pub created_at_ms: u128,
}

#[derive(Debug, Default)]
pub struct SessionGraph {
    sessions: HashMap<String, AgentSession>,
    order: Vec<String>,
    events: Vec<SessionGraphEvent>,
    next_event_sequence: u64,
}

impl SessionGraph {
    pub fn new() -> Self {
        Self {
            next_event_sequence: 1,
            ..Self::default()
        }
    }

    pub fn create_root(
        &mut self,
        project_key: impl Into<String>,
        runtime: SessionRuntime,
        title: impl Into<String>,
    ) -> AgentSession {
        let id = next_session_id();
        let timestamp = now_ms();
        let session = AgentSession {
            id: id.clone(),
            project_key: project_key.into(),
            title: normalized_title(title.into(), runtime),
            runtime,
            status: SessionStatus::Idle,
            parent_session_id: None,
            root_session_id: id.clone(),
            depth: 0,
            created_by_session_id: None,
            worker_ids: Vec::new(),
            created_at_ms: timestamp,
            updated_at_ms: timestamp,
        };
        self.insert_session(session.clone());
        self.push_event(
            &id,
            "session.created",
            format!("Root {} session created", runtime.label()),
        );
        session
    }

    pub fn spawn_worker(
        &mut self,
        parent_session_id: &str,
        runtime: SessionRuntime,
        title: impl Into<String>,
    ) -> Result<AgentSession, String> {
        let parent = self
            .sessions
            .get(parent_session_id)
            .cloned()
            .ok_or_else(|| format!("Unknown parent session: {parent_session_id}"))?;
        if !parent.status.is_active() {
            return Err(format!(
                "Session {parent_session_id} is {:?}; inactive sessions cannot spawn workers",
                parent.status
            ));
        }

        let id = next_session_id();
        let timestamp = now_ms();
        let session = AgentSession {
            id: id.clone(),
            project_key: parent.project_key.clone(),
            title: normalized_title(title.into(), runtime),
            runtime,
            status: SessionStatus::Idle,
            parent_session_id: Some(parent.id.clone()),
            root_session_id: parent.root_session_id.clone(),
            depth: parent.depth.saturating_add(1),
            created_by_session_id: Some(parent.id.clone()),
            worker_ids: Vec::new(),
            created_at_ms: timestamp,
            updated_at_ms: timestamp,
        };
        self.insert_session(session.clone());
        self.sessions
            .get_mut(parent_session_id)
            .expect("parent session exists")
            .worker_ids
            .push(id.clone());
        self.push_event(
            &id,
            "session.worker_spawned",
            format!(
                "{} spawned {} worker {}",
                parent.runtime.label(),
                runtime.label(),
                id
            ),
        );
        Ok(session)
    }

    pub fn get(&self, session_id: &str) -> Option<&AgentSession> {
        self.sessions.get(session_id)
    }

    pub fn set_status(
        &mut self,
        session_id: &str,
        status: SessionStatus,
    ) -> Result<AgentSession, String> {
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| format!("Unknown session: {session_id}"))?;
        session.status = status;
        session.updated_at_ms = now_ms();
        let snapshot = session.clone();
        self.push_event(
            session_id,
            "session.status",
            format!("Session status changed to {status:?}"),
        );
        Ok(snapshot)
    }

    pub fn list_project(&self, project_key: &str) -> Vec<AgentSession> {
        self.order
            .iter()
            .filter_map(|id| self.sessions.get(id))
            .filter(|session| session.project_key == project_key)
            .cloned()
            .collect()
    }

    pub fn active_count(&self, project_key: &str) -> usize {
        self.sessions
            .values()
            .filter(|session| session.project_key == project_key && session.status.is_active())
            .count()
    }

    pub fn workers_of(&self, parent_session_id: &str) -> Result<Vec<AgentSession>, String> {
        let parent = self
            .sessions
            .get(parent_session_id)
            .ok_or_else(|| format!("Unknown session: {parent_session_id}"))?;
        Ok(parent
            .worker_ids
            .iter()
            .filter_map(|id| self.sessions.get(id))
            .cloned()
            .collect())
    }

    pub fn events_after(&self, sequence: u64) -> Vec<SessionGraphEvent> {
        self.events
            .iter()
            .filter(|event| event.sequence > sequence)
            .cloned()
            .collect()
    }

    fn insert_session(&mut self, session: AgentSession) {
        self.order.push(session.id.clone());
        self.sessions.insert(session.id.clone(), session);
    }

    fn push_event(&mut self, session_id: &str, kind: &str, message: String) {
        let sequence = self.next_event_sequence;
        self.next_event_sequence = self.next_event_sequence.saturating_add(1);
        self.events.push(SessionGraphEvent {
            sequence,
            session_id: session_id.to_owned(),
            kind: kind.to_owned(),
            message,
            created_at_ms: now_ms(),
        });
        const MAX_EVENTS: usize = 4096;
        if self.events.len() > MAX_EVENTS {
            let remove = self.events.len() - MAX_EVENTS;
            self.events.drain(0..remove);
        }
    }
}

fn normalized_title(title: String, runtime: SessionRuntime) -> String {
    let title = title.trim();
    if title.is_empty() {
        runtime.label().to_owned()
    } else {
        title.chars().take(80).collect()
    }
}

fn next_session_id() -> String {
    format!(
        "session-{}-{}",
        now_ms(),
        SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn either_runtime_can_spawn_the_other_recursively() {
        let mut graph = SessionGraph::new();
        let web = graph.create_root("project-a", SessionRuntime::WebGpt, "Main");
        let codex = graph
            .spawn_worker(&web.id, SessionRuntime::Codex, "Implementation")
            .unwrap();
        let reviewer = graph
            .spawn_worker(&codex.id, SessionRuntime::WebGpt, "Independent review")
            .unwrap();
        let fixer = graph
            .spawn_worker(&reviewer.id, SessionRuntime::Codex, "Revision")
            .unwrap();

        assert_eq!(codex.parent_session_id.as_deref(), Some(web.id.as_str()));
        assert_eq!(
            reviewer.parent_session_id.as_deref(),
            Some(codex.id.as_str())
        );
        assert_eq!(
            fixer.parent_session_id.as_deref(),
            Some(reviewer.id.as_str())
        );
        assert_eq!(fixer.root_session_id, web.id);
        assert_eq!(fixer.depth, 3);
        assert_eq!(graph.active_count("project-a"), 4);
    }

    #[test]
    fn inactive_session_cannot_spawn_new_workers() {
        let mut graph = SessionGraph::new();
        let root = graph.create_root("project-a", SessionRuntime::Codex, "Main");
        graph
            .set_status(&root.id, SessionStatus::Completed)
            .unwrap();
        let error = graph
            .spawn_worker(&root.id, SessionRuntime::WebGpt, "Too late")
            .unwrap_err();
        assert!(error.contains("inactive"));
    }
}
