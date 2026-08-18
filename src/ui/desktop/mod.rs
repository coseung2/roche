#![allow(dead_code)]

mod chat_view;
mod ocx_view;
mod runtime_sync;
mod settings_view;
mod state;
mod workspace;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

use eframe::egui::{self, RichText, TextEdit};
use serde::{Deserialize, Serialize};

use state::{ComposerUiState, OcxUiState, RuntimeUiState, SessionOwnershipState, WorkspaceUiState};

use crate::{
    codex::{
        CodexActivity, CodexActivityKind, CodexActivityPhase, CodexCatalogModel, CodexConnection,
        CodexEvent, CodexHistoryRole, CodexReasoningLevel, CodexRuntimeController,
        CodexStoredThread,
    },
    ocx::{
        OcxEvent, OcxInjectionSettings, OcxModel, OcxQuotaController, OcxSubagentModels,
        ProcessMemory, ProviderAccount, ProviderConfig, ProviderPool, QuotaBar, commit_headroom,
        consume_reset_credit, fetch_codex_active_state, fetch_injection_settings, fetch_ocx_models,
        fetch_provider_configs, fetch_provider_pool, fetch_subagent_models, format_bytes,
        ocx_health_pid, quota_bars, request_account_reauth, run_ocx, sample_current_process,
        sample_process, set_account_paused, set_active_account, set_auto_switch_threshold,
        set_injection_settings, set_model_visibility, set_subagent_models,
    },
    sessions::{AgentSession, SessionRuntime, SessionStatus},
    web_browser::{SharedWebGptBrowser, WebGptBrowserEvent, WebGptBrowserState},
    web_browser_protocol::{
        DEFAULT_LOCAL_WEB_GPT_ACCOUNT_ID, WebGptTurnCorrelation, WebGptTurnRequest,
    },
    webgpt::{WebGptRuntimeController, WebGptRuntimeEvent},
};

const LUCIDE_CHEVRON_DOWN: char = '\u{e06d}';
const LUCIDE_CHEVRON_LEFT: char = '\u{e06e}';
const LUCIDE_CHEVRON_RIGHT: char = '\u{e06f}';
const LUCIDE_COPY: char = '\u{e09e}';
const LUCIDE_FOLDER: char = '\u{e0d7}';
const LUCIDE_FOLDER_PLUS: char = '\u{e0d9}';
const LUCIDE_REFRESH: char = '\u{e145}';
const LOCAL_MAIN_SESSION_KEY: &str = "local-main";
const ICON_FONT_FAMILY: &str = "roche-icons";
const UI_FONT_BODY: f32 = 14.0;
const UI_FONT_SMALL: f32 = 12.0;
const UI_FONT_HEADING: f32 = 20.0;
const UI_FONT_MONO: f32 = 13.0;
const UI_FONT_ICON: f32 = 14.0;
const UI_LINE_HEIGHT: f32 = 18.0;
const UI_CONTROL_HEIGHT: f32 = 28.0;
const CHAT_BOTTOM_MARGIN: f32 = 16.0;
const CHAT_ASSISTANT_MAX_WIDTH: f32 = 760.0;
const CHAT_USER_MAX_WIDTH: f32 = 540.0;
const ORCHESTRATION_RAIL_MIN_AVAILABLE_WIDTH: f32 = 1_040.0;
const ORCHESTRATION_RAIL_WIDTH: f32 = 340.0;
const ORCHESTRATION_GRAPH_MAX_WORKERS: usize = 3;

fn chat_content_height(available_height: f32) -> f32 {
    (available_height - CHAT_BOTTOM_MARGIN).max(0.0)
}

fn apply_ime_event(ime_composing: &mut bool, event: &egui::ImeEvent) -> bool {
    match event {
        egui::ImeEvent::Preedit { text, .. } => {
            *ime_composing = !text.is_empty();
            false
        }
        egui::ImeEvent::Commit(_) => {
            *ime_composing = false;
            true
        }
        _ => false,
    }
}

#[derive(Clone)]
struct OrchestrationView {
    root: AgentSession,
    workers: Vec<AgentSession>,
}

impl OrchestrationView {
    fn review_session(&self) -> Option<&AgentSession> {
        self.workers
            .iter()
            .find(|session| session.status == SessionStatus::NeedsInput)
    }

    fn completed_count(&self) -> usize {
        self.workers
            .iter()
            .filter(|session| session.status == SessionStatus::Completed)
            .count()
    }
}

fn orchestration_view(
    sessions: &[AgentSession],
    selected_session_id: Option<&str>,
) -> Option<OrchestrationView> {
    let selected = selected_session_id
        .and_then(|selected_id| sessions.iter().find(|session| session.id == selected_id));
    let root_id = selected
        .map(|session| session.root_session_id.as_str())
        .or_else(|| {
            sessions
                .iter()
                .find(|candidate| {
                    candidate.parent_session_id.is_none()
                        && sessions.iter().any(|session| {
                            session.parent_session_id.as_deref() == Some(candidate.id.as_str())
                        })
                })
                .map(|session| session.id.as_str())
        })?;
    let root = sessions
        .iter()
        .find(|session| session.id == root_id)?
        .clone();
    let mut workers = sessions
        .iter()
        .filter(|session| session.is_worker() && session.root_session_id == root.id)
        .cloned()
        .collect::<Vec<_>>();
    workers.sort_by_key(|session| (session.depth, session.created_at_ms));
    (!workers.is_empty()).then_some(OrchestrationView { root, workers })
}

