//! Codex app-server public fa챌ade and compatibility re-exports.

mod catalog;
mod controller;
mod protocol;
mod types;
mod worker;

#[allow(unused_imports)]
pub use controller::CodexRuntimeController;
pub(crate) use controller::{CodexCommand, CodexThreadTarget, CodexWorkerRuntime};
#[allow(unused_imports)]
pub use types::{
    CodexActivity, CodexActivityKind, CodexActivityPhase, CodexCatalogModel, CodexConnection,
    CodexEvent, CodexHistoryMessage, CodexHistoryRole, CodexReasoningLevel, CodexStoredThread,
};

#[cfg(test)]
use catalog::*;
#[cfg(test)]
use controller::CodexEventSink;
#[cfg(test)]
use protocol::*;
#[cfg(test)]
use serde_json::json;
#[cfg(test)]
use std::{path::PathBuf, sync::mpsc};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_sink_preserves_order_and_orchestrator_delivery_when_ui_disconnects() {
        let (ui_tx, ui_rx) = mpsc::channel();
        let (orchestrator_tx, orchestrator_rx) = mpsc::channel();
        let sink = CodexEventSink {
            ui: ui_tx,
            orchestrator: orchestrator_tx,
        };
        let first = CodexEvent::Notice("first".to_owned());
        let second = CodexEvent::Notice("second".to_owned());
        sink.send(first.clone()).expect("deliver first");
        sink.send(second.clone()).expect("deliver second");
        assert_eq!(orchestrator_rx.try_recv().unwrap(), first);
        assert_eq!(orchestrator_rx.try_recv().unwrap(), second);
        assert_eq!(ui_rx.try_recv().unwrap(), first);
        assert_eq!(ui_rx.try_recv().unwrap(), second);

        drop(ui_rx);
        let after_disconnect = CodexEvent::Notice("orchestrator-only".to_owned());
        assert!(sink.send(after_disconnect.clone()).is_err());
        assert_eq!(orchestrator_rx.try_recv().unwrap(), after_disconnect);
    }

    #[test]
    fn codex_text_input_matches_app_server_v2_shape() {
        let input = json!({"type": "text", "text": "hello", "textElements": []});
        assert_eq!(input["type"], "text");
        assert_eq!(input["text"], "hello");
        assert_eq!(input["textElements"], json!([]));
    }

    #[test]
    fn turn_start_params_include_selected_model_override() {
        let params = turn_start_params(
            "thread-1",
            "hello".to_owned(),
            &[],
            "high".to_owned(),
            Some("gpt-5.6-sol".to_owned()),
        );
        assert_eq!(params["threadId"], "thread-1");
        assert_eq!(params["effort"], "high");
        assert_eq!(params["model"], "gpt-5.6-sol");
        assert_eq!(params["input"][0]["text"], "hello");
    }

    #[test]
    fn turn_start_params_omit_model_for_configured_default() {
        let params = turn_start_params("thread-1", "hello".to_owned(), &[], "low".to_owned(), None);
        assert!(params.get("model").is_none());
    }

    #[test]
    fn turn_start_params_encode_local_images_and_file_mentions() {
        let attachments = vec![
            PathBuf::from(r"C:\tmp\screen.png"),
            PathBuf::from(r"C:\tmp\notes.pdf"),
        ];
        let params = turn_start_params(
            "thread-1",
            "check these".to_owned(),
            &attachments,
            "high".to_owned(),
            None,
        );
        assert_eq!(params["input"][1]["type"], "localImage");
        assert_eq!(params["input"][1]["path"], r"C:\tmp\screen.png");
        assert_eq!(params["input"][2]["type"], "mention");
        assert_eq!(params["input"][2]["name"], "notes.pdf");
    }

    #[test]
    fn catalog_parses_object_and_string_entries() {
        let root = json!({
            "models": [
                {
                    "slug": "gpt-5.6-sol",
                    "display_name": "GPT-5.6-Sol",
                    "description": "Reasoning model",
                    "default_reasoning_level": "high",
                    "supported_reasoning_levels": [
                        {"effort": "medium", "description": "Balanced"},
                        {"effort": "high", "description": "Deep"}
                    ],
                    "priority": 10
                },
                {"id": "opencode-go/deepseek-v4-flash"},
                "xai/grok-4.6"
            ]
        });
        let models = parse_catalog_models(&root);
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].slug, "gpt-5.6-sol");
        assert_eq!(models[0].display_name, "GPT-5.6-Sol");
        assert_eq!(models[0].default_reasoning_level.as_deref(), Some("high"));
        assert_eq!(models[0].supported_reasoning_levels.len(), 2);
        assert_eq!(
            models[0].supported_reasoning_levels[1]
                .description
                .as_deref(),
            Some("Deep")
        );
        assert_eq!(models[1].display_name, "opencode-go/deepseek-v4-flash");
        assert_eq!(models[2].slug, "xai/grok-4.6");
    }

    #[test]
    fn activity_classification_keeps_kind_separate_from_phase() {
        let terminal = json!({
            "id": "item-terminal",
            "type": "commandExecution",
            "status": "inProgress",
            "command": "git status --short"
        });
        let started = codex_activity_from_item("item/started", &terminal, "turn-1").unwrap();
        assert_eq!(started.item_id, "item-terminal");
        assert_eq!(started.kind, CodexActivityKind::Terminal);
        assert_eq!(started.phase, CodexActivityPhase::Running);
        assert_eq!(started.detail, "git status --short");

        let completed = codex_activity_from_item("item/completed", &terminal, "turn-1").unwrap();
        assert_eq!(completed.item_id, started.item_id);
        assert_eq!(completed.kind, CodexActivityKind::Terminal);
        assert_eq!(completed.phase, CodexActivityPhase::Completed);

        let tool = json!({
            "id": "item-tool",
            "type": "mcpToolCall",
            "server": "roche",
            "tool": "read",
            "status": "completed"
        });
        let tool = codex_activity_from_item("item/completed", &tool, "turn-1").unwrap();
        assert_eq!(tool.kind, CodexActivityKind::ToolCall);
        assert_eq!(tool.title, "roche/read");
    }

    #[test]
    fn collab_tool_call_exposes_worker_thread_identity() {
        let started = json!({
            "id": "item-worker",
            "type": "collabToolCall",
            "tool": "spawn_agent",
            "status": "inProgress",
            "prompt": "Inspect the Web GPT bridge"
        });
        let started = codex_activity_from_item("item/started", &started, "turn-1").unwrap();
        assert_eq!(started.kind, CodexActivityKind::Worker);
        assert_eq!(started.phase, CodexActivityPhase::Running);
        assert_eq!(started.title, "워커 생성");
        assert!(started.worker_thread_id.is_none());

        let completed = json!({
            "id": "item-worker",
            "type": "collabToolCall",
            "tool": "spawn_agent",
            "status": "completed",
            "prompt": "Inspect the Web GPT bridge",
            "newThreadId": "thread-worker-1",
            "agentStatus": "running"
        });
        let completed = codex_activity_from_item("item/completed", &completed, "turn-1").unwrap();
        assert_eq!(completed.item_id, started.item_id);
        assert_eq!(
            completed.worker_thread_id.as_deref(),
            Some("thread-worker-1")
        );
        assert_eq!(completed.worker_status.as_deref(), Some("running"));
    }

    #[test]
    fn non_activity_items_are_not_mislabeled_as_tools() {
        let reasoning = json!({"id": "reasoning-1", "type": "reasoning", "text": "hidden"});
        assert!(codex_activity_from_item("item/completed", &reasoning, "turn-1").is_none());

        let assistant = json!({"id": "message-1", "type": "agentMessage", "text": "hello"});
        assert!(codex_activity_from_item("item/completed", &assistant, "turn-1").is_none());
    }

    #[test]
    fn config_model_catalog_path_is_parsed() {
        let toml = "model = \"x\"\nmodel_catalog_json = \"C:\\\\Users\\\\.codex\\\\opencodex-catalog.json\"\n";
        let path = configured_model_catalog_path(toml).expect("configured path");
        assert_eq!(
            path,
            PathBuf::from(r"C:\Users\.codex\opencodex-catalog.json")
        );
    }
}
