//! Typed correlation contract shared by the Web GPT browser client and helper.
//!
//! The current production runtime still has one FIFO slot. These types make the
//! slot lease and turn ownership explicit before the browser runtime is expanded
//! to multiple WebViews, so a recycled request id cannot make an old event look
//! current merely because its string matches.

use serde::{Deserialize, Serialize};

/// Temporary account identity used by the existing single-profile runtime.
///
/// M2 replaces this value with a persisted `WebGptAccountPool` identifier. It is
/// deliberately explicit rather than inferred from cookies or visible page text.
pub const DEFAULT_LOCAL_WEB_GPT_ACCOUNT_ID: &str = "local-default";

/// The fixed slot plus generation leased to one browser turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WebGptSlotLease {
    pub slot_id: u32,
    pub generation: u64,
}

/// Ownership known before a turn receives a browser slot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WebGptTurnRequest {
    pub account_id: String,
    pub session_id: String,
    pub task_id: Option<String>,
    pub request_id: String,
}

impl WebGptTurnRequest {
    pub fn native_chat(session_id: String, request_id: String) -> Self {
        Self {
            account_id: DEFAULT_LOCAL_WEB_GPT_ACCOUNT_ID.to_owned(),
            session_id,
            task_id: None,
            request_id,
        }
    }

    pub fn worker(session_id: String, task_id: String, request_id: String) -> Self {
        Self {
            account_id: DEFAULT_LOCAL_WEB_GPT_ACCOUNT_ID.to_owned(),
            session_id,
            task_id: Some(task_id),
            request_id,
        }
    }

    pub fn lease(self, slot_id: u32, generation: u64) -> WebGptTurnCorrelation {
        WebGptTurnCorrelation {
            lease: WebGptSlotLease {
                slot_id,
                generation,
            },
            account_id: self.account_id,
            session_id: self.session_id,
            task_id: self.task_id,
            request_id: self.request_id,
        }
    }
}

/// Full identity echoed by every request-scoped browser command and event.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WebGptTurnCorrelation {
    pub lease: WebGptSlotLease,
    pub account_id: String,
    pub session_id: String,
    pub task_id: Option<String>,
    pub request_id: String,
}

impl WebGptTurnCorrelation {
    pub fn is_worker(&self) -> bool {
        self.task_id.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correlation_round_trips_without_losing_owner_or_lease() {
        let correlation = WebGptTurnRequest::worker(
            "session-a".to_owned(),
            "task-a".to_owned(),
            "request-a".to_owned(),
        )
        .lease(1, 7);

        let encoded = serde_json::to_string(&correlation).expect("serialize correlation");
        let decoded: WebGptTurnCorrelation =
            serde_json::from_str(&encoded).expect("deserialize correlation");

        assert_eq!(decoded, correlation);
        assert!(decoded.is_worker());
    }

    #[test]
    fn recycled_request_id_is_not_equal_across_slot_generations() {
        let first =
            WebGptTurnRequest::native_chat("session-a".to_owned(), "request-reused".to_owned())
                .lease(0, 1);
        let second =
            WebGptTurnRequest::native_chat("session-a".to_owned(), "request-reused".to_owned())
                .lease(0, 2);

        assert_ne!(first, second);
        assert!(!first.is_worker());
    }
}
