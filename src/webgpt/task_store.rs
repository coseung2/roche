//! Owned task map and bounded orchestration event log.

use std::collections::{BTreeMap, VecDeque};

use super::types::{OrchestratorEvent, OrchestratorTask, now_ms};

const MAX_TASK_EVENTS: usize = 2_000;

#[derive(Debug)]
pub(super) struct TaskStore {
    pub tasks: BTreeMap<String, OrchestratorTask>,
    pub events: VecDeque<OrchestratorEvent>,
    next_event_seq: u64,
}

impl Default for TaskStore {
    fn default() -> Self {
        Self {
            tasks: BTreeMap::new(),
            events: VecDeque::new(),
            next_event_seq: 1,
        }
    }
}

impl TaskStore {
    pub fn push_event(
        &mut self,
        task_id: Option<String>,
        event: impl Into<String>,
        summary: impl Into<String>,
    ) {
        let entry = OrchestratorEvent {
            seq: self.next_event_seq,
            task_id,
            event: event.into(),
            summary: summary.into(),
            timestamp_ms: now_ms(),
        };
        self.next_event_seq = self.next_event_seq.saturating_add(1);
        self.events.push_back(entry);
        while self.events.len() > MAX_TASK_EVENTS {
            self.events.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_log_is_bounded_and_monotonic() {
        let mut store = TaskStore::default();
        for index in 0..(MAX_TASK_EVENTS + 3) {
            store.push_event(None, "test.event", format!("event {index}"));
        }
        assert_eq!(store.events.len(), MAX_TASK_EVENTS);
        assert_eq!(store.events.front().expect("first retained").seq, 4);
        assert_eq!(
            store.events.back().expect("last retained").seq,
            (MAX_TASK_EVENTS + 3) as u64
        );
    }
}