fn visible_graph_workers(
    view: &OrchestrationView,
    selected_session_id: Option<&str>,
) -> Vec<AgentSession> {
    let mut direct = view
        .workers
        .iter()
        .filter(|session| session.parent_session_id.as_deref() == Some(view.root.id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if direct.is_empty() {
        direct = view.workers.clone();
    }
    direct.sort_by_key(|session| session.created_at_ms);
    if direct.len() <= ORCHESTRATION_GRAPH_MAX_WORKERS {
        return direct;
    }
    let selected = selected_session_id
        .and_then(|selected_id| direct.iter().find(|session| session.id == selected_id))
        .cloned();
    direct.truncate(ORCHESTRATION_GRAPH_MAX_WORKERS);
    if let Some(selected) = selected
        && !direct.iter().any(|session| session.id == selected.id)
    {
        direct[ORCHESTRATION_GRAPH_MAX_WORKERS - 1] = selected;
    }
    direct
}

const NOTCH_BG: egui::Color32 = egui::Color32::from_rgb(0x18, 0x1B, 0x21);
const NOTCH_PANEL: egui::Color32 = egui::Color32::from_rgb(0x20, 0x24, 0x2A);
const NOTCH_BORDER: egui::Color32 = egui::Color32::from_rgb(0x2D, 0x32, 0x3A);
const NOTCH_BORDER_2: egui::Color32 = egui::Color32::from_rgb(0x35, 0x3A, 0x42);
const NOTCH_TEXT: egui::Color32 = egui::Color32::from_rgb(0xE8, 0xEC, 0xF0);
const NOTCH_TEXT_SUB: egui::Color32 = egui::Color32::from_rgb(0xC7, 0xCB, 0xD2);
const NOTCH_TEXT_MUTED: egui::Color32 = egui::Color32::from_rgb(0x8B, 0x8F, 0x98);
const NOTCH_LABEL: egui::Color32 = egui::Color32::from_rgb(0xA6, 0xA6, 0xA6);
const NOTCH_ACCENT: egui::Color32 = egui::Color32::from_rgb(0x6E, 0xE7, 0xA8);
const NOTCH_GREEN: egui::Color32 = egui::Color32::from_rgb(0x4E, 0xCB, 0x9D);
const NOTCH_CAUTION: egui::Color32 = egui::Color32::from_rgb(0x24, 0xBF, 0xFB);
const NOTCH_DANGER: egui::Color32 = egui::Color32::from_rgb(0x54, 0x54, 0xF5);
const NOTCH_BAR_BG: egui::Color32 = egui::Color32::from_rgb(0x30, 0x30, 0x30);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorkspaceEntry {
    name: String,
    path: PathBuf,
}

impl WorkspaceEntry {
    fn from_path(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Self { name, path }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum WorkspaceTab {
    Chat,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OcxSettingsPage {
    CodexAuth,
    Providers,
    Models,
    Subagents,
}

impl OcxSettingsPage {
    const ALL: [Self; 4] = [
        Self::CodexAuth,
        Self::Providers,
        Self::Models,
        Self::Subagents,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::CodexAuth => "Codex 인증",
            Self::Providers => "프로바이더",
            Self::Models => "모델",
            Self::Subagents => "서브에이전트",
        }
    }
}

enum WindowButton {
    Minimize,
    Maximize,
    Close,
}

enum AccountAction {
    Activate,
    Pause,
    Reset,
    Reauth,
}

#[derive(Debug)]
enum DesktopTelemetryEvent {
    Memory {
        roche_memory: ProcessMemory,
        mem_headroom: u64,
        ocx_pid: Option<u32>,
        ocx_memory: ProcessMemory,
    },
    ProviderPools {
        pools: Vec<ProviderPool>,
        auto_switch_threshold: u32,
    },
    AutoSwitchUpdated(Result<u32, String>),
    AccountActionFinished {
        busy_key: String,
        result: Result<(), String>,
    },
    OcxSettingsLoaded(Result<(Vec<OcxModel>, OcxSubagentModels, OcxInjectionSettings), String>),
    OcxSettingsAction(Result<(), String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum ChatModel {
    WebGpt56Sol,
    Codex,
}

impl ChatModel {
    fn label(self) -> &'static str {
        match self {
            Self::WebGpt56Sol => "[WEB] GPT-5.6 Sol",
            Self::Codex => "Codex",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum ChatRole {
    User,
    Assistant,
    Activity,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatPopoverPage {
    Root,
    Model,
    Reasoning,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatActivityEntry {
    item_id: String,
    title: String,
    detail: String,
    phase: CodexActivityPhase,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatActivityGroup {
    id: String,
    kind: CodexActivityKind,
    entries: Vec<ChatActivityEntry>,
}

impl ChatActivityGroup {
    fn phase(&self) -> CodexActivityPhase {
        if self
            .entries
            .iter()
            .any(|entry| entry.phase == CodexActivityPhase::Running)
        {
            CodexActivityPhase::Running
        } else if self
            .entries
            .iter()
            .any(|entry| entry.phase == CodexActivityPhase::Failed)
        {
            CodexActivityPhase::Failed
        } else {
            CodexActivityPhase::Completed
        }
    }

    fn status_label(&self) -> String {
        let suffix = match self.phase() {
            CodexActivityPhase::Running => " 중…",
            CodexActivityPhase::Completed => " 완료",
            CodexActivityPhase::Failed => " 실패",
        };
        format!("{}{}", self.kind.label(), suffix)
    }
}

#[derive(Clone)]
struct ChatMessage {
    role: ChatRole,
    model: ChatModel,
    text: String,
    turn_id: Option<String>,
    streaming: bool,
    image: Option<egui::TextureHandle>,
    activity: Option<ChatActivityGroup>,
}

#[derive(Clone)]
struct DraftAttachment {
    path: PathBuf,
    preview: Option<egui::TextureHandle>,
}

impl DraftAttachment {
    fn label(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedChatMessage {
    role: ChatRole,
    model: ChatModel,
    text: String,
    #[serde(default)]
    activity: Option<ChatActivityGroup>,
}

#[derive(Default, Serialize, Deserialize)]
struct PersistedDesktopState {
    #[serde(default)]
    selected_workspace: Option<PathBuf>,
    #[serde(default)]
    selected_tab: Option<WorkspaceTab>,
    #[serde(default)]
    selected_session_id: Option<String>,
    #[serde(default)]
    selected_model: Option<ChatModel>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    expanded_providers: Vec<String>,
    #[serde(default)]
    provider_order: Vec<String>,
    #[serde(default)]
    session_tabs: Vec<AgentSession>,
    #[serde(default)]
    chat_messages: HashMap<String, Vec<PersistedChatMessage>>,
    #[serde(default)]
    codex_session_threads: HashMap<String, String>,
    #[serde(default)]
    hidden_session_ids: HashSet<String>,
    #[serde(default)]
    session_title_overrides: HashMap<String, String>,
    #[serde(default)]
    sidebar_workspace_ratio: Option<f32>,
}

pub struct DesktopApp {
    workspace: WorkspaceUiState,
    ocx_ui: OcxUiState,
    runtime: RuntimeUiState,
    sessions: SessionOwnershipState,
    composer: ComposerUiState,
    runtime_message: Option<String>,
}

impl DesktopApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_runtime_fonts(&cc.egui_ctx);
        apply_notch_theme(&cc.egui_ctx);
        let default_root = std::env::var_os("ROCHE_PROJECT_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        let workspaces_store = workspaces_store_path();
        let state_store = desktop_state_path();
        let persisted = load_desktop_state(&state_store);
        let mut workspaces = load_workspaces(&workspaces_store);
        if workspaces.is_empty() {
            let default = WorkspaceEntry::from_path(default_root.clone());
            workspaces.push(default);
            save_workspaces(&workspaces_store, &workspaces);
        }
        let selected_workspace = persisted
            .selected_workspace
            .as_ref()
            .filter(|path| workspaces.iter().any(|entry| entry.path == **path))
            .cloned()
            .or_else(|| workspaces.first().map(|entry| entry.path.clone()));
        let codex_root = selected_workspace
            .clone()
            .unwrap_or_else(|| default_root.clone());
        let web_browser = if std::env::var_os("ROCHE_DISABLE_WEBGPT_BROWSER").is_some() {
            SharedWebGptBrowser::disabled("Web GPT browser disabled for diagnostics")
        } else {
            SharedWebGptBrowser::spawn()
        };
        let codex = CodexRuntimeController::spawn_with_web_browser(codex_root, web_browser.clone());
        let webgpt = WebGptRuntimeController::spawn();
        let ocx = OcxQuotaController::spawn();
        std::thread::spawn(|| {
            let _ = run_ocx("ensure");
        });
        ocx.refresh();
        let (telemetry_tx, telemetry_rx) = mpsc::channel();

        Self {
            workspace: WorkspaceUiState {
                workspaces,
                selected_workspace,
                workspace_picker: None,
                workspaces_store,
                state_store,
                last_state_save: Instant::now(),
                sidebar_workspace_ratio: persisted
                    .sidebar_workspace_ratio
                    .unwrap_or(0.34)
                    .clamp(0.15, 0.75),
                selected_tab: persisted.selected_tab.unwrap_or(WorkspaceTab::Chat),
            },
            ocx_ui: OcxUiState {
                controller: ocx,
                reports: Vec::new(),
                online: false,
                status: None,
                memory: Default::default(),
                roche_memory: Default::default(),
                pid: None,
                mem_headroom: 0,
                last_mem_sample: Instant::now(),
                power_pending: false,
                pools: Vec::new(),
                account_busy: None,
                auto_switch_threshold: 80,
                auto_switch_busy: false,
                last_account_poll: Instant::now(),
                telemetry_tx,
                telemetry_rx,
                memory_sample_pending: false,
                account_poll_pending: false,
                expanded_providers: persisted.expanded_providers.into_iter().collect(),
                provider_order: persisted.provider_order,
                settings_page: OcxSettingsPage::CodexAuth,
                settings_provider: None,
                models: Vec::new(),
                subagent_models: OcxSubagentModels::default(),
                injection_settings: OcxInjectionSettings::default(),
                subagent_panel: 0,
                settings_poll_pending: false,
                settings_action_pending: false,
                last_settings_poll: Instant::now(),
            },
            runtime: RuntimeUiState {
                codex,
                webgpt,
                web_browser,
                web_browser_state: WebGptBrowserState::Starting,
                codex_connection: CodexConnection::Starting,
                codex_thread_id: None,
                codex_turn_id: None,
                codex_model: None,
                codex_catalog_source: None,
                codex_catalog: Vec::new(),
                selected_codex_slug: None,
            },
            sessions: SessionOwnershipState {
                restored_ids: persisted
                    .session_tabs
                    .iter()
                    .map(|session| session.id.clone())
                    .collect(),
                tabs: persisted.session_tabs,
                selected_id: persisted.selected_session_id,
                chat_messages: persisted
                    .chat_messages
                    .into_iter()
                    .map(|(session_id, messages)| {
                        let messages = messages
                            .into_iter()
                            .map(|message| {
                                let text = if message.role == ChatRole::Assistant
                                    && message.model == ChatModel::WebGpt56Sol
                                {
                                    sanitize_web_assistant_text(&message.text)
                                } else {
                                    message.text
                                };
                                ChatMessage {
                                    role: message.role,
                                    model: message.model,
                                    text,
                                    turn_id: None,
                                    streaming: false,
                                    image: None,
                                    activity: message.activity,
                                }
                            })
                            .collect();
                        (session_id, messages)
                    })
                    .collect(),
                expanded_activity_groups: HashSet::new(),
                web_local_sessions: HashMap::new(),
                web_local_correlations: HashMap::new(),
                pending_codex_sessions: VecDeque::new(),
                codex_turn_sessions: HashMap::new(),
                codex_session_threads: persisted.codex_session_threads,
                native_worker_sessions: HashMap::new(),
                hidden_ids: persisted.hidden_session_ids,
                title_overrides: persisted.session_title_overrides,
                rename_id: None,
                rename_draft: String::new(),
                delete_confirm_id: None,
            },
            composer: ComposerUiState {
                prompt: String::new(),
                attachments: Vec::new(),
                selected_model: persisted.selected_model.unwrap_or(ChatModel::WebGpt56Sol),
                reasoning_effort: persisted
                    .reasoning_effort
                    .unwrap_or_else(|| "xhigh".to_owned()),
                popover_open: false,
                popover_page: ChatPopoverPage::Root,
                ime_composing: false,
                focus_on_start: true,
            },
            runtime_message: None,
        }
    }

    fn selected_session_key(&self) -> String {
        self.sessions
            .selected_id
            .clone()
            .unwrap_or_else(|| LOCAL_MAIN_SESSION_KEY.to_owned())
    }

    fn activate_session(&mut self, session_id: String) {
        self.sessions.selected_id = Some(session_id.clone());
        if let Some(thread_id) = self
            .sessions
            .codex_session_threads
            .get(&session_id)
            .cloned()
        {
            self.runtime.codex.read_thread(thread_id);
        }
        self.workspace.selected_tab = WorkspaceTab::Chat;
        self.refocus_composer();
    }

    fn primary_main_session_id(&self) -> Option<String> {
        self.sessions
            .tabs
            .iter()
            .filter(|session| {
                session.parent_session_id.is_none() && session.runtime == SessionRuntime::Unified
            })
            .min_by_key(|session| session.created_at_ms)
            .map(|session| session.id.clone())
    }

    fn is_native_local_session(&self, session_id: &str) -> bool {
        self.sessions.codex_session_threads.contains_key(session_id)
            || self
                .sessions
                .native_worker_sessions
                .values()
                .any(|mapped| mapped == session_id)
    }

    fn local_session_subtree_ids(&self, session_id: &str) -> Vec<String> {
        let mut ids = vec![session_id.to_owned()];
        let mut index = 0;
        while index < ids.len() {
            let current = ids[index].clone();
            ids.extend(
                self.sessions
                    .tabs
                    .iter()
                    .filter(|session| {
                        session.parent_session_id.as_deref() == Some(current.as_str())
                    })
                    .map(|session| session.id.clone()),
            );
            index += 1;
        }
        ids
    }

    fn apply_session_rename(&mut self, session: AgentSession) {
        if let Some(existing) = self
            .sessions
            .tabs
            .iter_mut()
            .find(|existing| existing.id == session.id)
        {
            *existing = session;
        }
    }

    fn remove_session_tabs(&mut self, session_ids: &[String]) {
        let removed = session_ids.iter().collect::<HashSet<_>>();
        self.sessions
            .tabs
            .retain(|session| !removed.contains(&session.id));
        purge_web_local_session_ownership(
            session_ids,
            &mut self.sessions.web_local_sessions,
            &mut self.sessions.web_local_correlations,
        );
        for session_id in session_ids {
            self.sessions.chat_messages.remove(session_id);
            self.sessions.restored_ids.remove(session_id);
        }
        if self
            .sessions
            .selected_id
            .as_ref()
            .is_some_and(|selected| removed.contains(selected))
        {
            self.sessions.selected_id = self
                .sessions
                .tabs
                .iter()
                .find(|session| session.parent_session_id.is_none())
                .or_else(|| self.sessions.tabs.first())
                .map(|session| session.id.clone());
        }
    }

    fn rename_session_from_menu(&mut self, session_id: &str, title: String) {
        let title = title.trim();
        if title.is_empty() {
            self.runtime_message = Some("세션 이름은 비워둘 수 없습니다.".to_owned());
            return;
        }
        if self.is_native_local_session(session_id) {
            self.sessions
                .title_overrides
                .insert(session_id.to_owned(), title.to_owned());
            if let Some(session) = self
                .sessions
                .tabs
                .iter_mut()
                .find(|session| session.id == session_id)
            {
                session.title = title.to_owned();
            }
        } else {
            self.runtime
                .webgpt
                .rename_session(session_id.to_owned(), title.to_owned());
        }
        self.sessions.rename_id = None;
        self.sessions.rename_draft.clear();
        self.runtime_message = Some("세션 이름 변경 중…".to_owned());
    }

    fn delete_session_from_menu(&mut self, session_id: &str) {
        if self.primary_main_session_id().as_deref() == Some(session_id) {
            self.runtime_message = Some("Main 세션은 삭제할 수 없습니다.".to_owned());
            self.sessions.delete_confirm_id = None;
            return;
        }
        if self.is_native_local_session(session_id) {
            let session_ids = self.local_session_subtree_ids(session_id);
            for id in &session_ids {
                self.sessions.hidden_ids.insert(id.clone());
            }
            self.remove_session_tabs(&session_ids);
            self.runtime_message =
                Some("Codex 기록은 유지하고 Roche 탭에서 숨겼습니다.".to_owned());
        } else {
            self.runtime.webgpt.delete_session(session_id.to_owned());
            self.runtime_message = Some("세션 삭제 중…".to_owned());
        }
        self.sessions.delete_confirm_id = None;
    }

    fn push_chat_message(&mut self, session_id: &str, message: ChatMessage) {
        self.sessions
            .chat_messages
            .entry(session_id.to_owned())
            .or_default()
            .push(message);
    }

    fn upsert_activity(
        &mut self,
        session_id: &str,
        turn_id: &str,
        model: ChatModel,
        activity: CodexActivity,
    ) {
        let messages = self
            .sessions
            .chat_messages
            .entry(session_id.to_owned())
            .or_default();

        for message in messages.iter_mut().rev() {
            if message.turn_id.as_deref() != Some(turn_id) {
                continue;
            }
            let Some(group) = message.activity.as_mut() else {
                continue;
            };
            if let Some(entry) = group
                .entries
                .iter_mut()
                .find(|entry| entry.item_id == activity.item_id)
            {
                entry.title = activity.title;
                entry.detail = activity.detail;
                entry.phase = activity.phase;
                message.streaming = group.phase() == CodexActivityPhase::Running;
                return;
            }
        }

        let entry = ChatActivityEntry {
            item_id: activity.item_id.clone(),
            title: activity.title,
            detail: activity.detail,
            phase: activity.phase,
        };
        if let Some(message) = messages.last_mut()
            && message.role == ChatRole::Activity
            && message.model == model
            && message.turn_id.as_deref() == Some(turn_id)
            && let Some(group) = message.activity.as_mut()
            && group.kind == activity.kind
        {
            group.entries.push(entry);
            message.streaming = group.phase() == CodexActivityPhase::Running;
            return;
        }

        messages.push(ChatMessage {
            role: ChatRole::Activity,
            model,
            text: String::new(),
            turn_id: Some(turn_id.to_owned()),
            streaming: activity.phase == CodexActivityPhase::Running,
            image: None,
            activity: Some(ChatActivityGroup {
                id: format!("{turn_id}:{}", activity.item_id),
                kind: activity.kind,
                entries: vec![entry],
            }),
        });
    }

    fn upsert_native_worker_session(
        &mut self,
        parent_session_id: &str,
        turn_id: &str,
        activity: &CodexActivity,
    ) {
        if activity.kind != CodexActivityKind::Worker {
            return;
        }
        let item_key = format!("item:{turn_id}:{}", activity.item_id);
        let thread_key = activity
            .worker_thread_id
            .as_ref()
            .map(|thread_id| format!("thread:{thread_id}"));
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default();

        let existing_session_id = self
            .sessions
            .native_worker_sessions
            .get(&item_key)
            .cloned()
            .or_else(|| {
                thread_key
                    .as_ref()
                    .and_then(|key| self.sessions.native_worker_sessions.get(key).cloned())
            });
        if let Some(session_id) = existing_session_id {
            if !self.sessions.hidden_ids.contains(&session_id)
                && let Some(session) = self
                    .sessions
                    .tabs
                    .iter_mut()
                    .find(|session| session.id == session_id)
            {
                session.status = native_worker_status(activity, Some(session.status));
                session.updated_at_ms = timestamp;
                if let Some(title) = self.sessions.title_overrides.get(&session_id) {
                    session.title = title.clone();
                } else if activity.title == "워커 생성" && !activity.detail.trim().is_empty() {
                    session.title = native_worker_title(&activity.detail);
                }
            }
            self.sessions
                .native_worker_sessions
                .insert(item_key, session_id.clone());
            if let Some(thread_key) = thread_key {
                self.sessions
                    .native_worker_sessions
                    .insert(thread_key, session_id);
            }
            return;
        }

        let parent = self
            .sessions
            .tabs
            .iter()
            .find(|session| session.id == parent_session_id)
            .cloned();
        let native_id = activity
            .worker_thread_id
            .clone()
            .unwrap_or_else(|| format!("native-worker-{}", activity.item_id));
        let project_key = parent
            .as_ref()
            .map(|session| session.project_key.clone())
            .or_else(|| {
                self.workspace
                    .selected_workspace
                    .as_ref()
                    .map(|path| path.display().to_string())
            })
            .unwrap_or_else(|| "native".to_owned());
        let root_session_id = parent
            .as_ref()
            .map(|session| session.root_session_id.clone())
            .unwrap_or_else(|| parent_session_id.to_owned());
        let depth = parent
            .as_ref()
            .map(|session| session.depth.saturating_add(1))
            .unwrap_or(1);
        let title = self
            .sessions
            .title_overrides
            .get(&native_id)
            .cloned()
            .unwrap_or_else(|| native_worker_title(&activity.detail));
        let session = AgentSession {
            id: native_id.clone(),
            project_key,
            title,
            runtime: SessionRuntime::Codex,
            status: native_worker_status(activity, None),
            parent_session_id: Some(parent_session_id.to_owned()),
            root_session_id,
            depth,
            created_by_session_id: Some(parent_session_id.to_owned()),
            worker_ids: Vec::new(),
            created_at_ms: timestamp,
            updated_at_ms: timestamp,
        };
        if let Some(parent) = self
            .sessions
            .tabs
            .iter_mut()
            .find(|session| session.id == parent_session_id)
            && !parent.worker_ids.contains(&native_id)
        {
            parent.worker_ids.push(native_id.clone());
        }
        if !self.sessions.hidden_ids.contains(&native_id) {
            self.sessions.tabs.push(session);
        }
        self.sessions
            .native_worker_sessions
            .insert(item_key, native_id.clone());
        if let Some(thread_key) = thread_key {
            self.sessions
                .native_worker_sessions
                .insert(thread_key, native_id);
        }
    }

    fn refocus_composer(&mut self) {
        // Any picker/tab click can move native keyboard focus away from TextEdit. Clear
        // stale IME composition state at the same boundary so Enter cannot remain blocked.
        self.composer.ime_composing = false;
        self.composer.focus_on_start = true;
    }

    fn merge_codex_threads(&mut self, threads: Vec<CodexStoredThread>) {
        for thread in threads {
            let existing_session_id =
                self.sessions
                    .codex_session_threads
                    .iter()
                    .find_map(|(session_id, thread_id)| {
                        (thread_id == &thread.thread_id).then(|| session_id.clone())
                    });
            let session_id =
                existing_session_id.unwrap_or_else(|| format!("codex:{}", thread.thread_id));
            self.sessions
                .codex_session_threads
                .insert(session_id.clone(), thread.thread_id.clone());
            if self.sessions.hidden_ids.contains(&session_id) {
                continue;
            }
            let title = self
                .sessions
                .title_overrides
                .get(&session_id)
                .cloned()
                .unwrap_or_else(|| codex_thread_title(&thread));
            if let Some(session) = self
                .sessions
                .tabs
                .iter_mut()
                .find(|session| session.id == session_id)
            {
                session.title = title;
                session.updated_at_ms = thread.updated_at.max(0) as u128 * 1000;
                continue;
            }
            let parent_session_id = thread
                .parent_thread_id
                .as_ref()
                .map(|parent| format!("codex:{parent}"));
            let root_session_id = parent_session_id
                .clone()
                .unwrap_or_else(|| session_id.clone());
            self.sessions.tabs.push(AgentSession {
                id: session_id.clone(),
                project_key: thread.cwd.display().to_string(),
                title,
                runtime: SessionRuntime::Codex,
                status: SessionStatus::Idle,
                parent_session_id,
                root_session_id,
                depth: u32::from(thread.parent_thread_id.is_some()),
                created_by_session_id: None,
                worker_ids: Vec::new(),
                created_at_ms: thread.created_at.max(0) as u128 * 1000,
                updated_at_ms: thread.updated_at.max(0) as u128 * 1000,
            });
        }
    }

    fn apply_codex_history(
        &mut self,
        thread_id: &str,
        history: Vec<crate::codex::CodexHistoryMessage>,
    ) {
        let Some(session_id) = self.sessions.codex_session_threads.iter().find_map(
            |(session_id, mapped_thread_id)| {
                (mapped_thread_id == thread_id).then(|| session_id.clone())
            },
        ) else {
            return;
        };
        let restored = history
            .into_iter()
            .map(|message| ChatMessage {
                role: match message.role {
                    CodexHistoryRole::User => ChatRole::User,
                    CodexHistoryRole::Assistant => ChatRole::Assistant,
                },
                model: ChatModel::Codex,
                text: message.text,
                turn_id: message.turn_id,
                streaming: false,
                image: None,
                activity: None,
            })
            .collect::<Vec<_>>();
        if restored.is_empty() {
            return;
        }
        let existing = self.sessions.chat_messages.entry(session_id).or_default();
        let web_messages = existing
            .iter()
            .filter(|message| message.model == ChatModel::WebGpt56Sol)
            .cloned()
            .collect::<Vec<_>>();
        *existing = restored;
        existing.extend(web_messages);
    }

    fn add_draft_attachment(&mut self, path: PathBuf, preview: Option<egui::TextureHandle>) {
        if !path.is_file() {
            self.runtime_message = Some(format!("첨부할 수 없는 경로입니다: {}", path.display()));
            return;
        }
        if self
            .composer
            .attachments
            .iter()
            .any(|attachment| attachment.path == path)
        {
            return;
        }
        self.composer
            .attachments
            .push(DraftAttachment { path, preview });
    }

    fn apply_session_snapshot(&mut self, sessions: Vec<AgentSession>) {
        let previous = self.sessions.selected_id.clone();
        let mut native_session_ids = self
            .sessions
            .native_worker_sessions
            .values()
            .cloned()
            .collect::<HashSet<_>>();
        native_session_ids.extend(self.sessions.codex_session_threads.keys().cloned());
        let native_sessions = self
            .sessions
            .tabs
            .iter()
            .filter(|session| native_session_ids.contains(&session.id))
            .cloned()
            .collect::<Vec<_>>();
        let previous_is_live = session_id_is_live(previous.as_deref(), &sessions)
            || previous
                .as_ref()
                .is_some_and(|id| native_session_ids.contains(id));
        let restored_sessions = self
            .sessions
            .tabs
            .iter()
            .filter(|session| self.sessions.restored_ids.contains(&session.id))
            .cloned()
            .collect::<Vec<_>>();
        self.sessions.tabs = sessions;
        for session in native_sessions {
            if !self.sessions.tabs.iter().any(|live| live.id == session.id) {
                self.sessions.tabs.push(session);
            }
        }
        let next = self
            .sessions
            .tabs
            .iter()
            .find(|session| session.parent_session_id.is_none())
            .or_else(|| self.sessions.tabs.first())
            .map(|session| session.id.clone());
        if let Some(next) = next {
            let restored_root_ids = restored_sessions
                .iter()
                .filter(|session| session.parent_session_id.is_none())
                .map(|session| session.id.clone())
                .collect::<Vec<_>>();
            for session_id in restored_root_ids {
                if session_id != next
                    && let Some(messages) = self.sessions.chat_messages.remove(&session_id)
                {
                    self.sessions
                        .chat_messages
                        .entry(next.clone())
                        .or_default()
                        .extend(messages);
                }
                self.sessions.restored_ids.remove(&session_id);
            }
            if let Some(local_messages) = self.sessions.chat_messages.remove(LOCAL_MAIN_SESSION_KEY)
            {
                self.sessions
                    .chat_messages
                    .entry(next.clone())
                    .or_default()
                    .extend(local_messages);
            }
            for mut session in restored_sessions {
                if self.sessions.tabs.iter().any(|live| live.id == session.id) {
                    continue;
                }
                session.status = SessionStatus::Offline;
                self.sessions.tabs.push(session);
            }
            let selected_is_restored = previous.as_deref().is_some_and(|id| {
                self.sessions
                    .tabs
                    .iter()
                    .any(|session| session.id == id && self.sessions.restored_ids.contains(id))
            });
            self.sessions.selected_id = if previous_is_live || selected_is_restored {
                previous
            } else {
                Some(next)
            };
        }
    }

    fn render_top_bar_content(&mut self, ui: &mut egui::Ui) {
        let button_w = 40.0;
        let bar_rect = ui.max_rect();
        let drag_rect = egui::Rect::from_min_max(
            bar_rect.min,
            egui::pos2(bar_rect.max.x - button_w * 3.0, bar_rect.max.y),
        );

        // Title + runtime status inside the draggable region.
        let mut title_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(drag_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        title_ui.horizontal_centered(|ui| {
            ui.label(RichText::new("Roche").strong().color(NOTCH_TEXT));
            ui.small(RichText::new("AI Workstation").color(NOTCH_TEXT_MUTED));
            ui.separator();
            match &self.runtime.codex_connection {
                CodexConnection::Starting => {
                    paint_status_dot(ui, NOTCH_TEXT_MUTED);
                    ui.small(RichText::new("LOCAL CODEX STARTING").color(NOTCH_TEXT_SUB));
                }
                CodexConnection::Ready { version } => {
                    paint_status_dot(ui, NOTCH_ACCENT);
                    ui.small(RichText::new("LOCAL CODEX READY").color(NOTCH_ACCENT));
                    ui.small(RichText::new(version).color(NOTCH_TEXT_MUTED));
                }
                CodexConnection::Offline { message } => {
                    paint_status_dot(ui, NOTCH_DANGER);
                    ui.small(RichText::new("LOCAL CODEX OFFLINE").color(NOTCH_DANGER));
                    ui.small(RichText::new(message).color(NOTCH_TEXT_MUTED));
                }
            }
        });

        // Whole bar (minus the window controls) is the drag handle.
        let drag_response = ui.interact(
            drag_rect,
            egui::Id::new("roche_titlebar_drag"),
            egui::Sense::click_and_drag(),
        );
        let is_maximized = ui.input(|input| input.viewport().maximized == Some(true));
        if drag_response.double_clicked() {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Maximized(!is_maximized));
        } else if drag_response.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }

        // Window controls on the right, drawn in the app's design-system style.
        let close_rect = egui::Rect::from_min_size(
            egui::pos2(bar_rect.max.x - button_w, bar_rect.top()),
            egui::vec2(button_w, bar_rect.height()),
        );
        let max_rect = egui::Rect::from_min_size(
            egui::pos2(bar_rect.max.x - button_w * 2.0, bar_rect.top()),
            egui::vec2(button_w, bar_rect.height()),
        );
        let min_rect = egui::Rect::from_min_size(
            egui::pos2(bar_rect.max.x - button_w * 3.0, bar_rect.top()),
            egui::vec2(button_w, bar_rect.height()),
        );
        self.window_control_button(ui, close_rect, WindowButton::Close, is_maximized);
        self.window_control_button(ui, max_rect, WindowButton::Maximize, is_maximized);
        self.window_control_button(ui, min_rect, WindowButton::Minimize, is_maximized);
    }

    fn window_control_button(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        kind: WindowButton,
        is_maximized: bool,
    ) {
        let is_close = matches!(kind, WindowButton::Close);
        let id = match kind {
            WindowButton::Minimize => egui::Id::new("roche_wc_min"),
            WindowButton::Maximize => egui::Id::new("roche_wc_max"),
            WindowButton::Close => egui::Id::new("roche_wc_close"),
        };
        let response = ui.interact(rect, id, egui::Sense::click());
        let active = response.hovered() || response.is_pointer_button_down_on();
        if active {
            ui.painter().rect_filled(
                rect,
                egui::CornerRadius::ZERO,
                if is_close {
                    NOTCH_DANGER
                } else {
                    NOTCH_BORDER_2
                },
            );
        }
        let color = if active { NOTCH_TEXT } else { NOTCH_TEXT_SUB };
        let stroke = egui::Stroke::new(1.6, color);
        let painter = ui.painter();
        let center = rect.center();
        match kind {
            WindowButton::Minimize => {
                painter.line_segment(
                    [
                        egui::pos2(center.x - 6.0, center.y),
                        egui::pos2(center.x + 6.0, center.y),
                    ],
                    stroke,
                );
            }
            WindowButton::Maximize => {
                if is_maximized {
                    painter.text(
                        center,
                        egui::Align2::CENTER_CENTER,
                        LUCIDE_COPY,
                        icon_font_id(),
                        color,
                    );
                } else {
                    painter.rect_stroke(
                        egui::Rect::from_center_size(center, egui::vec2(13.0, 13.0)),
                        2.0,
                        stroke,
                        egui::StrokeKind::Inside,
                    );
                }
            }
            WindowButton::Close => {
                let inset = 6.0;
                painter.line_segment(
                    [
                        egui::pos2(center.x - inset, center.y - inset),
                        egui::pos2(center.x + inset, center.y + inset),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [
                        egui::pos2(center.x - inset, center.y + inset),
                        egui::pos2(center.x + inset, center.y - inset),
                    ],
                    stroke,
                );
            }
        }
        if response.clicked() {
            let command = match kind {
                WindowButton::Minimize => egui::ViewportCommand::Minimized(true),
                WindowButton::Maximize => egui::ViewportCommand::Maximized(!is_maximized),
                WindowButton::Close => egui::ViewportCommand::Close,
            };
            ui.ctx().send_viewport_cmd(command);
        }
    }

    fn render_sidebar_content(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(icon_rich_text(LUCIDE_FOLDER, NOTCH_TEXT_MUTED));
            ui.label(RichText::new("작업공간").color(NOTCH_TEXT).strong());
            ui.small(
                RichText::new(format!("{}", self.workspace.workspaces.len()))
                    .color(NOTCH_TEXT_MUTED),
            );
            if ui
                .add(
                    egui::Button::new(icon_text_job(
                        "추가",
                        LUCIDE_FOLDER_PLUS,
                        false,
                        NOTCH_TEXT_SUB,
                    ))
                    .frame(false),
                )
                .on_hover_text("로컬 폴더 선택")
                .clicked()
            {
                self.open_workspace_picker();
            }
        });
        ui.separator();

        let web_account_height = 58.0;
        let splitter_height = 10.0;
        let section_height =
            (ui.available_height() - web_account_height - splitter_height).max(1.0);
        let min_workspace_height = 90.0_f32.min(section_height * 0.45);
        let min_quota_height = 120.0_f32.min(section_height * 0.55);
        let max_workspace_height = (section_height - min_quota_height).max(min_workspace_height);
        let list_height = (section_height * self.workspace.sidebar_workspace_ratio)
            .clamp(min_workspace_height, max_workspace_height);
        let quota_height = (section_height - list_height).max(min_quota_height);

        let mut activate = None;
        egui::ScrollArea::vertical()
            .id_salt("workspace_list")
            .max_height(list_height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for entry in &self.workspace.workspaces {
                    let selected =
                        self.workspace.selected_workspace.as_deref() == Some(entry.path.as_path());
                    let label = if selected {
                        RichText::new(&entry.name).color(NOTCH_TEXT).strong()
                    } else {
                        RichText::new(&entry.name).color(NOTCH_TEXT_SUB)
                    };
                    if ui.selectable_label(selected, label).clicked() {
                        activate = Some(entry.path.clone());
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.small(
                            RichText::new(entry.path.display().to_string()).color(NOTCH_TEXT_MUTED),
                        );
                        if selected {
                            let active = self
                                .sessions
                                .tabs
                                .iter()
                                .filter(|session| session.status.is_active())
                                .count();
                            ui.small(
                                RichText::new(format!("· 활성 세션 {active}"))
                                    .color(NOTCH_TEXT_MUTED),
                            );
                        }
                    });
                    ui.add_space(4.0);
                }
            });
        if let Some(path) = activate {
            self.activate_workspace(path);
        }

        let (splitter_rect, splitter_response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), splitter_height),
            egui::Sense::drag(),
        );
        let splitter_response = splitter_response.on_hover_cursor(egui::CursorIcon::ResizeVertical);
        let splitter_color = if splitter_response.dragged() || splitter_response.hovered() {
            NOTCH_TEXT_MUTED
        } else {
            NOTCH_BORDER_2
        };
        ui.painter().line_segment(
            [
                egui::pos2(splitter_rect.left(), splitter_rect.center().y),
                egui::pos2(splitter_rect.right(), splitter_rect.center().y),
            ],
            egui::Stroke::new(1.0, splitter_color),
        );
        let origin_id = splitter_response.id.with("sidebar_split_origin");
        if splitter_response.drag_started() {
            ui.data_mut(|data| data.insert_temp(origin_id, self.workspace.sidebar_workspace_ratio));
        }
        if splitter_response.dragged() {
            let origin = ui
                .data(|data| data.get_temp::<f32>(origin_id))
                .unwrap_or(self.workspace.sidebar_workspace_ratio);
            let min_ratio = (min_workspace_height / section_height).clamp(0.05, 0.8);
            let max_ratio = (1.0 - min_quota_height / section_height).clamp(min_ratio, 0.95);
            self.workspace.sidebar_workspace_ratio = (origin
                + splitter_response.drag_delta().y / section_height)
                .clamp(min_ratio, max_ratio);
        }
        if splitter_response.drag_stopped() {
            ui.data_mut(|data| data.remove::<f32>(origin_id));
        }

        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), quota_height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("sidebar_provider_quota")
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.render_ocx_dashboard(ui));
            },
        );
        ui.separator();
        self.render_web_account(ui);
    }

    fn render_web_account(&mut self, ui: &mut egui::Ui) {
        let (status, status_color, action) = match &self.runtime.web_browser_state {
            WebGptBrowserState::Starting => ("확인 중", NOTCH_TEXT_MUTED, "열기"),
            WebGptBrowserState::LoginRequired => ("로그인 필요", NOTCH_TEXT_MUTED, "로그인"),
            WebGptBrowserState::LoggedIn => ("로그인됨", NOTCH_GREEN, "계정"),
            WebGptBrowserState::Offline(_) => ("오프라인", NOTCH_TEXT_MUTED, "다시 열기"),
        };
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.strong(RichText::new("Web GPT").color(NOTCH_TEXT));
                ui.small(RichText::new(status).color(status_color));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("설정").clicked() {
                    self.workspace.selected_tab = WorkspaceTab::Settings;
                }
                if ui.small_button(action).clicked() {
                    if matches!(
                        self.runtime.web_browser_state,
                        WebGptBrowserState::Offline(_)
                    ) {
                        self.runtime.web_browser.reload();
                    }
                    self.runtime.web_browser.show_login();
                }
            });
        });
    }

    fn render_session_rename_dialog(&mut self, ctx: &egui::Context) {
        let Some(session_id) = self.sessions.rename_id.clone() else {
            return;
        };
        let mut open = true;
        let mut submit = false;
        let mut cancel = false;
        egui::Window::new("세션 이름 변경")
            .id(egui::Id::new(("session-rename", session_id.as_str())))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .open(&mut open)
            .show(ctx, |ui| {
                let response = ui.add(
                    TextEdit::singleline(&mut self.sessions.rename_draft)
                        .desired_width(280.0)
                        .hint_text("세션 이름"),
                );
                if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                ui.horizontal(|ui| {
                    if ui.button("취소").clicked() {
                        cancel = true;
                    }
                    if ui.button("변경").clicked() {
                        submit = true;
                    }
                });
            });
        if submit {
            let title = self.sessions.rename_draft.clone();
            self.rename_session_from_menu(&session_id, title);
        } else if cancel || !open {
            self.sessions.rename_id = None;
            self.sessions.rename_draft.clear();
        }
    }

    fn render_session_delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(session_id) = self.sessions.delete_confirm_id.clone() else {
            return;
        };
        let title = self
            .sessions
            .tabs
            .iter()
            .find(|session| session.id == session_id)
            .map(|session| session.title.clone())
            .unwrap_or_else(|| "이 세션".to_owned());
        let mut open = true;
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("세션 삭제")
            .id(egui::Id::new(("session-delete", session_id.as_str())))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!("‘{title}’ 세션을 삭제할까요?"));
                if self.is_native_local_session(&session_id) {
                    ui.small(
                        RichText::new("Codex 원본 기록은 삭제하지 않고 Roche 탭에서만 숨깁니다.")
                            .color(NOTCH_TEXT_MUTED),
                    );
                }
                ui.horizontal(|ui| {
                    if ui.button("취소").clicked() {
                        cancel = true;
                    }
                    if ui
                        .button(RichText::new("삭제").color(NOTCH_DANGER))
                        .clicked()
                    {
                        confirm = true;
                    }
                });
            });
        if confirm {
            self.delete_session_from_menu(&session_id);
        } else if cancel || !open {
            self.sessions.delete_confirm_id = None;
        }
    }

    fn render_orchestration_cards(&mut self, ui: &mut egui::Ui) {
        let Some(view) =
            orchestration_view(&self.sessions.tabs, self.sessions.selected_id.as_deref())
        else {
            return;
        };
        let mut activate = None;
        let mut approve = None;
        let mut revise = None;
        egui::ScrollArea::vertical()
            .id_salt("orchestration_cards")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                activate = self.render_orchestration_graph_card(ui, &view);
                ui.add_space(12.0);
                if let Some(session_id) = self.render_orchestration_progress_card(ui, &view) {
                    activate = Some(session_id);
                }
                if let Some(review_session) = view.review_session() {
                    ui.add_space(12.0);
                    let actions = self.render_orchestration_review_card(ui, review_session);
                    approve = actions.0;
                    revise = actions.1;
                }
            });
        if let Some(session_id) = activate.or(revise) {
            self.activate_session(session_id);
        }
        if let Some(session_id) = approve {
            self.runtime.webgpt.approve_worker(session_id);
            self.runtime_message = Some("워커 결과 승인 중…".to_owned());
        }
    }

    fn render_orchestration_graph_card(
        &self,
        ui: &mut egui::Ui,
        view: &OrchestrationView,
    ) -> Option<String> {
        let workers = visible_graph_workers(view, self.sessions.selected_id.as_deref());
        let selected_session_id = self.sessions.selected_id.as_deref();
        let mut clicked = None;
        egui::Frame::new()
            .fill(NOTCH_PANEL)
            .stroke(egui::Stroke::new(1.0, NOTCH_BORDER_2))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.strong(RichText::new("첫 세션이 집행 중").color(NOTCH_TEXT));
                ui.small(RichText::new("Roche 연결").color(NOTCH_TEXT_MUTED));
                ui.add_space(4.0);
                let graph_height = 218.0;
                let (graph_rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), graph_height),
                    egui::Sense::hover(),
                );
                let painter = ui.painter().clone();
                painter.rect_filled(
                    graph_rect,
                    egui::CornerRadius::same(6),
                    egui::Color32::from_rgb(0x1B, 0x1E, 0x24),
                );
                let root_rect = egui::Rect::from_center_size(
                    egui::pos2(graph_rect.center().x, graph_rect.top() + 43.0),
                    egui::vec2(136.0, 54.0),
                );
                let count = workers.len().max(1);
                let gap = 10.0;
                let child_width = ((graph_rect.width() - gap * (count.saturating_sub(1)) as f32)
                    / count as f32)
                    .clamp(72.0, 90.0);
                let row_width = child_width * count as f32 + gap * (count - 1) as f32;
                let first_x = graph_rect.center().x - row_width / 2.0;
                let child_y = graph_rect.bottom() - 48.0;
                let time = ui.input(|input| input.time);
                let mut child_rects = Vec::with_capacity(workers.len());
                for (index, worker) in workers.iter().enumerate() {
                    let rect = egui::Rect::from_min_size(
                        egui::pos2(first_x + index as f32 * (child_width + gap), child_y),
                        egui::vec2(child_width, 58.0),
                    );
                    child_rects.push(rect);
                    let from = root_rect.center_bottom();
                    let to = rect.center_top();
                    paint_dashed_connection(
                        &painter,
                        from,
                        to,
                        egui::Stroke::new(1.5, egui::Color32::from_rgb(0x58, 0x60, 0x6B)),
                    );
                    if matches!(
                        worker.status,
                        SessionStatus::Running | SessionStatus::WaitingOnWorkers
                    ) {
                        let phase = (time * 0.72 + index as f64 * 0.18).fract() as f32;
                        let pulse = from.lerp(to, phase);
                        painter.circle_filled(
                            pulse,
                            6.0,
                            egui::Color32::from_rgba_unmultiplied(0x6E, 0xE7, 0xA8, 55),
                        );
                        painter.circle_filled(pulse, 3.0, NOTCH_ACCENT);
                        ui.ctx().request_repaint_after(Duration::from_millis(16));
                    }
                }
                for (worker, rect) in workers.iter().zip(child_rects) {
                    let response = paint_orchestration_node(
                        ui,
                        &painter,
                        rect,
                        worker,
                        selected_session_id == Some(worker.id.as_str()),
                        false,
                    );
                    if response.clicked() {
                        clicked = Some(worker.id.clone());
                    }
                }
                let root_response = paint_orchestration_node(
                    ui,
                    &painter,
                    root_rect,
                    &view.root,
                    selected_session_id == Some(view.root.id.as_str()),
                    true,
                );
                if root_response.clicked() {
                    clicked = Some(view.root.id.clone());
                }
                if view.workers.len() > workers.len() {
                    painter.text(
                        graph_rect.right_bottom() - egui::vec2(8.0, 7.0),
                        egui::Align2::RIGHT_BOTTOM,
                        format!("+{} 세션", view.workers.len() - workers.len()),
                        egui::FontId::proportional(10.0),
                        NOTCH_TEXT_MUTED,
                    );
                }
                ui.add_space(7.0);
                ui.small(
                    RichText::new("청록 테두리 = 선택 세션   ·   이동 점 = 통신")
                        .color(NOTCH_TEXT_MUTED),
                );
            });
        clicked
    }

    fn render_orchestration_progress_card(
        &self,
        ui: &mut egui::Ui,
        view: &OrchestrationView,
    ) -> Option<String> {
        let mut clicked = None;
        egui::Frame::new()
            .fill(NOTCH_PANEL)
            .stroke(egui::Stroke::new(1.0, NOTCH_BORDER_2))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(RichText::new("진행 단계").color(NOTCH_TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.small(
                            RichText::new(format!(
                                "{} / {}",
                                view.completed_count(),
                                view.workers.len()
                            ))
                            .color(NOTCH_ACCENT),
                        );
                    });
                });
                let ratio = view.completed_count() as f32 / view.workers.len().max(1) as f32;
                let (bar_rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 5.0),
                    egui::Sense::hover(),
                );
                ui.painter()
                    .rect_filled(bar_rect, egui::CornerRadius::same(3), NOTCH_BAR_BG);
                if ratio > 0.0 {
                    let fill = egui::Rect::from_min_size(
                        bar_rect.min,
                        egui::vec2(bar_rect.width() * ratio, bar_rect.height()),
                    );
                    ui.painter()
                        .rect_filled(fill, egui::CornerRadius::same(3), NOTCH_ACCENT);
                }
                ui.add_space(3.0);
                for session in view.workers.iter().take(5) {
                    let selected =
                        self.sessions.selected_id.as_deref() == Some(session.id.as_str());
                    let fill = if selected {
                        egui::Color32::from_rgb(0x1C, 0x28, 0x2F)
                    } else {
                        egui::Color32::from_rgb(0x1B, 0x1E, 0x24)
                    };
                    let frame = egui::Frame::new()
                        .fill(fill)
                        .stroke(egui::Stroke::new(
                            if selected { 1.5 } else { 1.0 },
                            if selected {
                                NOTCH_CAUTION
                            } else {
                                NOTCH_BORDER
                            },
                        ))
                        .corner_radius(egui::CornerRadius::same(5))
                        .inner_margin(egui::Margin::symmetric(9, 7))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(session_status_symbol(session.status))
                                        .color(session_status_color(session.status)),
                                );
                                ui.vertical(|ui| {
                                    ui.small(
                                        RichText::new(truncate_label(&session.title, 22))
                                            .color(NOTCH_TEXT),
                                    );
                                    ui.small(
                                        RichText::new(format!(
                                            "{} · {}",
                                            session.runtime.label(),
                                            session_status_label(session.status)
                                        ))
                                        .color(session_status_color(session.status)),
                                    );
                                });
                            });
                        });
                    let response = ui.interact(
                        frame.response.rect,
                        ui.id().with(("orchestration_stage", session.id.as_str())),
                        egui::Sense::click(),
                    );
                    if response.clicked() {
                        clicked = Some(session.id.clone());
                    }
                    ui.add_space(6.0);
                }
                if view.workers.len() > 5 {
                    ui.small(
                        RichText::new(format!("외 {}개 세션", view.workers.len() - 5))
                            .color(NOTCH_TEXT_MUTED),
                    );
                }
            });
        clicked
    }

    fn render_orchestration_review_card(
        &self,
        ui: &mut egui::Ui,
        review_session: &AgentSession,
    ) -> (Option<String>, Option<String>) {
        let mut approve = None;
        let mut revise = None;
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(0x1E, 0x29, 0x27))
            .stroke(egui::Stroke::new(1.0, NOTCH_GREEN))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.small(RichText::new("NEEDS REVIEW").color(NOTCH_ACCENT).strong());
                ui.strong(RichText::new("워커 결과가 도착했습니다").color(NOTCH_TEXT));
                ui.small(
                    RichText::new(truncate_label(&review_session.title, 36)).color(NOTCH_TEXT_SUB),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("수정 요청").clicked() {
                        revise = Some(review_session.id.clone());
                    }
                    if ui
                        .add(
                            egui::Button::new(RichText::new("결과 승인").color(NOTCH_BG).strong())
                                .fill(NOTCH_GREEN),
                        )
                        .clicked()
                    {
                        approve = Some(review_session.id.clone());
                    }
                });
            });
        (approve, revise)
    }

    fn render_workspace_content(&mut self, ui: &mut egui::Ui) {
        let session_tabs = self.sessions.tabs.clone();
        ui.horizontal_wrapped(|ui| {
            if session_tabs.is_empty() {
                ui.selectable_value(&mut self.workspace.selected_tab, WorkspaceTab::Chat, "Main");
            } else {
                for session in session_tabs {
                    let selected = self.workspace.selected_tab == WorkspaceTab::Chat
                        && self.sessions.selected_id.as_deref() == Some(session.id.as_str());
                    let runtime = match session.runtime {
                        SessionRuntime::Unified => "Main",
                        SessionRuntime::WebGpt => "WEB",
                        SessionRuntime::Codex => "Codex",
                    };
                    let title = if session.parent_session_id.is_none() {
                        session.title.clone()
                    } else {
                        format!("{runtime} · {}", session.title)
                    };
                    let response = ui.selectable_label(selected, title);
                    if response.clicked() {
                        let session_id = session.id.clone();
                        self.activate_session(session_id);
                    }
                    let session_id = session.id.clone();
                    let session_title = session.title.clone();
                    let is_primary =
                        self.primary_main_session_id().as_deref() == Some(session_id.as_str());
                    response.context_menu(|ui| {
                        if ui.button("이름 변경").clicked() {
                            self.sessions.rename_id = Some(session_id.clone());
                            self.sessions.rename_draft = session_title.clone();
                            self.sessions.delete_confirm_id = None;
                            ui.close();
                        }
                        ui.separator();
                        if is_primary {
                            ui.add_enabled(false, egui::Button::new("삭제"))
                                .on_disabled_hover_text("Main 세션은 삭제할 수 없습니다.");
                        } else if ui
                            .button(RichText::new("삭제").color(NOTCH_DANGER))
                            .clicked()
                        {
                            self.sessions.delete_confirm_id = Some(session_id.clone());
                            self.sessions.rename_id = None;
                            ui.close();
                        }
                    });
                }
            }
            ui.separator();
            if ui
                .add(
                    egui::Button::new(RichText::new("+").size(18.0).color(NOTCH_TEXT_SUB))
                        .frame(false)
                        .min_size(egui::vec2(UI_CONTROL_HEIGHT, UI_CONTROL_HEIGHT)),
                )
                .on_hover_text("현재 프로젝트로 새 세션")
                .clicked()
            {
                let title = self
                    .workspace
                    .selected_workspace
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "New session".to_owned());
                self.runtime.webgpt.create_session(title);
                self.runtime_message = Some("새 세션 생성 중…".to_owned());
            }
        });
        self.render_session_rename_dialog(ui.ctx());
        self.render_session_delete_dialog(ui.ctx());
        if self.workspace.selected_tab != WorkspaceTab::Chat {
            if let Some(message) = self.runtime_message.as_deref() {
                ui.small(message);
            }
            ui.separator();
        }

        match self.workspace.selected_tab {
            WorkspaceTab::Chat => {
                let avail = ui.available_rect_before_wrap();
                let chat_w = avail.width().min(CHAT_ASSISTANT_MAX_WIDTH);
                let chat_rect = egui::Rect::from_min_size(
                    egui::pos2(avail.left() + (avail.width() - chat_w) / 2.0, avail.top()),
                    egui::vec2(chat_w, chat_content_height(avail.height())),
                );
                let mut chat_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(chat_rect)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                self.render_chat(&mut chat_ui);
            }
            WorkspaceTab::Settings => self.render_settings(ui),
        }
    }
}

