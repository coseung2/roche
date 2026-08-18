//! In-memory Web GPT chat mailbox and its deterministic state transitions.

use std::collections::{BTreeMap, VecDeque};

use serde_json::Value;

use super::types::{WebChatRequest, WebChatStatus, next_chat_id, now_ms, required_string};

pub(super) struct ChatEvent {
    pub event: &'static str,
    pub summary: String,
}

pub(super) struct ChatOutcome {
    pub value: Value,
    pub event: Option<ChatEvent>,
}

#[derive(Debug, Default)]
pub(super) struct ChatMailbox {
    requests: BTreeMap<String, WebChatRequest>,
    pending: VecDeque<String>,
}

impl ChatMailbox {
    pub fn len(&self) -> usize {
        self.requests.len()
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn submit(&mut self, params: &Value) -> Result<ChatOutcome, String> {
        let text = required_string(params, "text")?;
        let reasoning_level = params
            .get("reasoning_level")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("very_high")
            .to_owned();
        let id = next_chat_id();
        let timestamp = now_ms();
        let request = WebChatRequest {
            id: id.clone(),
            text,
            reasoning_level,
            status: WebChatStatus::Pending,
            response: None,
            created_at_ms: timestamp,
            updated_at_ms: timestamp,
        };
        self.requests.insert(id.clone(), request);
        self.pending.push_back(id.clone());
        Ok(ChatOutcome {
            value: self.serialize(&id),
            event: Some(ChatEvent {
                event: "chat.pending",
                summary: format!("Web GPT chat request queued: {id}"),
            }),
        })
    }

    pub fn claim_pending(&mut self) -> Result<ChatOutcome, String> {
        while let Some(id) = self.pending.pop_front() {
            let Some(request) = self.requests.get_mut(&id) else {
                continue;
            };
            if request.status != WebChatStatus::Pending {
                continue;
            }
            request.status = WebChatStatus::Claimed;
            request.updated_at_ms = now_ms();
            let value = serde_json::to_value(&*request)
                .map_err(|error| format!("Could not serialize chat request: {error}"))?;
            return Ok(ChatOutcome {
                value,
                event: Some(ChatEvent {
                    event: "chat.claimed",
                    summary: format!("Web GPT claimed chat request: {id}"),
                }),
            });
        }
        Ok(ChatOutcome {
            value: Value::Null,
            event: None,
        })
    }

    pub fn release(&mut self, params: &Value) -> Result<ChatOutcome, String> {
        let request_id = required_string(params, "request_id")?;
        let request = self
            .requests
            .get_mut(&request_id)
            .ok_or_else(|| format!("Unknown chat request: {request_id}"))?;
        if request.status != WebChatStatus::Claimed {
            return Err(format!(
                "Chat request {request_id} is {:?}; only claimed requests can be released",
                request.status
            ));
        }
        request.status = WebChatStatus::Pending;
        request.updated_at_ms = now_ms();
        self.pending.push_front(request_id.clone());
        Ok(ChatOutcome {
            value: self.serialize(&request_id),
            event: Some(ChatEvent {
                event: "chat.released",
                summary: format!("Web GPT released chat request: {request_id}"),
            }),
        })
    }

    pub fn respond(&mut self, params: &Value) -> Result<ChatOutcome, String> {
        let request_id = required_string(params, "request_id")?;
        let text = required_string(params, "text")?;
        let request = self
            .requests
            .get_mut(&request_id)
            .ok_or_else(|| format!("Unknown chat request: {request_id}"))?;
        if matches!(
            request.status,
            WebChatStatus::Answered | WebChatStatus::Cancelled
        ) {
            return Err(format!(
                "Chat request {request_id} is already {:?}",
                request.status
            ));
        }
        request.status = WebChatStatus::Answered;
        request.response = Some(text);
        request.updated_at_ms = now_ms();
        Ok(ChatOutcome {
            value: self.serialize(&request_id),
            event: Some(ChatEvent {
                event: "chat.answered",
                summary: format!("Web GPT answered chat request: {request_id}"),
            }),
        })
    }

    pub fn poll(&self, params: &Value) -> Result<Value, String> {
        let request_id = required_string(params, "request_id")?;
        self.requests
            .get(&request_id)
            .map(|request| {
                serde_json::to_value(request).expect("chat request serialization cannot fail")
            })
            .ok_or_else(|| format!("Unknown chat request: {request_id}"))
    }

    pub fn cancel(&mut self, params: &Value) -> Result<ChatOutcome, String> {
        let request_id = required_string(params, "request_id")?;
        let request = self
            .requests
            .get_mut(&request_id)
            .ok_or_else(|| format!("Unknown chat request: {request_id}"))?;
        if request.status != WebChatStatus::Answered {
            request.status = WebChatStatus::Cancelled;
            request.updated_at_ms = now_ms();
            self.pending.retain(|id| id != &request_id);
        }
        Ok(ChatOutcome {
            value: self.serialize(&request_id),
            event: Some(ChatEvent {
                event: "chat.cancelled",
                summary: format!("Web GPT chat request cancelled: {request_id}"),
            }),
        })
    }

    fn serialize(&self, request_id: &str) -> Value {
        serde_json::to_value(
            self.requests
                .get(request_id)
                .expect("existing chat request"),
        )
        .expect("chat request serialization cannot fail")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn request(value: &Value) -> WebChatRequest {
        serde_json::from_value(value.clone()).expect("chat request")
    }

    #[test]
    fn release_returns_claimed_request_to_the_front_of_fifo() {
        let mut mailbox = ChatMailbox::default();
        let first = mailbox
            .submit(&json!({"text": "first"}))
            .expect("first submit");
        let second = mailbox
            .submit(&json!({"text": "second"}))
            .expect("second submit");
        let first_request = request(&first.value);
        let first_event = first.event.expect("pending event");
        assert_eq!(first_event.event, "chat.pending");
        assert_eq!(
            first_event.summary,
            format!("Web GPT chat request queued: {}", first_request.id)
        );
        assert_eq!(second.event.expect("pending event").event, "chat.pending");

        let claimed = mailbox.claim_pending().expect("claim first");
        let first_id = request(&claimed.value).id;
        assert_eq!(claimed.event.expect("claimed event").event, "chat.claimed");
        let released = mailbox
            .release(&json!({"request_id": first_id.clone()}))
            .expect("release first");
        assert_eq!(
            released.event.expect("released event").event,
            "chat.released"
        );

        let reclaimed = mailbox.claim_pending().expect("reclaim first");
        assert_eq!(request(&reclaimed.value).id, first_id);
        let next = mailbox.claim_pending().expect("claim second");
        assert_ne!(request(&next.value).id, first_id);
        assert_eq!(mailbox.pending_len(), 0);
    }

    #[test]
    fn empty_claim_invalid_release_and_pending_cancel_are_explicit() {
        let mut mailbox = ChatMailbox::default();
        let empty = mailbox.claim_pending().expect("empty claim");
        assert_eq!(empty.value, Value::Null);
        assert!(empty.event.is_none());

        let submitted = mailbox
            .submit(&json!({"text": "cancel me"}))
            .expect("submit");
        let request_id = request(&submitted.value).id;
        assert!(
            mailbox
                .release(&json!({"request_id": request_id.clone()}))
                .is_err()
        );
        let cancelled = mailbox
            .cancel(&json!({"request_id": request_id.clone()}))
            .expect("cancel pending");
        assert_eq!(request(&cancelled.value).status, WebChatStatus::Cancelled);
        assert_eq!(mailbox.pending_len(), 0);
        assert_eq!(
            cancelled.event.expect("cancel event").summary,
            format!("Web GPT chat request cancelled: {request_id}")
        );
        assert_eq!(
            mailbox.claim_pending().expect("claim after cancel").value,
            Value::Null
        );
    }

    #[test]
    fn response_and_cancel_preserve_terminal_contract() {
        let mut mailbox = ChatMailbox::default();
        let submitted = mailbox
            .submit(&json!({"text": "question", "reasoning_level": "high"}))
            .expect("submit");
        let request_id = request(&submitted.value).id;
        let answered = mailbox
            .respond(&json!({"request_id": request_id.clone(), "text": "answer"}))
            .expect("respond");
        assert_eq!(request(&answered.value).status, WebChatStatus::Answered);
        assert_eq!(
            answered.event.expect("answered event").event,
            "chat.answered"
        );
        assert!(
            mailbox
                .respond(&json!({"request_id": request_id.clone(), "text": "again"}))
                .is_err()
        );

        let cancelled_after_answer = mailbox
            .cancel(&json!({"request_id": request_id.clone()}))
            .expect("cancel after answer");
        assert_eq!(
            request(&cancelled_after_answer.value).status,
            WebChatStatus::Answered
        );
        assert_eq!(
            cancelled_after_answer.event.expect("cancelled event").event,
            "chat.cancelled"
        );
    }
}
