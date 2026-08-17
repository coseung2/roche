use crate::models::AgentRuntimeStatus;

pub const STANDARD_MESSAGE_COUNT: usize = 100_000;
pub const STANDARD_TOOL_EVENT_COUNT: usize = 100_000;
pub const STANDARD_SESSION_COUNT: usize = 1_000;
pub const STANDARD_TERMINAL_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntheticActor {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticMessage {
    pub id: u64,
    pub actor: SyntheticActor,
    pub markdown: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntheticToolKind {
    Search,
    Read,
    Edit,
    Test,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticToolEvent {
    pub id: u64,
    pub kind: SyntheticToolKind,
    pub summary: String,
    pub payload_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticSession {
    pub id: u64,
    pub task_name: String,
    pub runtime_status: AgentRuntimeStatus,
    pub changed_files: usize,
    pub attention_required: bool,
    pub recent_activity: String,
}

#[derive(Debug, Clone)]
pub struct SyntheticWorkload {
    pub messages: Vec<SyntheticMessage>,
    pub tool_events: Vec<SyntheticToolEvent>,
    pub sessions: Vec<SyntheticSession>,
}

impl SyntheticWorkload {
    pub fn standard() -> Self {
        Self {
            messages: generate_messages(STANDARD_MESSAGE_COUNT),
            tool_events: generate_tool_events(STANDARD_TOOL_EVENT_COUNT),
            sessions: generate_sessions(STANDARD_SESSION_COUNT),
        }
    }
}

pub fn generate_messages(count: usize) -> Vec<SyntheticMessage> {
    (0..count)
        .map(|index| SyntheticMessage {
            id: index as u64,
            actor: if index % 3 == 0 {
                SyntheticActor::User
            } else {
                SyntheticActor::Assistant
            },
            markdown: format!(
                "### Event {index}\nSynthetic conversation content for virtualization. `src/task_{:04}.rs` remains outside the render tree until visible.",
                index % 4096
            ),
        })
        .collect()
}

pub fn generate_tool_events(count: usize) -> Vec<SyntheticToolEvent> {
    (0..count)
        .map(|index| {
            let (kind, verb) = match index % 4 {
                0 => (SyntheticToolKind::Search, "search"),
                1 => (SyntheticToolKind::Read, "read"),
                2 => (SyntheticToolKind::Edit, "edit"),
                _ => (SyntheticToolKind::Test, "test"),
            };

            SyntheticToolEvent {
                id: index as u64,
                kind,
                summary: format!("{verb} synthetic target {:05}", index % 10_000),
                payload_bytes: 512 + (index % 32) * 128,
            }
        })
        .collect()
}

pub fn generate_sessions(count: usize) -> Vec<SyntheticSession> {
    (0..count)
        .map(|index| {
            let runtime_status = match index % 10 {
                0 => AgentRuntimeStatus::Blocked,
                1..=5 => AgentRuntimeStatus::Working,
                6..=7 => AgentRuntimeStatus::Idle,
                8 => AgentRuntimeStatus::Done,
                _ => AgentRuntimeStatus::Unknown,
            };

            SyntheticSession {
                id: index as u64,
                task_name: format!("task-{index:04}"),
                runtime_status,
                changed_files: index % 12,
                attention_required: matches!(runtime_status, AgentRuntimeStatus::Blocked),
                recent_activity: format!("Processing src/module_{:03}.rs", index % 256),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_workload_matches_phase_zero_cardinality() {
        let workload = SyntheticWorkload::standard();

        assert_eq!(workload.messages.len(), STANDARD_MESSAGE_COUNT);
        assert_eq!(workload.tool_events.len(), STANDARD_TOOL_EVENT_COUNT);
        assert_eq!(workload.sessions.len(), STANDARD_SESSION_COUNT);
    }
}