impl DesktopApp {
    fn persisted_state(&self) -> PersistedDesktopState {
        PersistedDesktopState {
            selected_workspace: self.workspace.selected_workspace.clone(),
            selected_tab: Some(self.workspace.selected_tab),
            selected_session_id: self.sessions.selected_id.clone(),
            selected_model: Some(self.composer.selected_model),
            reasoning_effort: Some(self.composer.reasoning_effort.clone()),
            expanded_providers: self.ocx_ui.expanded_providers.iter().cloned().collect(),
            provider_order: self.ocx_ui.provider_order.clone(),
            session_tabs: self.sessions.tabs.clone(),
            chat_messages: self
                .sessions
                .chat_messages
                .iter()
                .map(|(session_id, messages)| {
                    let messages = messages
                        .iter()
                        .map(|message| PersistedChatMessage {
                            role: message.role,
                            model: message.model,
                            text: message.text.clone(),
                            activity: message.activity.clone(),
                        })
                        .collect();
                    (session_id.clone(), messages)
                })
                .collect(),
            codex_session_threads: self.sessions.codex_session_threads.clone(),
            hidden_session_ids: self.sessions.hidden_ids.clone(),
            session_title_overrides: self.sessions.title_overrides.clone(),
            sidebar_workspace_ratio: Some(self.workspace.sidebar_workspace_ratio),
        }
    }
}

