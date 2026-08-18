//! Cohesive desktop state ownership groups; rendering remains in the app shell.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::mpsc::{Receiver, Sender},
    thread::JoinHandle,
    time::Instant,
};

use crate::{
    codex::{CodexCatalogModel, CodexConnection, CodexRuntimeController},
    ocx::{
        OcxInjectionSettings, OcxModel, OcxQuotaController, OcxSubagentModels, ProcessMemory,
        ProviderPool, QuotaReport,
    },
    sessions::AgentSession,
    web_browser::{SharedWebGptBrowser, WebGptBrowserState},
    web_browser_protocol::WebGptTurnCorrelation,
    webgpt::WebGptRuntimeController,
};

use super::{
    ChatMessage, ChatModel, ChatPopoverPage, DesktopTelemetryEvent, DraftAttachment,
    OcxSettingsPage, WorkspaceEntry, WorkspaceTab,
};

pub(super) struct WorkspaceUiState {
    pub workspaces: Vec<WorkspaceEntry>,
    pub selected_workspace: Option<PathBuf>,
    pub workspace_picker: Option<JoinHandle<Option<PathBuf>>>,
    pub workspaces_store: PathBuf,
    pub state_store: PathBuf,
    pub last_state_save: Instant,
    pub sidebar_workspace_ratio: f32,
    pub selected_tab: WorkspaceTab,
}

pub(super) struct OcxUiState {
    pub controller: OcxQuotaController,
    pub reports: Vec<QuotaReport>,
    pub online: bool,
    pub status: Option<String>,
    pub memory: ProcessMemory,
    pub roche_memory: ProcessMemory,
    pub pid: Option<u32>,
    pub mem_headroom: u64,
    pub last_mem_sample: Instant,
    pub power_pending: bool,
    pub pools: Vec<ProviderPool>,
    pub account_busy: Option<String>,
    pub auto_switch_threshold: u32,
    pub auto_switch_busy: bool,
    pub last_account_poll: Instant,
    pub telemetry_tx: Sender<DesktopTelemetryEvent>,
    pub telemetry_rx: Receiver<DesktopTelemetryEvent>,
    pub memory_sample_pending: bool,
    pub account_poll_pending: bool,
    pub expanded_providers: HashSet<String>,
    pub provider_order: Vec<String>,
    pub settings_page: OcxSettingsPage,
    pub settings_provider: Option<String>,
    pub models: Vec<OcxModel>,
    pub subagent_models: OcxSubagentModels,
    pub injection_settings: OcxInjectionSettings,
    pub subagent_panel: usize,
    pub settings_poll_pending: bool,
    pub settings_action_pending: bool,
    pub last_settings_poll: Instant,
}

pub(super) struct RuntimeUiState {
    pub codex: CodexRuntimeController,
    pub webgpt: WebGptRuntimeController,
    pub web_browser: SharedWebGptBrowser,
    pub web_browser_state: WebGptBrowserState,
    pub codex_connection: CodexConnection,
    pub codex_thread_id: Option<String>,
    pub codex_turn_id: Option<String>,
    pub codex_model: Option<String>,
    pub codex_catalog_source: Option<String>,
    pub codex_catalog: Vec<CodexCatalogModel>,
    pub selected_codex_slug: Option<String>,
}

pub(super) struct SessionOwnershipState {
    pub tabs: Vec<AgentSession>,
    pub restored_ids: HashSet<String>,
    pub selected_id: Option<String>,
    pub chat_messages: HashMap<String, Vec<ChatMessage>>,
    pub expanded_activity_groups: HashSet<String>,
    pub web_local_sessions: HashMap<String, String>,
    pub web_local_correlations: HashMap<String, WebGptTurnCorrelation>,
    pub pending_codex_sessions: VecDeque<String>,
    pub codex_turn_sessions: HashMap<String, String>,
    pub codex_session_threads: HashMap<String, String>,
    pub native_worker_sessions: HashMap<String, String>,
    pub hidden_ids: HashSet<String>,
    pub title_overrides: HashMap<String, String>,
    pub rename_id: Option<String>,
    pub rename_draft: String,
    pub delete_confirm_id: Option<String>,
}

pub(super) struct ComposerUiState {
    pub prompt: String,
    pub attachments: Vec<DraftAttachment>,
    pub selected_model: ChatModel,
    pub reasoning_effort: String,
    pub popover_open: bool,
    pub popover_page: ChatPopoverPage,
    pub ime_composing: bool,
    pub focus_on_start: bool,
}