impl Drop for DesktopApp {
    fn drop(&mut self) {
        save_desktop_state(&self.workspace.state_store, &self.persisted_state());
    }
}

impl eframe::App for DesktopApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Keep the app theme authoritative even if the native backend reapplies
        // its default visuals after viewport changes or a fullscreen transition.
        apply_notch_theme(ui.ctx());
        self.drain_codex();
        self.drain_webgpt();
        self.drain_web_browser();
        self.drain_workspace_picker();
        self.drain_ocx();
        self.drain_telemetry();
        self.sample_memory();
        self.poll_accounts();

        let margin = safe_area_margin(ui.ctx());
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(NOTCH_BG).inner_margin(margin))
            .show(ui, |ui| {
                egui::Panel::top("top_bar")
                    .default_size(36.0)
                    .min_size(36.0)
                    .max_size(36.0)
                    .frame(
                        egui::Frame::NONE
                            .fill(NOTCH_PANEL)
                            .stroke(egui::Stroke::new(1.0, NOTCH_BORDER))
                            .inner_margin(egui::Margin::symmetric(12, 0)),
                    )
                    .show(ui, |ui| self.render_top_bar_content(ui));
                egui::Panel::left("workspace_sidebar")
                    .resizable(true)
                    .default_size(300.0)
                    .min_size(230.0)
                    .max_size(440.0)
                    .frame(
                        egui::Frame::NONE
                            .fill(NOTCH_BG)
                            .stroke(egui::Stroke::new(1.0, NOTCH_BORDER))
                            .inner_margin(egui::Margin::symmetric(12, 10)),
                    )
                    .show(ui, |ui| self.render_sidebar_content(ui));
                let show_orchestration_cards = self.workspace.selected_tab == WorkspaceTab::Chat
                    && ui.available_width() >= ORCHESTRATION_RAIL_MIN_AVAILABLE_WIDTH
                    && orchestration_view(
                        &self.sessions.tabs,
                        self.sessions.selected_id.as_deref(),
                    )
                    .is_some();
                if show_orchestration_cards {
                    egui::Panel::right("orchestration_cards")
                        .resizable(false)
                        .show_separator_line(false)
                        .default_size(ORCHESTRATION_RAIL_WIDTH)
                        .min_size(ORCHESTRATION_RAIL_WIDTH)
                        .max_size(ORCHESTRATION_RAIL_WIDTH)
                        .frame(egui::Frame::NONE.fill(NOTCH_BG).inner_margin(egui::Margin {
                            left: 14,
                            right: 14,
                            top: 14,
                            bottom: 14,
                        }))
                        .show(ui, |ui| self.render_orchestration_cards(ui));
                }
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE.fill(NOTCH_BG).inner_margin(egui::Margin {
                        left: 18,
                        right: 18,
                        top: 14,
                        bottom: 26,
                    }))
                    .show(ui, |ui| self.render_workspace_content(ui));
            });
        if self.workspace.last_state_save.elapsed() >= Duration::from_millis(500) {
            save_desktop_state(&self.workspace.state_store, &self.persisted_state());
            self.workspace.last_state_save = Instant::now();
        }
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
}

fn paint_dashed_connection(
    painter: &egui::Painter,
    from: egui::Pos2,
    to: egui::Pos2,
    stroke: egui::Stroke,
) {
    let delta = to - from;
    let length = delta.length();
    if length <= f32::EPSILON {
        return;
    }
    let direction = delta / length;
    let mut offset = 0.0;
    const DASH: f32 = 5.0;
    const GAP: f32 = 5.0;
    while offset < length {
        let end = (offset + DASH).min(length);
        painter.line_segment([from + direction * offset, from + direction * end], stroke);
        offset += DASH + GAP;
    }
}

fn paint_orchestration_node(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    rect: egui::Rect,
    session: &AgentSession,
    selected: bool,
    is_root: bool,
) -> egui::Response {
    let response = ui.interact(
        rect,
        ui.id()
            .with(("orchestration_graph_node", session.id.as_str())),
        egui::Sense::click(),
    );
    let fill = if selected {
        egui::Color32::from_rgb(0x1C, 0x28, 0x2F)
    } else if response.hovered() {
        NOTCH_BORDER
    } else {
        NOTCH_PANEL
    };
    let border = if selected {
        NOTCH_CAUTION
    } else {
        NOTCH_BORDER_2
    };
    painter.rect_filled(rect, egui::CornerRadius::same(7), fill);
    painter.rect_stroke(
        rect,
        egui::CornerRadius::same(7),
        egui::Stroke::new(if selected { 2.0 } else { 1.0 }, border),
        egui::StrokeKind::Inside,
    );
    let title = truncate_label(&session.title, if is_root { 18 } else { 12 });
    painter.text(
        egui::pos2(
            rect.center().x,
            rect.top() + if is_root { 17.0 } else { 16.0 },
        ),
        egui::Align2::CENTER_CENTER,
        title,
        egui::FontId::proportional(if is_root { 12.0 } else { 11.0 }),
        if selected { NOTCH_CAUTION } else { NOTCH_TEXT },
    );
    let detail = if is_root {
        session.runtime.label().to_owned()
    } else {
        format!(
            "{} {}",
            session_status_symbol(session.status),
            session_status_label(session.status)
        )
    };
    painter.text(
        egui::pos2(
            rect.center().x,
            rect.bottom() - if is_root { 16.0 } else { 17.0 },
        ),
        egui::Align2::CENTER_CENTER,
        detail,
        egui::FontId::proportional(9.0),
        if is_root {
            NOTCH_TEXT_MUTED
        } else {
            session_status_color(session.status)
        },
    );
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(format!(
            "{}\n{} · {}",
            session.title,
            session.runtime.label(),
            session_status_label(session.status)
        ))
}

fn session_status_label(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Idle => "대기",
        SessionStatus::Running => "실행 중",
        SessionStatus::WaitingOnWorkers => "준비 중",
        SessionStatus::NeedsInput => "검토 필요",
        SessionStatus::Completed => "완료",
        SessionStatus::Failed => "실패",
        SessionStatus::Cancelled => "취소",
        SessionStatus::Offline => "오프라인",
    }
}

fn session_status_symbol(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Running | SessionStatus::WaitingOnWorkers => "●",
        SessionStatus::NeedsInput => "!",
        SessionStatus::Completed => "✓",
        SessionStatus::Failed => "×",
        SessionStatus::Cancelled => "−",
        SessionStatus::Idle | SessionStatus::Offline => "○",
    }
}

fn session_status_color(status: SessionStatus) -> egui::Color32 {
    match status {
        SessionStatus::Running | SessionStatus::WaitingOnWorkers => NOTCH_ACCENT,
        SessionStatus::NeedsInput => NOTCH_CAUTION,
        SessionStatus::Failed => NOTCH_DANGER,
        SessionStatus::Completed
        | SessionStatus::Cancelled
        | SessionStatus::Idle
        | SessionStatus::Offline => NOTCH_TEXT_MUTED,
    }
}

fn truncate_label(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let compact = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{compact}…")
    } else {
        compact
    }
}

fn icon_font_id() -> egui::FontId {
    egui::FontId::new(
        UI_FONT_ICON,
        egui::FontFamily::Name(ICON_FONT_FAMILY.into()),
    )
}

fn body_text_format(color: egui::Color32) -> egui::TextFormat {
    egui::TextFormat {
        font_id: egui::FontId::new(UI_FONT_BODY, egui::FontFamily::Proportional),
        line_height: Some(UI_LINE_HEIGHT),
        color,
        valign: egui::Align::Center,
        ..Default::default()
    }
}

fn icon_text_format(color: egui::Color32) -> egui::TextFormat {
    egui::TextFormat {
        font_id: icon_font_id(),
        line_height: Some(UI_LINE_HEIGHT),
        color,
        valign: egui::Align::Center,
        ..Default::default()
    }
}

fn icon_rich_text(icon: char, color: egui::Color32) -> RichText {
    RichText::new(icon.to_string())
        .font(icon_font_id())
        .color(color)
}

fn icon_text_job(
    text: &str,
    icon: char,
    icon_on_right: bool,
    color: egui::Color32,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    if icon_on_right {
        job.append(text, 0.0, body_text_format(color));
        job.append(&icon.to_string(), 8.0, icon_text_format(color));
    } else {
        job.append(&icon.to_string(), 0.0, icon_text_format(color));
        job.append(text, 6.0, body_text_format(color));
    }
    job
}

fn model_settings_job(model: &str, reasoning: &str) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    job.append(model, 0.0, body_text_format(NOTCH_TEXT_SUB));
    job.append(reasoning, 10.0, body_text_format(NOTCH_TEXT_MUTED));
    job.append(
        &LUCIDE_CHEVRON_DOWN.to_string(),
        8.0,
        icon_text_format(NOTCH_TEXT_MUTED),
    );
    job
}

fn paint_status_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, UI_LINE_HEIGHT), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.0, color);
}

fn account_reauth_eligible(provider: &str, account: &ProviderAccount) -> bool {
    account.needs_reauth
        && matches!(account.kind.as_str(), "codex" | "oauth")
        && !(provider == "openai" && (account.is_main || account.id == "__main__"))
}

fn account_pool_control(
    ui: &mut egui::Ui,
    play: bool,
    enabled: bool,
    busy: bool,
) -> egui::Response {
    let sense = if enabled && !busy {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(UI_CONTROL_HEIGHT, UI_CONTROL_HEIGHT), sense);
    let color = if busy || !enabled {
        NOTCH_TEXT_MUTED
    } else if response.hovered() || response.is_pointer_button_down_on() {
        NOTCH_TEXT
    } else if play {
        NOTCH_ACCENT
    } else {
        NOTCH_TEXT_MUTED
    };
    let center = rect.center();
    if play {
        ui.painter().add(egui::Shape::convex_polygon(
            vec![
                egui::pos2(center.x - 4.5, center.y - 6.0),
                egui::pos2(center.x - 4.5, center.y + 6.0),
                egui::pos2(center.x + 6.0, center.y),
            ],
            color,
            egui::Stroke::NONE,
        ));
    } else {
        ui.painter().rect_filled(
            egui::Rect::from_center_size(
                egui::pos2(center.x - 3.5, center.y),
                egui::vec2(3.0, 12.0),
            ),
            0.5,
            color,
        );
        ui.painter().rect_filled(
            egui::Rect::from_center_size(
                egui::pos2(center.x + 3.5, center.y),
                egui::vec2(3.0, 12.0),
            ),
            0.5,
            color,
        );
    }
    response
}

fn drag_handle(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(16.0, UI_CONTROL_HEIGHT), egui::Sense::drag());
    let color = if response.hovered() {
        NOTCH_TEXT_SUB
    } else {
        NOTCH_TEXT_MUTED
    };
    let center = rect.center();
    for y in [-4.0, 0.0, 4.0] {
        for x in [-2.5, 2.5] {
            ui.painter()
                .circle_filled(egui::pos2(center.x + x, center.y + y), 1.1, color);
        }
    }
    response.on_hover_cursor(egui::CursorIcon::Grab)
}

fn draw_power_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    use std::f32::consts::{FRAC_PI_2, TAU};
    let center = egui::pos2(rect.center().x, rect.center().y + 1.0);
    let radius = 6.0;
    let stroke = egui::Stroke::new(2.2, color);
    // Ring open at the visual top (the classic power symbol), then a stub up.
    let half_gap = 0.32f32;
    let start = (FRAC_PI_2 * 3.0) + half_gap;
    let end = start + TAU - half_gap * 2.0;
    let steps = 48;
    let mut points = Vec::with_capacity(steps + 1);
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let phi = start + (end - start) * t;
        points.push(egui::pos2(
            center.x + radius * phi.cos(),
            center.y + radius * phi.sin(),
        ));
    }
    painter.add(egui::Shape::line(points, stroke));
    painter.line_segment([center, egui::pos2(center.x, center.y - radius)], stroke);
}

fn format_quota_percent(percent: f64) -> String {
    format!("{:.0}%", percent.clamp(0.0, 100.0))
}

fn quota_head(bar: &QuotaBar) -> String {
    match (bar.value_label.as_deref(), format_reset(bar.reset_at)) {
        (Some(value), Some(reset)) => format!("{} · {value} · {reset}", bar.label),
        (Some(value), None) => format!("{} · {value}", bar.label),
        (None, Some(reset)) => format!("{} · {reset}", bar.label),
        (None, None) => bar.label.clone(),
    }
}

fn format_reset(reset_at: Option<f64>) -> Option<String> {
    let reset_at = reset_at?;
    if !reset_at.is_finite() {
        return None;
    }
    let reset_ms = if reset_at < 10_000_000_000.0 {
        reset_at * 1000.0
    } else {
        reset_at
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    let remaining = ((reset_ms - now_ms) / 1000.0).max(0.0) as u64;
    let days = remaining / 86_400;
    let hours = (remaining % 86_400) / 3_600;
    let minutes = (remaining % 3_600) / 60;
    if days > 0 {
        Some(format!("resets {days}d {hours}h"))
    } else if hours > 0 {
        Some(format!("resets {hours}h {minutes}m"))
    } else if minutes > 0 {
        Some(format!("resets {minutes}m"))
    } else {
        Some("resets now".into())
    }
}

fn safe_area_margin(ctx: &egui::Context) -> egui::Margin {
    // A maximized Windows window is already constrained to the work area. Only
    // borderless true fullscreen needs the taskbar/work-area inset applied.
    let fullscreen = ctx.input(|input| input.viewport().fullscreen == Some(true));
    if !fullscreen {
        return egui::Margin::ZERO;
    }
    let scale = ctx.pixels_per_point();
    let [top, right, bottom, left] = primary_work_area_insets_px();
    egui::Margin {
        left: (left / scale) as i8,
        right: (right / scale) as i8,
        top: (top / scale) as i8,
        bottom: (bottom / scale) as i8,
    }
}

#[cfg(windows)]
fn primary_work_area_insets_px() -> [f32; 4] {
    use windows_sys::Win32::{
        Foundation::RECT,
        UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN, SPI_GETWORKAREA, SystemParametersInfoW,
        },
    };
    unsafe {
        let mut work = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            &mut work as *mut RECT as *mut core::ffi::c_void,
            0,
        ) == 0
        {
            return [0.0; 4];
        }
        let width = GetSystemMetrics(SM_CXSCREEN) as f32;
        let height = GetSystemMetrics(SM_CYSCREEN) as f32;
        [
            work.top.max(0) as f32,
            (width - work.right as f32).max(0.0),
            (height - work.bottom as f32).max(0.0),
            work.left.max(0) as f32,
        ]
    }
}

#[cfg(not(windows))]
fn primary_work_area_insets_px() -> [f32; 4] {
    [0.0; 4]
}

fn workspaces_store_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default();
    base.join("roche").join("workspaces.json")
}

fn load_workspaces(store: &Path) -> Vec<WorkspaceEntry> {
    let Ok(text) = std::fs::read_to_string(store) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<WorkspaceEntry>>(&text).unwrap_or_default()
}

fn save_workspaces(store: &Path, workspaces: &[WorkspaceEntry]) {
    let Some(parent) = store.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(parent);
    let Ok(json) = serde_json::to_string_pretty(workspaces) else {
        return;
    };
    let _ = std::fs::write(store, json);
}

fn desktop_state_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default();
    base.join("roche").join("desktop-state.json")
}

fn codex_thread_title(thread: &CodexStoredThread) -> String {
    if let Some(name) = thread
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return name.chars().take(48).collect();
    }
    let compact = thread
        .preview
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if compact.is_empty() {
        "Codex session".to_owned()
    } else {
        compact.chars().take(48).collect()
    }
}

fn native_worker_title(detail: &str) -> String {
    let compact = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty()
        || matches!(
            compact.as_str(),
            "spawn_agent" | "send_input" | "resume_agent" | "wait" | "close_agent"
        )
    {
        return "Codex worker".to_owned();
    }
    const MAX_CHARS: usize = 36;
    let mut title = compact.chars().take(MAX_CHARS).collect::<String>();
    if compact.chars().count() > MAX_CHARS {
        title.push('…');
    }
    title
}

fn load_desktop_state(store: &Path) -> PersistedDesktopState {
    let Ok(text) = std::fs::read_to_string(store) else {
        return PersistedDesktopState::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_desktop_state(store: &Path, state: &PersistedDesktopState) {
    let Some(parent) = store.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(parent);
    let Ok(json) = serde_json::to_string_pretty(state) else {
        return;
    };
    let temp = store.with_extension("json.tmp");
    if std::fs::write(&temp, json).is_err() {
        return;
    }
    if std::fs::rename(&temp, store).is_err() {
        let _ = std::fs::remove_file(store);
        if std::fs::rename(&temp, store).is_err() {
            let _ = std::fs::remove_file(&temp);
        }
    }
}

/// Read a bitmap from the clipboard, persist it as a PNG, and keep a texture preview for the draft.
fn save_clipboard_image(ctx: &egui::Context) -> Option<DraftAttachment> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let image = clipboard.get_image().ok()?;
    let [width, height] = [image.width, image.height];
    if width == 0 || height == 0 {
        return None;
    }
    let bytes = image.bytes.into_owned();
    let color_image = egui::ColorImage::from_rgba_unmultiplied([width, height], &bytes);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let directory = std::env::temp_dir().join("Roche").join("attachments");
    std::fs::create_dir_all(&directory).ok()?;
    let path = directory.join(format!("clipboard-{stamp}.png"));
    let file = std::fs::File::create(&path).ok()?;
    let mut encoder = png::Encoder::new(file, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(&bytes).ok()?;
    drop(writer);
    let texture = ctx.load_texture(
        format!("roche-pasted-{stamp}"),
        color_image,
        egui::TextureOptions::LINEAR,
    );
    Some(DraftAttachment {
        path,
        preview: Some(texture),
    })
}

#[cfg(windows)]
fn clipboard_file_paths() -> Vec<PathBuf> {
    use windows_sys::Win32::{
        System::{DataExchange, Ole},
        UI::Shell,
    };

    unsafe {
        if DataExchange::IsClipboardFormatAvailable(Ole::CF_HDROP as u32) == 0
            || DataExchange::OpenClipboard(std::ptr::null_mut()) == 0
        {
            return Vec::new();
        }
        let handle = DataExchange::GetClipboardData(Ole::CF_HDROP as u32);
        if handle.is_null() {
            let _ = DataExchange::CloseClipboard();
            return Vec::new();
        }
        let drop_handle = handle as Shell::HDROP;
        let count = Shell::DragQueryFileW(drop_handle, u32::MAX, std::ptr::null_mut(), 0);
        let mut paths = Vec::with_capacity(count as usize);
        for index in 0..count {
            let length = Shell::DragQueryFileW(drop_handle, index, std::ptr::null_mut(), 0);
            if length == 0 {
                continue;
            }
            let mut buffer = vec![0u16; length as usize + 1];
            let written =
                Shell::DragQueryFileW(drop_handle, index, buffer.as_mut_ptr(), buffer.len() as u32);
            if written > 0 {
                paths.push(PathBuf::from(String::from_utf16_lossy(
                    &buffer[..written as usize],
                )));
            }
        }
        let _ = DataExchange::CloseClipboard();
        paths
    }
}

#[cfg(not(windows))]
fn clipboard_file_paths() -> Vec<PathBuf> {
    Vec::new()
}

fn install_runtime_fonts(ctx: &egui::Context) {
    let korean_candidates = [
        PathBuf::from(r"C:\Windows\Fonts\malgun.ttf"),
        PathBuf::from(r"C:\Windows\Fonts\malgunsl.ttf"),
    ];

    let mut fonts = egui::FontDefinitions::default();
    let mut korean_name = None;
    if let Some(bytes) = korean_candidates
        .iter()
        .find_map(|path| std::fs::read(path).ok())
    {
        let font_name = "roche-korean".to_owned();
        fonts
            .font_data
            .insert(font_name.clone(), egui::FontData::from_owned(bytes).into());
        korean_name = Some(font_name);
    }

    // Segoe UI as the Latin primary to match the ocx-notch look.
    let mut latin_name = None;
    if let Ok(bytes) = std::fs::read(r"C:\Windows\Fonts\segoeui.ttf") {
        let font_name = "roche-latin".to_owned();
        fonts
            .font_data
            .insert(font_name.clone(), egui::FontData::from_owned(bytes).into());
        latin_name = Some(font_name);
    }

    // Lucide icons: chevrons, gauge, folder(s), refresh.
    const LUCIDE_ICONS: &[u8] = include_bytes!("../../../assets/lucide-icons.ttf");
    let lucide_name = "roche-lucide".to_owned();
    fonts.font_data.insert(
        lucide_name.clone(),
        egui::FontData::from_owned(LUCIDE_ICONS.to_vec()).into(),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = fonts.families.entry(family.clone()).or_default();
        if let Some(latin_name) = &latin_name {
            list.insert(0, latin_name.clone());
        }
        if let Some(korean_name) = &korean_name {
            list.push(korean_name.clone());
        }
    }
    fonts.families.insert(
        egui::FontFamily::Name(ICON_FONT_FAMILY.into()),
        vec![lucide_name],
    );
    ctx.set_fonts(fonts);
}

fn apply_notch_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = NOTCH_BG;
    visuals.window_fill = NOTCH_BG;
    visuals.extreme_bg_color = NOTCH_PANEL;
    visuals.code_bg_color = NOTCH_PANEL;
    visuals.faint_bg_color = NOTCH_PANEL;
    visuals.override_text_color = Some(NOTCH_TEXT);
    visuals.hyperlink_color = NOTCH_ACCENT;
    visuals.selection.bg_fill = egui::Color32::from_rgb(59, 99, 153);
    visuals.selection.stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(129, 174, 229));
    // Keep the insertion marker from covering Korean IME preedit glyphs.
    visuals.text_cursor.stroke = egui::Stroke::new(1.0, NOTCH_TEXT_MUTED);
    // Only the real blinking caret; no gray hover-preview cursor.
    visuals.text_cursor.preview = false;
    // Korean IME composition: use the modern underline style instead of the
    // legacy gray selection-block visuals (which Windows defaults to).
    visuals.ime_composition.legacy_visuals = false;
    visuals.ime_composition.active_underline_stroke = egui::Stroke::new(1.0, NOTCH_TEXT_MUTED);
    visuals.ime_composition.inactive_underline_stroke = egui::Stroke::new(1.0, NOTCH_TEXT_MUTED);
    visuals.window_stroke = egui::Stroke::new(1.0, NOTCH_BORDER);
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, NOTCH_TEXT_SUB);
    visuals.widgets.noninteractive.bg_fill = NOTCH_BG;
    visuals.widgets.inactive.weak_bg_fill = NOTCH_PANEL;
    visuals.widgets.inactive.bg_fill = NOTCH_PANEL;
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, NOTCH_TEXT_SUB);
    visuals.widgets.hovered.weak_bg_fill = NOTCH_BORDER;
    visuals.widgets.hovered.bg_fill = NOTCH_BORDER;
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.5, NOTCH_TEXT);
    visuals.widgets.active.weak_bg_fill = NOTCH_BORDER_2;
    visuals.widgets.active.bg_fill = NOTCH_BORDER_2;
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.5, NOTCH_ACCENT);
    visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(4);
    visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(4);
    visuals.widgets.active.corner_radius = egui::CornerRadius::same(4);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::new(UI_FONT_BODY, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        egui::FontId::new(UI_FONT_BODY, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        egui::FontId::new(UI_FONT_SMALL, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Heading,
        egui::FontId::new(UI_FONT_HEADING, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        egui::FontId::new(UI_FONT_MONO, egui::FontFamily::Monospace),
    );
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(7.0, 4.0);
    style.spacing.interact_size.y = UI_CONTROL_HEIGHT;
    style.spacing.icon_width = 14.0;
    style.spacing.icon_width_inner = 8.0;
    style.spacing.icon_spacing = 6.0;
    ctx.set_style_of(egui::Theme::Dark, style);
}

fn native_web_session_for_correlation<'a>(
    web_local_sessions: &'a HashMap<String, String>,
    correlation: &WebGptTurnCorrelation,
) -> Option<&'a String> {
    if correlation.account_id != DEFAULT_LOCAL_WEB_GPT_ACCOUNT_ID || correlation.task_id.is_some() {
        return None;
    }
    web_local_sessions
        .get(&correlation.request_id)
        .filter(|session_id| *session_id == &correlation.session_id)
}

fn purge_web_local_session_ownership(
    removed_session_ids: &[String],
    web_local_sessions: &mut HashMap<String, String>,
    web_local_correlations: &mut HashMap<String, WebGptTurnCorrelation>,
) {
    let removed = removed_session_ids.iter().collect::<HashSet<_>>();
    let request_ids = web_local_sessions
        .iter()
        .filter(|(_, session_id)| removed.contains(session_id))
        .map(|(request_id, _)| request_id.clone())
        .collect::<Vec<_>>();
    for request_id in request_ids {
        web_local_sessions.remove(&request_id);
        web_local_correlations.remove(&request_id);
    }
}

fn native_web_correlation_matches(
    web_local_correlations: &HashMap<String, WebGptTurnCorrelation>,
    correlation: &WebGptTurnCorrelation,
) -> bool {
    web_local_correlations
        .get(&correlation.request_id)
        .is_some_and(|expected| expected == correlation)
}

fn latch_native_web_correlation(
    web_local_sessions: &HashMap<String, String>,
    web_local_correlations: &mut HashMap<String, WebGptTurnCorrelation>,
    correlation: &WebGptTurnCorrelation,
) -> Option<String> {
    let session_id = native_web_session_for_correlation(web_local_sessions, correlation)?.clone();
    if web_local_correlations.contains_key(&correlation.request_id) {
        if !native_web_correlation_matches(web_local_correlations, correlation) {
            return None;
        }
    } else {
        web_local_correlations.insert(correlation.request_id.clone(), correlation.clone());
    }
    Some(session_id)
}

fn apply_web_answer(
    web_local_sessions: &mut HashMap<String, String>,
    chat_messages: &mut HashMap<String, Vec<ChatMessage>>,
    request_id: &str,
    text: String,
) -> bool {
    let Some(session_id) = web_local_sessions.remove(request_id) else {
        return false;
    };
    let text = sanitize_web_assistant_text(&text);
    let messages = chat_messages.entry(session_id).or_default();
    if let Some(message) = messages.iter_mut().rev().find(|message| {
        message.role == ChatRole::Assistant
            && message.model == ChatModel::WebGpt56Sol
            && message.turn_id.as_deref() == Some(request_id)
    }) {
        message.text = text;
        message.streaming = false;
    } else {
        messages.push(ChatMessage {
            role: ChatRole::Assistant,
            model: ChatModel::WebGpt56Sol,
            text,
            turn_id: Some(request_id.to_owned()),
            streaming: false,
            image: None,
            activity: None,
        });
    }
    for message in messages.iter_mut().rev() {
        if message.turn_id.as_deref() == Some(request_id) {
            message.streaming = false;
            if let Some(group) = message.activity.as_mut() {
                for entry in &mut group.entries {
                    if entry.phase == CodexActivityPhase::Running {
                        entry.phase = CodexActivityPhase::Completed;
                    }
                }
            }
        }
    }
    true
}

fn finish_web_local_session(
    web_local_sessions: &mut HashMap<String, String>,
    chat_messages: &mut HashMap<String, Vec<ChatMessage>>,
    request_id: &str,
) -> bool {
    let Some(session_id) = web_local_sessions.remove(request_id) else {
        return false;
    };
    if let Some(messages) = chat_messages.get_mut(&session_id) {
        for message in messages.iter_mut().rev() {
            if message.turn_id.as_deref() == Some(request_id) {
                message.streaming = false;
                if let Some(group) = message.activity.as_mut() {
                    for entry in &mut group.entries {
                        if entry.phase == CodexActivityPhase::Running {
                            entry.phase = CodexActivityPhase::Failed;
                        }
                    }
                }
            }
        }
    }
    true
}

fn sanitize_web_assistant_text(value: &str) -> String {
    value
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            let lower = trimmed.to_ascii_lowercase();
            let codex_runtime_noise = lower.starts_with("codex:")
                && (lower.contains("error")
                    || lower.contains("warn")
                    || lower.contains("failed to connect")
                    || lower.contains("websocket"));
            !lower.starts_with("inprogress:")
                && !lower.starts_with("completed:")
                && !lower.starts_with("failed:")
                && !lower.starts_with("warning:")
                && !lower.starts_with("warnings:")
                && !codex_runtime_noise
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

fn web_activity_from_visible_text(request_id: &str, value: &str) -> Option<CodexActivity> {
    let compact = value.trim();
    if compact.is_empty() {
        return None;
    }
    let lower = compact.to_ascii_lowercase();
    if lower.starts_with("warnings:") || lower.starts_with("warning:") {
        return None;
    }

    let (phase, detail) = if lower.starts_with("inprogress:") {
        (
            CodexActivityPhase::Running,
            compact.split_once(':')?.1.trim(),
        )
    } else if lower.starts_with("completed:") {
        (
            CodexActivityPhase::Completed,
            compact.split_once(':')?.1.trim(),
        )
    } else if lower.starts_with("failed:") {
        (
            CodexActivityPhase::Failed,
            compact.split_once(':')?.1.trim(),
        )
    } else if compact.ends_with(" 완료") {
        (CodexActivityPhase::Completed, compact)
    } else if compact.ends_with(" 실패") {
        (CodexActivityPhase::Failed, compact)
    } else {
        (CodexActivityPhase::Running, compact)
    };
    let detail_lower = detail.to_ascii_lowercase();
    let kind = if detail_lower.contains("pwsh.exe")
        || detail_lower.contains("powershell")
        || detail_lower.contains("cmd.exe")
        || detail_lower.contains("bash")
        || detail_lower.contains("rtk ")
        || detail_lower.contains("cargo ")
        || detail_lower.contains("git ")
    {
        CodexActivityKind::Terminal
    } else if detail_lower.contains("spawn_agent")
        || detail_lower.contains("subagent")
        || detail_lower.contains("worker")
        || detail.contains("워커")
    {
        CodexActivityKind::Worker
    } else if detail_lower.contains("file change")
        || detail_lower.contains("apply patch")
        || detail.contains("파일 변경")
    {
        CodexActivityKind::FileChange
    } else if detail_lower.contains("search")
        || detail_lower.contains("browse")
        || detail.contains("검색")
    {
        CodexActivityKind::WebSearch
    } else {
        CodexActivityKind::ToolCall
    };
    let title = match kind {
        CodexActivityKind::Terminal => "명령 실행",
        CodexActivityKind::FileChange => "파일 변경",
        CodexActivityKind::ToolCall => "도구 요청",
        CodexActivityKind::WebSearch => "웹 검색",
        CodexActivityKind::Worker => "워커 작업",
    }
    .to_owned();
    let stable_detail = detail.trim_matches('"').trim().to_owned();
    Some(CodexActivity {
        item_id: format!("web:{request_id}:{}:{stable_detail}", kind.label()),
        kind,
        phase,
        title,
        detail: stable_detail,
        worker_thread_id: None,
        worker_status: None,
    })
}

fn native_worker_status(activity: &CodexActivity, current: Option<SessionStatus>) -> SessionStatus {
    if activity.phase == CodexActivityPhase::Failed {
        return SessionStatus::Failed;
    }
    if let Some(status) = activity.worker_status.as_deref() {
        return match status.to_ascii_lowercase().as_str() {
            "pendinginit" | "pending_init" | "running" => SessionStatus::Running,
            "completed" | "shutdown" => SessionStatus::Completed,
            "errored" | "error" | "notfound" | "not_found" => SessionStatus::Failed,
            "interrupted" => SessionStatus::NeedsInput,
            _ => current.unwrap_or(SessionStatus::Running),
        };
    }
    match activity.title.as_str() {
        "워커 종료" if activity.phase == CodexActivityPhase::Completed => {
            SessionStatus::Completed
        }
        _ => current.unwrap_or(SessionStatus::Running),
    }
}

fn session_id_is_live(session_id: Option<&str>, sessions: &[AgentSession]) -> bool {
    session_id.is_some_and(|id| sessions.iter().any(|session| session.id == id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ime_preedit_and_commit_keep_submit_blocked_for_the_commit_frame() {
        let mut composing = false;
        assert!(!apply_ime_event(
            &mut composing,
            &egui::ImeEvent::Preedit {
                text: "한".to_owned(),
                active_range_chars: Some(0..1),
            }
        ));
        assert!(composing);
        assert!(apply_ime_event(
            &mut composing,
            &egui::ImeEvent::Commit("한".to_owned())
        ));
        assert!(!composing);
        assert!(!apply_ime_event(
            &mut composing,
            &egui::ImeEvent::Preedit {
                text: String::new(),
                active_range_chars: None,
            }
        ));
        assert!(!composing);
    }

    #[test]
    fn native_web_correlation_guards_unknown_and_stale_transcript_events() {
        let request_id = "web-chat-reused".to_owned();
        let session_id = "session-a".to_owned();
        let correlation =
            WebGptTurnRequest::native_chat(session_id.clone(), request_id.clone()).lease(0, 7);
        let mut sessions = HashMap::from([(request_id.clone(), session_id.clone())]);
        let mut correlations = HashMap::from([(request_id.clone(), correlation.clone())]);
        let mut messages = HashMap::from([(
            session_id.clone(),
            vec![ChatMessage {
                role: ChatRole::Assistant,
                model: ChatModel::WebGpt56Sol,
                text: "selected transcript".to_owned(),
                turn_id: Some(request_id.clone()),
                streaming: true,
                image: None,
                activity: None,
            }],
        )]);

        let mut wrong_session = correlation.clone();
        wrong_session.session_id = "session-other".to_owned();
        let mut wrong_account = correlation.clone();
        wrong_account.account_id = "account-other".to_owned();
        let mut wrong_generation = correlation.clone();
        wrong_generation.lease.generation += 1;
        for wrong in [wrong_session, wrong_account] {
            assert!(native_web_session_for_correlation(&sessions, &wrong).is_none());
            assert!(!native_web_correlation_matches(&correlations, &wrong));
            assert_eq!(messages[&session_id][0].text, "selected transcript");
        }
        assert!(native_web_session_for_correlation(&sessions, &wrong_generation).is_some());
        assert!(!native_web_correlation_matches(
            &correlations,
            &wrong_generation
        ));
        assert_eq!(messages[&session_id][0].text, "selected transcript");

        assert!(!apply_web_answer(
            &mut HashMap::new(),
            &mut messages,
            &request_id,
            "late answer".to_owned(),
        ));
        assert_eq!(messages[&session_id][0].text, "selected transcript");

        assert!(finish_web_local_session(
            &mut sessions,
            &mut messages,
            &request_id,
        ));
        correlations.remove(&request_id);
        assert!(!finish_web_local_session(
            &mut sessions,
            &mut messages,
            &request_id,
        ));
        assert!(!native_web_correlation_matches(&correlations, &correlation));
        assert!(!messages[&session_id][0].streaming);
    }

    #[test]
    fn native_web_terminal_before_submitted_latches_and_finishes_once() {
        let request_id = "web-chat-failed-before-submitted".to_owned();
        let session_id = "session-terminal".to_owned();
        let correlation =
            WebGptTurnRequest::native_chat(session_id.clone(), request_id.clone()).lease(0, 12);
        let mut sessions = HashMap::from([(request_id.clone(), session_id.clone())]);
        let mut correlations = HashMap::new();
        let mut messages = HashMap::from([(
            session_id.clone(),
            vec![ChatMessage {
                role: ChatRole::Assistant,
                model: ChatModel::WebGpt56Sol,
                text: String::new(),
                turn_id: Some(request_id.clone()),
                streaming: true,
                image: None,
                activity: None,
            }],
        )]);

        assert_eq!(
            latch_native_web_correlation(&sessions, &mut correlations, &correlation),
            Some(session_id.clone())
        );
        assert_eq!(correlations.get(&request_id), Some(&correlation));
        assert!(finish_web_local_session(
            &mut sessions,
            &mut messages,
            &request_id,
        ));
        correlations.remove(&request_id);
        assert!(!finish_web_local_session(
            &mut sessions,
            &mut messages,
            &request_id,
        ));
        assert!(!native_web_correlation_matches(&correlations, &correlation));
        assert!(!messages[&session_id][0].streaming);
    }

    #[test]
    fn deleted_session_purges_native_web_turn_ownership() {
        let request_id = "web-chat-deleted".to_owned();
        let session_id = "session-deleted".to_owned();
        let correlation =
            WebGptTurnRequest::native_chat(session_id.clone(), request_id.clone()).lease(0, 4);
        let mut sessions = HashMap::from([(request_id.clone(), session_id.clone())]);
        let mut correlations = HashMap::from([(request_id.clone(), correlation.clone())]);

        purge_web_local_session_ownership(
            std::slice::from_ref(&session_id),
            &mut sessions,
            &mut correlations,
        );

        assert!(sessions.is_empty());
        assert!(correlations.is_empty());
        assert!(latch_native_web_correlation(&sessions, &mut correlations, &correlation).is_none());
    }

    #[test]
    fn web_activity_noise_is_removed_from_assistant_text() {
        let raw = "inProgress: \"pwsh.exe -Command git status\"\ncompleted: \"pwsh.exe -Command git status\"\nCodex: ERROR failed websocket\n최종 답변";
        assert_eq!(sanitize_web_assistant_text(raw), "최종 답변");
    }

    #[test]
    fn web_terminal_status_becomes_structured_activity() {
        let activity = web_activity_from_visible_text(
            "request-1",
            "completed: \"C:\\Program Files\\PowerShell\\pwsh.exe\" -Command \"git status\"",
        )
        .expect("terminal activity");
        assert_eq!(activity.kind, CodexActivityKind::Terminal);
        assert_eq!(activity.phase, CodexActivityPhase::Completed);
        assert!(activity.item_id.contains("터미널 작업"));
    }

    #[test]
    fn session_snapshot_preserves_a_live_non_main_selection() {
        let mut graph = crate::sessions::SessionGraph::new();
        let main = graph.create_root("project", SessionRuntime::Unified, "Main");
        let second = graph.create_root("project", SessionRuntime::Unified, "Second");
        let sessions = graph.list_project("project");

        assert!(session_id_is_live(Some(&second.id), &sessions));
        assert!(session_id_is_live(Some(&main.id), &sessions));
        assert!(!session_id_is_live(Some("missing"), &sessions));
    }

    #[test]
    fn orchestration_cards_follow_the_selected_sessions_root() {
        let mut graph = crate::sessions::SessionGraph::new();
        let first = graph.create_root("project", SessionRuntime::Unified, "First");
        let worker = graph
            .spawn_worker(&first.id, SessionRuntime::Codex, "Implementation")
            .unwrap();
        let unrelated = graph.create_root("project", SessionRuntime::Unified, "Other");
        let sessions = graph.list_project("project");

        let view = orchestration_view(&sessions, Some(&worker.id)).expect("worker graph");
        assert_eq!(view.root.id, first.id);
        assert_eq!(view.workers.len(), 1);
        assert_eq!(view.workers[0].id, worker.id);
        assert!(orchestration_view(&sessions, Some(&unrelated.id)).is_none());
    }

    #[test]
    fn graph_worker_limit_keeps_the_selected_session_visible() {
        let mut graph = crate::sessions::SessionGraph::new();
        let root = graph.create_root("project", SessionRuntime::Unified, "First");
        let mut workers = Vec::new();
        for index in 0..4 {
            workers.push(
                graph
                    .spawn_worker(&root.id, SessionRuntime::Codex, format!("Worker {index}"))
                    .unwrap(),
            );
        }
        let sessions = graph.list_project("project");
        let view = orchestration_view(&sessions, Some(&workers[3].id)).expect("worker graph");
        let visible = visible_graph_workers(&view, Some(&workers[3].id));

        assert_eq!(visible.len(), ORCHESTRATION_GRAPH_MAX_WORKERS);
        assert!(visible.iter().any(|session| session.id == workers[3].id));
    }

    #[test]
    fn activity_group_stays_running_until_every_entry_finishes() {
        let mut group = ChatActivityGroup {
            id: "group-1".into(),
            kind: CodexActivityKind::Terminal,
            entries: vec![
                ChatActivityEntry {
                    item_id: "a".into(),
                    title: "명령 실행".into(),
                    detail: "git status".into(),
                    phase: CodexActivityPhase::Completed,
                },
                ChatActivityEntry {
                    item_id: "b".into(),
                    title: "명령 실행".into(),
                    detail: "cargo check".into(),
                    phase: CodexActivityPhase::Running,
                },
            ],
        };
        assert_eq!(group.phase(), CodexActivityPhase::Running);
        assert_eq!(group.status_label(), "터미널 작업 중…");

        group.entries[1].phase = CodexActivityPhase::Completed;
        assert_eq!(group.phase(), CodexActivityPhase::Completed);
        assert_eq!(group.status_label(), "터미널 작업 완료");
    }

    #[test]
    fn chat_surface_always_reserves_bottom_margin() {
        assert_eq!(chat_content_height(500.0), 500.0 - CHAT_BOTTOM_MARGIN);
        assert_eq!(chat_content_height(CHAT_BOTTOM_MARGIN), 0.0);
        assert_eq!(chat_content_height(0.0), 0.0);
    }

    #[test]
    fn reauth_control_only_appears_for_supported_accounts_that_need_it() {
        let oauth = ProviderAccount {
            id: "oauth-a".into(),
            kind: "oauth".into(),
            needs_reauth: true,
            ..Default::default()
        };
        assert!(account_reauth_eligible("kiro", &oauth));

        let healthy_oauth = ProviderAccount {
            needs_reauth: false,
            ..oauth.clone()
        };
        assert!(!account_reauth_eligible("kiro", &healthy_oauth));

        let key = ProviderAccount {
            kind: "key".into(),
            ..oauth.clone()
        };
        assert!(!account_reauth_eligible("opencode-go", &key));

        let openai_main = ProviderAccount {
            id: "__main__".into(),
            kind: "codex".into(),
            is_main: true,
            ..oauth.clone()
        };
        assert!(!account_reauth_eligible("openai", &openai_main));

        let openai_pool = ProviderAccount {
            id: "pool-b".into(),
            kind: "codex".into(),
            is_main: false,
            ..oauth
        };
        assert!(account_reauth_eligible("openai", &openai_pool));
    }
}
