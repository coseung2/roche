use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, Instant},
};

use eframe::egui::{self, RichText, TextEdit};
use serde::{Deserialize, Serialize};

use crate::{
    codex::{CodexCatalogModel, CodexConnection, CodexEvent, CodexRuntimeController},
    ocx::{
        OcxEvent, OcxQuotaController, ProcessMemory, ProviderAccount, ProviderConfig, ProviderPool,
        QuotaBar, QuotaReport, commit_headroom, consume_reset_credit, fetch_codex_active_state,
        fetch_provider_configs, fetch_provider_pool, format_bytes, ocx_health_pid, quota_bars,
        request_account_reauth, run_ocx, sample_current_process, sample_process,
        set_account_paused, set_active_account, set_auto_switch_threshold,
    },
    perf::{SyntheticActor, SyntheticToolKind, SyntheticWorkload},
    sessions::{AgentSession, SessionRuntime},
    web_browser::{WebGptBrowserController, WebGptBrowserEvent, WebGptBrowserState},
    webgpt::{WebGptRuntimeController, WebGptRuntimeEvent},
};

const LUCIDE_CHEVRON_DOWN: char = '\u{e06d}';
const LUCIDE_CHEVRON_LEFT: char = '\u{e06e}';
const LUCIDE_CHEVRON_RIGHT: char = '\u{e06f}';
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceTab {
    Chat,
    Performance,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReasoningLevel {
    Fast,
    High,
    VeryHigh,
}

impl ReasoningLevel {
    fn label(self) -> &'static str {
        match self {
            Self::Fast => "빠름",
            Self::High => "높음",
            Self::VeryHigh => "매우 높음",
        }
    }

    fn codex_effort(self) -> &'static str {
        match self {
            Self::Fast => "low",
            Self::High => "high",
            Self::VeryHigh => "xhigh",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatRole {
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatPopoverPage {
    Root,
    Model,
    Reasoning,
}

#[derive(Clone)]
struct ChatMessage {
    role: ChatRole,
    model: ChatModel,
    text: String,
    turn_id: Option<String>,
    streaming: bool,
    image: Option<egui::TextureHandle>,
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

pub struct DesktopApp {
    workspaces: Vec<WorkspaceEntry>,
    selected_workspace: Option<PathBuf>,
    workspace_picker: Option<std::thread::JoinHandle<Option<PathBuf>>>,
    workspaces_store: PathBuf,
    ocx: OcxQuotaController,
    ocx_reports: Vec<QuotaReport>,
    ocx_online: bool,
    ocx_status: Option<String>,
    ocx_memory: ProcessMemory,
    roche_memory: ProcessMemory,
    ocx_pid: Option<u32>,
    mem_headroom: u64,
    last_mem_sample: Instant,
    power_pending: bool,
    ocx_pools: Vec<ProviderPool>,
    account_busy: Option<String>,
    auto_switch_threshold: u32,
    auto_switch_busy: bool,
    last_account_poll: Instant,
    telemetry_tx: Sender<DesktopTelemetryEvent>,
    telemetry_rx: Receiver<DesktopTelemetryEvent>,
    memory_sample_pending: bool,
    account_poll_pending: bool,
    expanded_providers: HashSet<String>,
    provider_order: Vec<String>,
    codex: CodexRuntimeController,
    webgpt: WebGptRuntimeController,
    web_browser: WebGptBrowserController,
    web_browser_state: WebGptBrowserState,
    codex_connection: CodexConnection,
    codex_thread_id: Option<String>,
    codex_turn_id: Option<String>,
    codex_model: Option<String>,
    codex_catalog_source: Option<String>,
    codex_catalog: Vec<CodexCatalogModel>,
    selected_codex_slug: Option<String>,
    session_tabs: Vec<AgentSession>,
    selected_session_id: Option<String>,
    chat_messages: HashMap<String, Vec<ChatMessage>>,
    web_local_sessions: HashMap<String, String>,
    pending_codex_sessions: VecDeque<String>,
    codex_turn_sessions: HashMap<String, String>,
    agent_prompt: String,
    draft_attachments: Vec<DraftAttachment>,
    selected_model: ChatModel,
    reasoning_level: ReasoningLevel,
    chat_popover_open: bool,
    chat_popover_page: ChatPopoverPage,
    ime_composing: bool,
    focus_composer_on_start: bool,
    runtime_message: Option<String>,
    selected_tab: WorkspaceTab,
    workload: SyntheticWorkload,
}

impl DesktopApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_runtime_fonts(&cc.egui_ctx);
        apply_notch_theme(&cc.egui_ctx);
        let default_root = std::env::var_os("ROCHE_PROJECT_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        let workspaces_store = workspaces_store_path();
        let mut workspaces = load_workspaces(&workspaces_store);
        if workspaces.is_empty() {
            let default = WorkspaceEntry::from_path(default_root.clone());
            workspaces.push(default);
            save_workspaces(&workspaces_store, &workspaces);
        }
        let selected_workspace = workspaces.first().map(|entry| entry.path.clone());
        let codex_root = selected_workspace
            .clone()
            .unwrap_or_else(|| default_root.clone());
        let codex = CodexRuntimeController::spawn(codex_root);
        let webgpt = WebGptRuntimeController::spawn();
        let web_browser = if std::env::var_os("ROCHE_DISABLE_WEBGPT_BROWSER").is_some() {
            WebGptBrowserController::disabled("Web GPT browser disabled for diagnostics")
        } else {
            WebGptBrowserController::spawn()
        };
        let ocx = OcxQuotaController::spawn();
        let (telemetry_tx, telemetry_rx) = mpsc::channel();

        Self {
            workspaces,
            selected_workspace,
            workspace_picker: None,
            workspaces_store,
            ocx,
            ocx_reports: Vec::new(),
            ocx_online: false,
            ocx_status: None,
            ocx_memory: Default::default(),
            roche_memory: Default::default(),
            ocx_pid: None,
            mem_headroom: 0,
            last_mem_sample: Instant::now(),
            power_pending: false,
            ocx_pools: Vec::new(),
            account_busy: None,
            auto_switch_threshold: 80,
            auto_switch_busy: false,
            last_account_poll: Instant::now(),
            telemetry_tx,
            telemetry_rx,
            memory_sample_pending: false,
            account_poll_pending: false,
            expanded_providers: HashSet::new(),
            provider_order: Vec::new(),
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
            session_tabs: Vec::new(),
            selected_session_id: None,
            chat_messages: HashMap::new(),
            web_local_sessions: HashMap::new(),
            pending_codex_sessions: VecDeque::new(),
            codex_turn_sessions: HashMap::new(),
            agent_prompt: String::new(),
            draft_attachments: Vec::new(),
            selected_model: ChatModel::WebGpt56Sol,
            reasoning_level: ReasoningLevel::VeryHigh,
            chat_popover_open: false,
            chat_popover_page: ChatPopoverPage::Root,
            ime_composing: false,
            focus_composer_on_start: true,
            runtime_message: None,
            selected_tab: WorkspaceTab::Chat,
            workload: SyntheticWorkload::standard(),
        }
    }

    fn selected_session_key(&self) -> String {
        self.selected_session_id
            .clone()
            .unwrap_or_else(|| LOCAL_MAIN_SESSION_KEY.to_owned())
    }

    fn push_chat_message(&mut self, session_id: &str, message: ChatMessage) {
        self.chat_messages
            .entry(session_id.to_owned())
            .or_default()
            .push(message);
    }

    fn add_draft_attachment(&mut self, path: PathBuf, preview: Option<egui::TextureHandle>) {
        if !path.is_file() {
            self.runtime_message = Some(format!("첨부할 수 없는 경로입니다: {}", path.display()));
            return;
        }
        if self
            .draft_attachments
            .iter()
            .any(|attachment| attachment.path == path)
        {
            return;
        }
        self.draft_attachments
            .push(DraftAttachment { path, preview });
    }

    fn apply_session_snapshot(&mut self, sessions: Vec<AgentSession>) {
        let previous = self.selected_session_id.clone();
        self.session_tabs = sessions;
        if previous
            .as_deref()
            .is_some_and(|id| self.session_tabs.iter().any(|session| session.id == id))
        {
            return;
        }
        let next = self
            .session_tabs
            .iter()
            .find(|session| session.parent_session_id.is_none())
            .or_else(|| self.session_tabs.first())
            .map(|session| session.id.clone());
        if let Some(next) = next {
            if let Some(local_messages) = self.chat_messages.remove(LOCAL_MAIN_SESSION_KEY) {
                self.chat_messages
                    .entry(next.clone())
                    .or_default()
                    .extend(local_messages);
            }
            self.selected_session_id = Some(next);
        }
    }

    fn send_chat_message(&mut self) {
        let prompt = self.agent_prompt.trim().to_owned();
        if prompt.is_empty() && self.draft_attachments.is_empty() {
            return;
        }
        let model = self.selected_model;
        if matches!(self.codex_connection, CodexConnection::Offline { .. })
            && model == ChatModel::Codex
        {
            self.runtime_message = Some(
                "Codex CLI가 연결되지 않아 전송하지 않았습니다 (LOCAL CODEX OFFLINE)".to_owned(),
            );
            return;
        }
        if model == ChatModel::WebGpt56Sol
            && !matches!(self.web_browser_state, WebGptBrowserState::LoggedIn)
        {
            self.runtime_message = Some("[WEB] GPT 로그인이 필요합니다".to_owned());
            self.web_browser.show_login();
            return;
        }
        let session_id = self.selected_session_key();
        let attachments = std::mem::take(&mut self.draft_attachments);
        let attachment_paths = attachments
            .iter()
            .map(|attachment| attachment.path.clone())
            .collect::<Vec<_>>();
        let attachment_summary = if attachments.is_empty() {
            String::new()
        } else {
            attachments
                .iter()
                .map(DraftAttachment::label)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let message_text = match (prompt.is_empty(), attachment_summary.is_empty()) {
            (false, false) => format!("{prompt}\n첨부: {attachment_summary}"),
            (true, false) => format!("첨부: {attachment_summary}"),
            _ => prompt.clone(),
        };
        let preview = attachments
            .iter()
            .find_map(|attachment| attachment.preview.clone());
        self.push_chat_message(
            &session_id,
            ChatMessage {
                role: ChatRole::User,
                model,
                text: message_text,
                turn_id: None,
                streaming: false,
                image: preview,
            },
        );
        self.agent_prompt.clear();

        match model {
            ChatModel::Codex => {
                self.pending_codex_sessions.push_back(session_id);
                self.codex.send_with_attachments(
                    prompt,
                    attachment_paths,
                    self.reasoning_level.codex_effort().to_owned(),
                    self.selected_codex_slug.clone(),
                );
            }
            ChatModel::WebGpt56Sol => {
                let request_id = format!(
                    "web-chat-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                );
                self.web_local_sessions
                    .insert(request_id.clone(), session_id);
                self.web_browser
                    .submit_chat_with_attachments(request_id, prompt, attachment_paths);
                self.runtime_message = Some("[WEB] GPT 요청 전송 중…".to_owned());
            }
        }
    }

    fn drain_webgpt(&mut self) {
        for event in self.webgpt.drain() {
            match event {
                WebGptRuntimeEvent::SessionsUpdated { sessions } => {
                    self.apply_session_snapshot(sessions);
                }
                WebGptRuntimeEvent::Submitted { .. }
                | WebGptRuntimeEvent::Answered { .. }
                | WebGptRuntimeEvent::Cancelled { .. }
                | WebGptRuntimeEvent::Error { .. } => {}
            }
        }
    }

    fn drain_web_browser(&mut self) {
        for event in self.web_browser.drain() {
            match event {
                WebGptBrowserEvent::State(state) => {
                    self.web_browser_state = state;
                }
                WebGptBrowserEvent::WakeSubmitted { request_id } => {
                    self.runtime_message =
                        Some(format!("[WEB] GPT 오케스트레이션 실행 중 · {request_id}"));
                }
                WebGptBrowserEvent::ChatSubmitted { request_id } => {
                    self.runtime_message = Some(format!("[WEB] GPT 응답 생성 중 · {request_id}"));
                }
                WebGptBrowserEvent::ChatAnswered { request_id, text } => {
                    let session_id = self
                        .web_local_sessions
                        .remove(&request_id)
                        .unwrap_or_else(|| self.selected_session_key());
                    self.push_chat_message(
                        &session_id,
                        ChatMessage {
                            role: ChatRole::Assistant,
                            model: ChatModel::WebGpt56Sol,
                            text,
                            turn_id: Some(request_id),
                            streaming: false,
                            image: None,
                        },
                    );
                    self.runtime_message = None;
                }
                WebGptBrowserEvent::Error(message) => {
                    self.runtime_message = Some(format!("[WEB] 브라우저 오류 · {message}"));
                }
            }
        }
    }

    fn drain_codex(&mut self) {
        for event in self.codex.drain() {
            match event {
                CodexEvent::Connection(connection) => self.codex_connection = connection,
                CodexEvent::ThreadStarted { thread_id, model } => {
                    self.codex_thread_id = Some(thread_id);
                    self.codex_model = model;
                }
                CodexEvent::TurnStarted { turn_id, .. } => {
                    let session_id = self
                        .pending_codex_sessions
                        .pop_front()
                        .unwrap_or_else(|| self.selected_session_key());
                    self.codex_turn_sessions.insert(turn_id.clone(), session_id);
                    self.codex_turn_id = Some(turn_id);
                }
                CodexEvent::AssistantDelta { turn_id, delta, .. } => {
                    let session_id = self
                        .codex_turn_sessions
                        .get(&turn_id)
                        .cloned()
                        .unwrap_or_else(|| self.selected_session_key());
                    let messages = self.chat_messages.entry(session_id).or_default();
                    if let Some(message) = messages.iter_mut().rev().find(|message| {
                        message.role == ChatRole::Assistant
                            && message.model == ChatModel::Codex
                            && message.turn_id.as_deref() == Some(turn_id.as_str())
                    }) {
                        message.text.push_str(&delta);
                        message.streaming = true;
                    } else {
                        messages.push(ChatMessage {
                            role: ChatRole::Assistant,
                            model: ChatModel::Codex,
                            text: delta,
                            turn_id: Some(turn_id),
                            streaming: true,
                            image: None,
                        });
                    }
                }
                CodexEvent::AssistantCompleted { turn_id, text, .. } => {
                    let session_id = self
                        .codex_turn_sessions
                        .get(&turn_id)
                        .cloned()
                        .unwrap_or_else(|| self.selected_session_key());
                    let messages = self.chat_messages.entry(session_id).or_default();
                    if let Some(message) = messages.iter_mut().rev().find(|message| {
                        message.role == ChatRole::Assistant
                            && message.model == ChatModel::Codex
                            && message.turn_id.as_deref() == Some(turn_id.as_str())
                    }) {
                        message.text = text;
                        message.streaming = false;
                    } else {
                        messages.push(ChatMessage {
                            role: ChatRole::Assistant,
                            model: ChatModel::Codex,
                            text,
                            turn_id: Some(turn_id),
                            streaming: false,
                            image: None,
                        });
                    }
                }
                CodexEvent::ToolActivity {
                    turn_id, summary, ..
                } => {
                    let session_id = self
                        .codex_turn_sessions
                        .get(&turn_id)
                        .cloned()
                        .unwrap_or_else(|| self.selected_session_key());
                    self.push_chat_message(
                        &session_id,
                        ChatMessage {
                            role: ChatRole::Tool,
                            model: ChatModel::Codex,
                            text: summary,
                            turn_id: Some(turn_id),
                            streaming: false,
                            image: None,
                        },
                    );
                }
                CodexEvent::TurnCompleted {
                    turn_id, status, ..
                } => {
                    if self.codex_turn_id.as_deref() == Some(turn_id.as_str()) {
                        self.codex_turn_id = None;
                    }
                    if let Some(session_id) = self.codex_turn_sessions.remove(&turn_id)
                        && let Some(messages) = self.chat_messages.get_mut(&session_id)
                    {
                        for message in messages.iter_mut().rev() {
                            if message.turn_id.as_deref() == Some(turn_id.as_str()) {
                                message.streaming = false;
                            }
                        }
                    }
                    self.runtime_message = Some(format!("Codex turn {status}"));
                }
                CodexEvent::CatalogLoaded { source, models } => {
                    self.codex_catalog_source = Some(source);
                    self.codex_catalog = models;
                }
                CodexEvent::Notice(message) => self.runtime_message = Some(message),
                CodexEvent::Error(message) => self.runtime_message = Some(message),
            }
        }
    }

    fn selected_model_label(&self) -> String {
        if let Some(slug) = self.selected_codex_slug.as_deref() {
            return self
                .codex_catalog
                .iter()
                .find(|model| model.slug == slug)
                .map(|model| model.display_name.clone())
                .unwrap_or_else(|| slug.to_owned());
        }
        self.selected_model.label().to_owned()
    }

    fn model_popover_height(&self) -> f32 {
        if self.codex_catalog.is_empty() {
            340.0
        } else {
            (152.0 + (self.codex_catalog.len() as f32 * 48.0).min(448.0)).min(624.0)
        }
    }

    fn select_chat_model(&mut self, model: ChatModel) {
        self.selected_model = model;
        self.selected_codex_slug = None;
        self.chat_popover_open = false;
        self.chat_popover_page = ChatPopoverPage::Root;
    }

    fn model_row(&mut self, ui: &mut egui::Ui, model: ChatModel) {
        let selected = self.selected_model == model && self.selected_codex_slug.is_none();
        if ui.selectable_label(selected, model.label()).clicked() {
            self.select_chat_model(model);
        }
    }

    fn drain_ocx(&mut self) {
        for event in self.ocx.drain() {
            match event {
                OcxEvent::Updated { reports } => {
                    self.ocx_reports = reports;
                    self.ocx_online = true;
                    self.ocx_status = None;
                    self.power_pending = false;
                }
                OcxEvent::Error(message) => {
                    self.ocx_online = false;
                    self.ocx_status = Some(message);
                    self.power_pending = false;
                }
            }
        }
    }

    fn sample_memory(&mut self) {
        let now = Instant::now();
        if self.memory_sample_pending
            || now.duration_since(self.last_mem_sample) < Duration::from_secs(2)
        {
            return;
        }
        self.last_mem_sample = now;
        self.memory_sample_pending = true;
        let ocx_online = self.ocx_online;
        let events = self.telemetry_tx.clone();
        std::thread::spawn(move || {
            let roche_memory = sample_current_process().unwrap_or_default();
            let mem_headroom = commit_headroom().unwrap_or(0);
            let (ocx_pid, ocx_memory) = if ocx_online {
                match ocx_health_pid() {
                    Some(pid) => (Some(pid), sample_process(pid).unwrap_or_default()),
                    None => (None, ProcessMemory::default()),
                }
            } else {
                (None, ProcessMemory::default())
            };
            let _ = events.send(DesktopTelemetryEvent::Memory {
                roche_memory,
                mem_headroom,
                ocx_pid,
                ocx_memory,
            });
        });
    }

    fn drain_telemetry(&mut self) {
        while let Ok(event) = self.telemetry_rx.try_recv() {
            match event {
                DesktopTelemetryEvent::Memory {
                    roche_memory,
                    mem_headroom,
                    ocx_pid,
                    ocx_memory,
                } => {
                    self.roche_memory = roche_memory;
                    self.mem_headroom = mem_headroom;
                    self.ocx_pid = ocx_pid;
                    self.ocx_memory = ocx_memory;
                    self.memory_sample_pending = false;
                }
                DesktopTelemetryEvent::ProviderPools {
                    pools,
                    auto_switch_threshold,
                } => {
                    self.ocx_pools = pools;
                    self.auto_switch_threshold = auto_switch_threshold.min(100);
                    for pool in &self.ocx_pools {
                        if !self.provider_order.contains(&pool.provider) {
                            self.provider_order.push(pool.provider.clone());
                        }
                    }
                    self.provider_order
                        .retain(|name| self.ocx_pools.iter().any(|pool| pool.provider == *name));
                    self.account_poll_pending = false;
                }
                DesktopTelemetryEvent::AutoSwitchUpdated(result) => {
                    self.auto_switch_busy = false;
                    match result {
                        Ok(threshold) => {
                            self.auto_switch_threshold = threshold.min(100);
                            self.last_account_poll = Instant::now() - Duration::from_secs(5);
                        }
                        Err(error) => self.runtime_message = Some(error),
                    }
                }
                DesktopTelemetryEvent::AccountActionFinished { busy_key, result } => {
                    if self.account_busy.as_deref() == Some(busy_key.as_str()) {
                        self.account_busy = None;
                    }
                    if let Err(error) = result {
                        self.runtime_message = Some(error);
                    }
                    self.last_account_poll = Instant::now() - Duration::from_secs(5);
                    self.ocx.refresh();
                }
            }
        }
    }

    fn toggle_power(&mut self) {
        if self.power_pending {
            return;
        }
        self.power_pending = true;
        let action = if self.ocx_online { "stop" } else { "start" }.to_owned();
        std::thread::spawn(move || {
            let _ = run_ocx(&action);
        });
        self.ocx.refresh();
    }

    fn poll_accounts(&mut self) {
        let now = Instant::now();
        if self.account_poll_pending
            || now.duration_since(self.last_account_poll) < Duration::from_secs(5)
        {
            return;
        }
        self.last_account_poll = now;
        if !self.ocx_online {
            self.ocx_pools.clear();
            return;
        }

        // Network-backed account discovery must never run on the egui UI thread.
        let report_providers: Vec<String> = self
            .ocx_reports
            .iter()
            .map(|report| report.provider.clone())
            .collect();
        self.account_poll_pending = true;
        let events = self.telemetry_tx.clone();
        std::thread::spawn(move || {
            let mut configs = fetch_provider_configs().unwrap_or_default();
            for provider in report_providers {
                if !configs.iter().any(|config| config.name == provider) {
                    configs.push(ProviderConfig {
                        name: provider,
                        ..Default::default()
                    });
                }
            }
            if !configs.iter().any(|config| config.name == "openai") {
                configs.push(ProviderConfig {
                    name: "openai".into(),
                    ..Default::default()
                });
            }
            configs.retain(|config| !config.disabled);
            configs.sort_by(|left, right| left.name.cmp(&right.name));
            configs.dedup_by(|left, right| left.name == right.name);

            let active_state = fetch_codex_active_state().unwrap_or_default();
            let pools = configs
                .iter()
                .map(|config| {
                    fetch_provider_pool(config, active_state.active_codex_account_id.as_deref())
                })
                .collect();
            let _ = events.send(DesktopTelemetryEvent::ProviderPools {
                pools,
                auto_switch_threshold: active_state.auto_switch_threshold,
            });
        });
    }

    fn run_account_action(
        &mut self,
        provider: &str,
        account: &ProviderAccount,
        action: AccountAction,
    ) {
        if self.account_busy.is_some() {
            return;
        }
        let provider = provider.to_owned();
        let kind = account.kind.clone();
        let id = account.id.clone();
        let was_paused = account.paused;
        let busy_key = format!("{provider}:{id}");
        self.account_busy = Some(busy_key.clone());
        let events = self.telemetry_tx.clone();
        std::thread::spawn(move || {
            let result = match action {
                AccountAction::Activate => {
                    let resume = if was_paused && matches!(kind.as_str(), "codex" | "oauth") {
                        set_account_paused(&provider, &kind, &id, false)
                    } else {
                        Ok(())
                    };
                    resume.and_then(|_| set_active_account(&provider, &kind, &id))
                }
                AccountAction::Pause => set_account_paused(&provider, &kind, &id, true),
                AccountAction::Reset => consume_reset_credit(&id),
                AccountAction::Reauth => request_account_reauth(&provider, &kind, &id),
            };
            let _ = events.send(DesktopTelemetryEvent::AccountActionFinished { busy_key, result });
        });
    }

    fn update_auto_switch_threshold(&mut self, threshold: u32) {
        if self.auto_switch_busy {
            return;
        }
        let threshold = threshold.min(100);
        self.auto_switch_threshold = threshold;
        self.auto_switch_busy = true;
        let events = self.telemetry_tx.clone();
        std::thread::spawn(move || {
            let result = set_auto_switch_threshold(threshold).map(|_| threshold);
            let _ = events.send(DesktopTelemetryEvent::AutoSwitchUpdated(result));
        });
    }

    fn render_quota_panel(&mut self, ui: &mut egui::Ui) {
        // Notch-style header: power (ocx start/stop) + per-process private commit.
        let mut power_clicked = false;
        ui.horizontal(|ui| {
            let power_color = if self.ocx_online {
                NOTCH_ACCENT
            } else {
                NOTCH_TEXT_SUB
            };
            let size = egui::vec2(UI_LINE_HEIGHT, UI_LINE_HEIGHT);
            let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
            power_clicked = response.clicked();
            draw_power_icon(ui.painter(), rect, power_color);
            ui.small(
                RichText::new(if self.ocx_online {
                    "OCX online"
                } else {
                    "OCX offline"
                })
                .color(power_color),
            );
            if ui
                .add(egui::Button::new(icon_rich_text(LUCIDE_REFRESH, NOTCH_TEXT_SUB)).frame(false))
                .on_hover_text("새로고침")
                .clicked()
            {
                self.ocx.refresh();
            }
        });
        if power_clicked {
            self.toggle_power();
        }
        self.render_process_memory(ui, "OCX", self.ocx_memory);
        self.render_process_memory(ui, "Roche", self.roche_memory);
        ui.separator();
        self.render_account_pool(ui);
    }

    fn render_process_memory(&self, ui: &mut egui::Ui, name: &str, memory: ProcessMemory) {
        let headroom = self.mem_headroom;
        let max = memory.private_commit.saturating_add(headroom).max(1);
        let percent = ((memory.private_commit as f64 / max as f64) * 100.0).clamp(0.0, 100.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(name).strong().color(NOTCH_TEXT).small());
            ui.label(
                RichText::new(format!("Private {}", format_bytes(memory.private_commit)))
                    .color(NOTCH_ACCENT)
                    .small(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("Max {}", format_bytes(max)))
                        .color(NOTCH_TEXT_MUTED)
                        .small(),
                );
            });
        });
        let desired = egui::vec2(ui.available_width(), 4.0);
        let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 2.0, NOTCH_BAR_BG);
        let filled = (rect.width() * percent as f32 / 100.0).round();
        if filled > 0.0 {
            let fill = filled.min(rect.width());
            painter.rect_filled(
                egui::Rect::from_min_size(rect.min, egui::vec2(fill, rect.height())),
                2.0,
                NOTCH_GREEN,
            );
        }
        ui.add_space(6.0);
    }

    fn render_provider_pool(&mut self, ui: &mut egui::Ui, pool: &ProviderPool) -> egui::Response {
        let expanded = self.expanded_providers.contains(&pool.provider);
        let account_count = pool.accounts.len();
        let provider_label = self
            .ocx_reports
            .iter()
            .find(|report| report.provider == pool.provider)
            .and_then(|report| report.label.clone())
            .unwrap_or_else(|| pool.provider.clone());
        let provider_label = [" (Codex login)", " (AWS CodeWhisperer)"]
            .iter()
            .find_map(|suffix| provider_label.strip_suffix(suffix))
            .unwrap_or(&provider_label)
            .to_owned();
        let active_identity = pool
            .accounts
            .iter()
            .find(|account| account.active)
            .map(|account| account.identity.clone());
        let chevron = if expanded {
            LUCIDE_CHEVRON_DOWN
        } else {
            LUCIDE_CHEVRON_RIGHT
        };
        let mut provider_drag_handle = None;
        ui.horizontal(|ui| {
            provider_drag_handle = Some(drag_handle(ui));
            if ui
                .add(egui::Button::new(icon_rich_text(chevron, NOTCH_TEXT_SUB)).frame(false))
                .clicked()
            {
                self.toggle_provider(&pool.provider);
            }
            if ui
                .selectable_label(
                    expanded,
                    RichText::new(provider_label).color(NOTCH_TEXT).strong(),
                )
                .clicked()
            {
                self.toggle_provider(&pool.provider);
            }
            if let Some(identity) = active_identity {
                ui.small(RichText::new(format!("({identity})")).color(NOTCH_TEXT_MUTED));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if account_count > 0 {
                    ui.small(RichText::new(format!("{account_count}")).color(NOTCH_TEXT_MUTED));
                }
            });
        });
        if let Some(quota) = self
            .ocx_reports
            .iter()
            .find(|report| report.provider == pool.provider)
            .map(|report| report.quota.clone())
        {
            let bars = quota_bars(&quota);
            if bars.len() == 1 {
                self.render_quota_bar(ui, &bars[0]);
            } else if bars.len() > 1 {
                let count = bars.len();
                ui.columns(count, |columns| {
                    for (index, bar) in bars.iter().enumerate() {
                        self.render_quota_bar_split(&mut columns[index], bar);
                    }
                });
            }
        }
        if expanded {
            if pool.provider == "openai" && !pool.accounts.is_empty() {
                self.render_auto_switch_control(ui);
            }
            for account in &pool.accounts {
                self.render_account_row(ui, &pool.provider, account);
            }
        }
        ui.separator();
        provider_drag_handle.expect("provider header always renders a drag handle")
    }

    fn render_auto_switch_control(&mut self, ui: &mut egui::Ui) {
        let mut threshold = self.auto_switch_threshold.min(100);
        let mut commit = None;
        ui.horizontal(|ui| {
            ui.small(RichText::new("풀 순환").color(NOTCH_TEXT_SUB));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.small(
                    RichText::new(if threshold == 0 {
                        "끔".to_owned()
                    } else {
                        format!("{threshold}%")
                    })
                    .color(if self.auto_switch_busy {
                        NOTCH_TEXT_MUTED
                    } else {
                        NOTCH_ACCENT
                    }),
                );
                let response = ui
                    .add_enabled(
                        !self.auto_switch_busy,
                        egui::Slider::new(&mut threshold, 0..=100).show_value(false),
                    )
                    .on_hover_text("0%는 자동 풀 순환을 끕니다");
                if response.changed() {
                    self.auto_switch_threshold = threshold;
                    if !response.dragged() {
                        commit = Some(threshold);
                    }
                }
                if response.drag_stopped() {
                    commit = Some(self.auto_switch_threshold);
                }
            });
        });
        if let Some(threshold) = commit {
            self.update_auto_switch_threshold(threshold);
        }
        ui.add_space(4.0);
    }

    fn render_account_pool(&mut self, ui: &mut egui::Ui) {
        if self.ocx_pools.is_empty() {
            ui.small(RichText::new("No account pools").color(NOTCH_TEXT_MUTED));
            return;
        }
        let mut pools_by_name: std::collections::HashMap<String, ProviderPool> = self
            .ocx_pools
            .iter()
            .map(|p| (p.provider.clone(), p.clone()))
            .collect();

        let drag_state_id = egui::Id::new("dragging_provider");
        let pointer_pos = ui.ctx().input(|input| input.pointer.hover_pos());
        let mut target_index: Option<usize> = None;

        for (idx, name) in self.provider_order.clone().iter().enumerate() {
            if let Some(pool) = pools_by_name.remove(name) {
                let scoped = ui.scope(|ui| self.render_provider_pool(ui, &pool));
                let drag_response = scoped.inner;
                let provider_rect = scoped.response.rect;

                if drag_response.drag_started() {
                    ui.data_mut(|data| data.insert_temp(drag_state_id, idx));
                }

                if let Some(src) = ui.data(|data| data.get_temp::<usize>(drag_state_id))
                    && src != idx
                    && pointer_pos.is_some_and(|pos| provider_rect.contains(pos))
                {
                    target_index = Some(idx);
                    ui.painter().rect_stroke(
                        provider_rect.shrink(1.0),
                        3.0,
                        egui::Stroke::new(1.0, NOTCH_ACCENT),
                        egui::StrokeKind::Inside,
                    );
                }
            }
        }

        if ui.ctx().input(|input| input.pointer.any_released()) {
            let source_index = ui.data(|data| data.get_temp::<usize>(drag_state_id));
            if let (Some(src), Some(tgt)) = (source_index, target_index) {
                let item = self.provider_order.remove(src);
                self.provider_order.insert(tgt, item);
            }
            ui.data_mut(|data| data.remove::<usize>(drag_state_id));
        }

        for pool in pools_by_name.values() {
            let _ = self.render_provider_pool(ui, pool);
        }
    }

    fn toggle_provider(&mut self, name: &str) {
        let name = name.to_owned();
        if !self.expanded_providers.insert(name.clone()) {
            self.expanded_providers.remove(&name);
        }
    }

    fn render_quota_bar(&self, ui: &mut egui::Ui, bar: &QuotaBar) {
        let threshold = self.auto_switch_threshold.min(100);
        let warning = (threshold > 0 && bar.percent >= threshold as f64) || bar.percent >= 99.5;
        let fill_color = if warning { NOTCH_CAUTION } else { NOTCH_GREEN };
        let percent_color = if warning {
            NOTCH_CAUTION
        } else {
            NOTCH_TEXT_SUB
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(quota_head(bar)).color(NOTCH_LABEL).small());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format_quota_percent(bar.percent))
                        .color(percent_color)
                        .small(),
                );
            });
        });
        let desired = egui::vec2(ui.available_width(), 6.0);
        let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 3.0, NOTCH_BAR_BG);
        let filled = (rect.width() * (bar.percent.clamp(0.0, 100.0) as f32 / 100.0)).round();
        if filled > 0.0 {
            let fill = filled.max(4.0).min(rect.width());
            painter.rect_filled(
                egui::Rect::from_min_size(rect.min, egui::vec2(fill, rect.height())),
                3.0,
                fill_color,
            );
        }
        ui.add_space(6.0);
    }

    fn render_quota_bar_split(&self, ui: &mut egui::Ui, bar: &QuotaBar) {
        let threshold = self.auto_switch_threshold.min(100);
        let warning = (threshold > 0 && bar.percent >= threshold as f64) || bar.percent >= 99.5;
        let fill_color = if warning { NOTCH_CAUTION } else { NOTCH_GREEN };
        let percent_color = if warning {
            NOTCH_CAUTION
        } else {
            NOTCH_TEXT_SUB
        };
        ui.vertical(|ui| {
            ui.label(RichText::new(quota_head(bar)).color(NOTCH_LABEL).small());
            let gauge = egui::vec2(ui.available_width().max(20.0), 4.0);
            let (rect, _) = ui.allocate_exact_size(gauge, egui::Sense::hover());
            let painter = ui.painter();
            painter.rect_filled(rect, 2.0, NOTCH_BAR_BG);
            let filled = (rect.width() * (bar.percent.clamp(0.0, 100.0) as f32 / 100.0)).round();
            if filled > 0.0 {
                let fill = filled.max(2.0).min(rect.width());
                painter.rect_filled(
                    egui::Rect::from_min_size(rect.min, egui::vec2(fill, rect.height())),
                    2.0,
                    fill_color,
                );
            }
            ui.label(
                RichText::new(format_quota_percent(bar.percent))
                    .color(percent_color)
                    .small(),
            );
        });
    }

    fn render_account_row(&mut self, ui: &mut egui::Ui, provider: &str, account: &ProviderAccount) {
        let (status, status_color) = if account.needs_reauth {
            ("Reauth required", NOTCH_DANGER)
        } else if account.paused {
            ("Paused", NOTCH_TEXT_MUTED)
        } else if account.active {
            ("Active", NOTCH_ACCENT)
        } else {
            (account.health.as_str(), NOTCH_TEXT_MUTED)
        };
        let busy_key = format!("{provider}:{}", account.id);
        let busy = self.account_busy.as_deref() == Some(busy_key.as_str());
        let can_pause = matches!(account.kind.as_str(), "codex" | "oauth");
        let reauth_eligible = account_reauth_eligible(provider, account);
        let mut action: Option<AccountAction> = None;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(&account.identity)
                    .color(NOTCH_TEXT_SUB)
                    .small(),
            );
            if account.active {
                let control =
                    account_pool_control(ui, false, can_pause, busy).on_hover_text(if can_pause {
                        "이 계정을 일시정지"
                    } else {
                        "현재 사용 중"
                    });
                if can_pause && control.clicked() {
                    action = Some(AccountAction::Pause);
                }
            } else {
                let control =
                    account_pool_control(ui, true, true, busy).on_hover_text("이 계정을 사용");
                if control.clicked() {
                    action = Some(AccountAction::Activate);
                }
            }
            if reauth_eligible
                && ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(icon_rich_text(LUCIDE_REFRESH, NOTCH_CAUTION))
                            .frame(false)
                            .min_size(egui::vec2(UI_CONTROL_HEIGHT, UI_CONTROL_HEIGHT)),
                    )
                    .on_hover_text("재인증")
                    .clicked()
            {
                action = Some(AccountAction::Reauth);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(status).color(status_color).small());
            });
        });
        let mut bars = Vec::new();
        if let Some(percent) = account.weekly_percent {
            bars.push(QuotaBar {
                label: "Weekly".into(),
                percent,
                reset_at: account.weekly_reset_at,
                value_label: None,
            });
        }
        if let Some(percent) = account.monthly_percent {
            bars.push(QuotaBar {
                label: "Monthly".into(),
                percent,
                reset_at: account.monthly_reset_at,
                value_label: None,
            });
        }
        if bars.len() == 1 {
            self.render_quota_bar(ui, &bars[0]);
        } else if bars.len() > 1 {
            let count = bars.len();
            ui.columns(count, |columns| {
                for (index, bar) in bars.iter().enumerate() {
                    self.render_quota_bar_split(&mut columns[index], bar);
                }
            });
        }
        if let Some(credits) = account.reset_credits {
            ui.horizontal(|ui| {
                ui.small(
                    RichText::new(format!("Reset credits: {credits}")).color(NOTCH_TEXT_MUTED),
                );
                if credits > 0 && !busy && ui.small_button("Reset").clicked() {
                    action = Some(AccountAction::Reset);
                }
                if busy {
                    ui.small("…");
                }
            });
        }
        if let Some(action) = action {
            self.run_account_action(provider, account, action);
        }
        ui.add_space(8.0);
    }

    fn activate_workspace(&mut self, path: PathBuf) {
        if self.selected_workspace.as_deref() == Some(path.as_path()) {
            return;
        }
        self.codex = CodexRuntimeController::spawn(path.clone());
        self.codex_connection = CodexConnection::Starting;
        self.codex_thread_id = None;
        self.codex_turn_id = None;
        self.codex_model = None;
        self.codex_catalog_source = None;
        self.codex_catalog.clear();
        self.selected_workspace = Some(path.clone());
        self.runtime_message = Some(format!("작업공간 전환: {}", path.display()));
    }

    fn open_workspace_picker(&mut self) {
        if self.workspace_picker.is_some() {
            return;
        }
        let handle = std::thread::spawn(|| {
            rfd::FileDialog::new()
                .set_title("로컬 폴더 선택")
                .pick_folder()
                .map(|path| path.to_path_buf())
        });
        self.workspace_picker = Some(handle);
    }

    fn drain_workspace_picker(&mut self) {
        let Some(handle) = self.workspace_picker.take() else {
            return;
        };
        if !handle.is_finished() {
            self.workspace_picker = Some(handle);
            return;
        }
        match handle.join() {
            Ok(Some(path)) => self.add_workspace(path),
            Ok(None) => {}
            Err(_) => self.runtime_message = Some("폴더 선택이 실패했습니다".to_owned()),
        }
    }

    fn add_workspace(&mut self, path: PathBuf) {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if !path.is_dir() {
            self.runtime_message = Some(format!("폴더가 아닙니다: {}", path.display()));
            return;
        }
        if let Some(existing) = self.workspaces.iter().find(|entry| entry.path == path) {
            self.activate_workspace(existing.path.clone());
            return;
        }
        let entry = WorkspaceEntry::from_path(path.clone());
        self.workspaces.push(entry);
        save_workspaces(&self.workspaces_store, &self.workspaces);
        self.activate_workspace(path);
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
            match &self.codex_connection {
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
                    // Restore: two overlapping window outlines.
                    painter.rect_stroke(
                        egui::Rect::from_center_size(
                            egui::pos2(center.x - 3.0, center.y - 3.0),
                            egui::vec2(11.0, 11.0),
                        ),
                        2.0,
                        stroke,
                        egui::StrokeKind::Inside,
                    );
                    painter.rect_stroke(
                        egui::Rect::from_center_size(
                            egui::pos2(center.x + 3.0, center.y + 3.0),
                            egui::vec2(11.0, 11.0),
                        ),
                        2.0,
                        stroke,
                        egui::StrokeKind::Inside,
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
            ui.small(RichText::new(format!("{}", self.workspaces.len())).color(NOTCH_TEXT_MUTED));
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

        let list_height = (ui.available_height() * 0.5).max(90.0);
        let mut activate = None;
        egui::ScrollArea::vertical()
            .id_salt("workspace_list")
            .max_height(list_height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for entry in &self.workspaces {
                    let selected = self.selected_workspace.as_deref() == Some(entry.path.as_path());
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
                                .session_tabs
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

        ui.separator();
        let footer_height = 58.0;
        let quota_height = (ui.available_height() - footer_height).max(80.0);
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), quota_height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("sidebar_runtime_status")
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.render_quota_panel(ui));
            },
        );
        ui.separator();
        self.render_web_account(ui);
    }

    fn render_web_account(&mut self, ui: &mut egui::Ui) {
        let (status, status_color, action) = match &self.web_browser_state {
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
                if ui.small_button(action).clicked() {
                    if matches!(self.web_browser_state, WebGptBrowserState::Offline(_)) {
                        self.web_browser.reload();
                    }
                    self.web_browser.show_login();
                }
            });
        });
    }

    fn render_workspace_content(&mut self, ui: &mut egui::Ui) {
        let session_tabs = self.session_tabs.clone();
        ui.horizontal_wrapped(|ui| {
            if session_tabs.is_empty() {
                ui.selectable_value(&mut self.selected_tab, WorkspaceTab::Chat, "Main");
            } else {
                for session in session_tabs {
                    let selected = self.selected_tab == WorkspaceTab::Chat
                        && self.selected_session_id.as_deref() == Some(session.id.as_str());
                    let runtime = match session.runtime {
                        SessionRuntime::Unified => "Main",
                        SessionRuntime::WebGpt => "WEB",
                        SessionRuntime::Codex => "Codex",
                    };
                    let title = if session.parent_session_id.is_none() {
                        "Main".to_owned()
                    } else {
                        format!("{runtime} · {}", session.title)
                    };
                    if ui.selectable_label(selected, title).clicked() {
                        self.selected_session_id = Some(session.id);
                        self.selected_tab = WorkspaceTab::Chat;
                        self.focus_composer_on_start = true;
                    }
                }
            }
            ui.separator();
            ui.selectable_value(
                &mut self.selected_tab,
                WorkspaceTab::Performance,
                "Performance PoC",
            );
        });
        if self.selected_tab != WorkspaceTab::Chat {
            if let Some(message) = self.runtime_message.as_deref() {
                ui.small(message);
            }
            ui.separator();
        }

        match self.selected_tab {
            WorkspaceTab::Chat => {
                let avail = ui.available_rect_before_wrap();
                let chat_w = avail.width().min(760.0);
                let chat_rect = egui::Rect::from_min_size(
                    egui::pos2(avail.left() + (avail.width() - chat_w) / 2.0, avail.top()),
                    egui::vec2(chat_w, avail.height()),
                );
                let mut chat_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(chat_rect)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                self.render_chat(&mut chat_ui);
            }
            WorkspaceTab::Performance => self.render_performance(ui),
        }
    }

    fn update_ime_composition(&mut self, ctx: &egui::Context) -> bool {
        let mut committed_this_frame = false;
        ctx.input(|input| {
            for event in &input.events {
                let egui::Event::Ime(ime) = event else {
                    continue;
                };
                match ime {
                    egui::ImeEvent::Preedit { text, .. } => {
                        self.ime_composing = !text.is_empty();
                    }
                    egui::ImeEvent::Commit(_) => {
                        self.ime_composing = false;
                        committed_this_frame = true;
                    }
                    _ => {}
                }
            }
        });
        self.ime_composing || committed_this_frame
    }

    fn render_chat(&mut self, ui: &mut egui::Ui) {
        let ime_submit_blocked = self.update_ime_composition(ui.ctx());
        let transcript_height = (ui.available_height() - 122.0).max(180.0);
        egui::ScrollArea::vertical()
            .id_salt("unified_chat_transcript")
            .max_height(transcript_height)
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let session_id = self.selected_session_key();
                let messages = self
                    .chat_messages
                    .get(&session_id)
                    .cloned()
                    .unwrap_or_default();
                if messages.is_empty() {
                    ui.add_space(48.0);
                    ui.vertical_centered(|ui| {
                        ui.heading("무엇을 도와드릴까요?");
                    });
                    return;
                }

                for message in &messages {
                    if let Some(texture) = &message.image {
                        let size = texture.size_vec2();
                        let display_w = size.x.clamp(32.0, 400.0);
                        let display_h = if size.x > 0.0 {
                            (size.y * (display_w / size.x)).max(32.0)
                        } else {
                            200.0
                        };
                        ui.add(
                            egui::Image::new(texture)
                                .fit_to_exact_size(egui::vec2(display_w, display_h)),
                        );
                    }
                    match message.role {
                        ChatRole::User => {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                                egui::Frame::NONE
                                    .fill(NOTCH_PANEL)
                                    .stroke(egui::Stroke::new(1.0, NOTCH_BORDER_2))
                                    .corner_radius(egui::CornerRadius::same(8))
                                    .inner_margin(egui::Margin::symmetric(10, 6))
                                    .show(ui, |ui| {
                                        ui.label(&message.text);
                                    });
                            });
                        }
                        ChatRole::Assistant => {
                            ui.label(&message.text);
                            if message.streaming {
                                ui.small("응답 중…");
                            }
                        }
                        ChatRole::Tool => {
                            ui.small(RichText::new(&message.text).monospace());
                        }
                    }
                    ui.add_space(14.0);
                }
            });

        if let Some(message) = self.runtime_message.as_deref() {
            ui.small(RichText::new(message).color(NOTCH_TEXT_MUTED));
            ui.add_space(4.0);
        }

        if let Some(path) = self.selected_workspace.as_deref() {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            ui.horizontal(|ui| {
                ui.small(RichText::new(name).color(NOTCH_TEXT_SUB).strong());
                ui.small(RichText::new(path.display().to_string()).color(NOTCH_TEXT_MUTED));
                ui.small(RichText::new("· 로컬").color(NOTCH_TEXT_MUTED));
            });
            ui.add_space(4.0);
        }

        ui.add_space(8.0);
        let dropped_paths = ui.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect::<Vec<_>>()
        });
        for path in dropped_paths {
            self.add_draft_attachment(path, None);
        }

        let mut settings_response = None;
        let mut send_clicked = false;
        let composer_response = egui::Frame::NONE
            .fill(NOTCH_PANEL)
            .stroke(egui::Stroke::new(1.0, NOTCH_BORDER_2))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                let response = ui.add(
                    TextEdit::multiline(&mut self.agent_prompt)
                        .id_salt("roche_chat_input")
                        .hint_text("메시지를 입력하세요")
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .frame(egui::Frame::NONE),
                );

                if !self.draft_attachments.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        let mut remove_index = None;
                        for (index, attachment) in self.draft_attachments.iter().enumerate() {
                            let label = format!("{}  ×", attachment.label());
                            if ui.small_button(label).clicked() {
                                remove_index = Some(index);
                            }
                        }
                        if let Some(index) = remove_index {
                            self.draft_attachments.remove(index);
                        }
                    });
                    ui.add_space(4.0);
                }

                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("+").size(18.0).color(NOTCH_TEXT_SUB))
                                .frame(false)
                                .min_size(egui::vec2(UI_CONTROL_HEIGHT, UI_CONTROL_HEIGHT)),
                        )
                        .on_hover_text("파일 첨부")
                        .clicked()
                        && let Some(paths) = rfd::FileDialog::new().pick_files()
                    {
                        for path in paths {
                            self.add_draft_attachment(path, None);
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.selected_model == ChatModel::Codex && self.codex_turn_id.is_some() {
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new("■").size(13.0).color(NOTCH_TEXT_SUB),
                                    )
                                    .min_size(egui::vec2(UI_CONTROL_HEIGHT, UI_CONTROL_HEIGHT)),
                                )
                                .clicked()
                            {
                                self.codex.interrupt();
                            }
                        } else {
                            send_clicked = ui
                                .add(
                                    egui::Button::new(
                                        RichText::new("↑").size(17.0).color(NOTCH_TEXT),
                                    )
                                    .min_size(egui::vec2(UI_CONTROL_HEIGHT, UI_CONTROL_HEIGHT)),
                                )
                                .clicked();
                        }
                        let model_label = self.selected_model_label();
                        settings_response = Some(
                            ui.add(
                                egui::Button::new(model_settings_job(
                                    &model_label,
                                    self.reasoning_level.label(),
                                ))
                                .frame(false)
                                .min_size(egui::vec2(0.0, UI_CONTROL_HEIGHT)),
                            ),
                        );
                    });
                });

                response
            });
        let response = composer_response.inner;
        if self.focus_composer_on_start {
            self.focus_composer_on_start = false;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
            response.request_focus();
        }

        // Prefer clipboard file/image attachments over normal text paste while the composer is focused.
        if response.has_focus()
            && ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::V))
        {
            let clipboard_files = clipboard_file_paths();
            if !clipboard_files.is_empty() {
                for path in clipboard_files {
                    self.add_draft_attachment(path, None);
                }
            } else if let Some(attachment) = save_clipboard_image(ui.ctx()) {
                self.add_draft_attachment(attachment.path, attachment.preview);
            }
        }

        let submit = send_clicked
            || (response.has_focus()
                && !ime_submit_blocked
                && ui.input(|input| input.key_pressed(egui::Key::Enter) && !input.modifiers.shift));
        if submit {
            self.send_chat_message();
        }

        if let Some(settings_response) = settings_response {
            if settings_response.clicked() {
                self.chat_popover_open = !self.chat_popover_open;
                if self.chat_popover_open {
                    self.chat_popover_page = ChatPopoverPage::Root;
                }
            }

            if self.chat_popover_open {
                let popover_width = 496.0;
                let popover_height = match self.chat_popover_page {
                    ChatPopoverPage::Root => 224.0,
                    ChatPopoverPage::Model => self.model_popover_height(),
                    ChatPopoverPage::Reasoning => 364.0,
                };
                let popover_pos = egui::pos2(
                    (settings_response.rect.right() - popover_width).max(8.0),
                    (settings_response.rect.top() - popover_height - 8.0).max(8.0),
                );

                let popover = egui::Area::new(egui::Id::new("chat_settings_popover"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(popover_pos)
                    .show(ui.ctx(), |ui| {
                        ui.set_min_size(egui::vec2(popover_width, popover_height));
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            match self.chat_popover_page {
                                ChatPopoverPage::Root => {
                                    let selected_model = self.selected_model_label();
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new("모델").color(NOTCH_TEXT).strong());
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui
                                                    .add(
                                                        egui::Button::new(icon_text_job(
                                                            &selected_model,
                                                            LUCIDE_CHEVRON_RIGHT,
                                                            true,
                                                            NOTCH_TEXT_SUB,
                                                        ))
                                                        .frame(false)
                                                        .min_size(egui::vec2(
                                                            0.0,
                                                            UI_CONTROL_HEIGHT,
                                                        )),
                                                    )
                                                    .clicked()
                                                {
                                                    self.chat_popover_page = ChatPopoverPage::Model;
                                                }
                                            },
                                        );
                                    });
                                    ui.add_space(4.0);
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new("추론 강도").color(NOTCH_TEXT).strong(),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui
                                                    .add(
                                                        egui::Button::new(icon_text_job(
                                                            self.reasoning_level.label(),
                                                            LUCIDE_CHEVRON_RIGHT,
                                                            true,
                                                            NOTCH_TEXT_SUB,
                                                        ))
                                                        .frame(false)
                                                        .min_size(egui::vec2(
                                                            0.0,
                                                            UI_CONTROL_HEIGHT,
                                                        )),
                                                    )
                                                    .clicked()
                                                {
                                                    self.chat_popover_page =
                                                        ChatPopoverPage::Reasoning;
                                                }
                                            },
                                        );
                                    });
                                }
                                ChatPopoverPage::Model => {
                                    if ui
                                        .add(
                                            egui::Button::new(icon_text_job(
                                                "모델",
                                                LUCIDE_CHEVRON_LEFT,
                                                false,
                                                NOTCH_TEXT_SUB,
                                            ))
                                            .frame(false)
                                            .min_size(egui::vec2(0.0, UI_CONTROL_HEIGHT)),
                                        )
                                        .clicked()
                                    {
                                        self.chat_popover_page = ChatPopoverPage::Root;
                                    }
                                    self.model_row(ui, ChatModel::Codex);
                                    self.model_row(ui, ChatModel::WebGpt56Sol);

                                    if !self.codex_catalog.is_empty() {
                                        egui::ScrollArea::vertical()
                                            .id_salt("chat_model_catalog")
                                            .max_height(448.0)
                                            .auto_shrink([false, false])
                                            .show(ui, |ui| {
                                                for model in &self.codex_catalog {
                                                    let selected =
                                                        self.selected_codex_slug.as_deref()
                                                            == Some(model.slug.as_str());
                                                    if ui
                                                        .selectable_label(
                                                            selected,
                                                            &model.display_name,
                                                        )
                                                        .clicked()
                                                    {
                                                        self.selected_model = ChatModel::Codex;
                                                        self.selected_codex_slug =
                                                            Some(model.slug.clone());
                                                        self.chat_popover_open = false;
                                                        self.chat_popover_page =
                                                            ChatPopoverPage::Root;
                                                    }
                                                }
                                            });
                                    }
                                }
                                ChatPopoverPage::Reasoning => {
                                    if ui
                                        .add(
                                            egui::Button::new(icon_text_job(
                                                "추론 강도",
                                                LUCIDE_CHEVRON_LEFT,
                                                false,
                                                NOTCH_TEXT_SUB,
                                            ))
                                            .frame(false)
                                            .min_size(egui::vec2(0.0, UI_CONTROL_HEIGHT)),
                                        )
                                        .clicked()
                                    {
                                        self.chat_popover_page = ChatPopoverPage::Root;
                                    }
                                    for level in [
                                        ReasoningLevel::Fast,
                                        ReasoningLevel::High,
                                        ReasoningLevel::VeryHigh,
                                    ] {
                                        let selected = self.reasoning_level == level;
                                        if ui.selectable_label(selected, level.label()).clicked() {
                                            self.reasoning_level = level;
                                            self.chat_popover_open = false;
                                            self.chat_popover_page = ChatPopoverPage::Root;
                                        }
                                    }
                                }
                            }
                        });
                    });

                if self.chat_popover_open
                    && ui.ctx().input(|input| input.pointer.any_pressed())
                    && let Some(pointer_pos) = ui.ctx().input(|input| input.pointer.interact_pos())
                    && !settings_response.rect.contains(pointer_pos)
                    && !popover.response.rect.contains(pointer_pos)
                {
                    self.chat_popover_open = false;
                    self.chat_popover_page = ChatPopoverPage::Root;
                }
            }
        }

        if let Some(message) = self.runtime_message.as_deref() {
            ui.add_space(4.0);
            ui.small(message);
        }
    }

    fn render_performance(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Synthetic performance fixture");
            ui.label(format!(
                "{} messages · {} tool events",
                self.workload.messages.len(),
                self.workload.tool_events.len()
            ));
        });
        ui.label(
            "This tab remains synthetic so Phase 0 virtualization can be measured independently of runtime I/O.",
        );
        ui.separator();

        ui.columns(2, |columns| {
            columns[0].strong("100,000 virtualized messages");
            let total_messages = self.workload.messages.len();
            egui::ScrollArea::vertical()
                .id_salt("performance_messages")
                .show_rows(&mut columns[0], 48.0, total_messages, |ui, row_range| {
                    for index in row_range {
                        let message = &self.workload.messages[index];
                        let actor = match message.actor {
                            SyntheticActor::User => "YOU",
                            SyntheticActor::Assistant => "CODEX",
                        };
                        ui.strong(format!("{actor} #{:06}", message.id));
                        ui.label(&message.markdown);
                    }
                });

            columns[1].strong("100,000 virtualized tool events");
            let total_tools = self.workload.tool_events.len();
            egui::ScrollArea::vertical()
                .id_salt("performance_tools")
                .show_rows(&mut columns[1], 32.0, total_tools, |ui, row_range| {
                    for index in row_range {
                        let event = &self.workload.tool_events[index];
                        let kind = match event.kind {
                            SyntheticToolKind::Search => "search",
                            SyntheticToolKind::Read => "read",
                            SyntheticToolKind::Edit => "edit",
                            SyntheticToolKind::Test => "test",
                        };
                        ui.horizontal(|ui| {
                            ui.monospace(format!("> {kind:6}"));
                            ui.label(&event.summary);
                        });
                    }
                });
        });
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
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE.fill(NOTCH_BG).inner_margin(egui::Margin {
                        left: 18,
                        right: 18,
                        top: 14,
                        bottom: 26,
                    }))
                    .show(ui, |ui| self.render_workspace_content(ui));
            });
        ui.ctx().request_repaint_after(Duration::from_millis(100));
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
    const LUCIDE_ICONS: &[u8] = include_bytes!("../../assets/lucide-icons.ttf");
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
    visuals.selection.bg_fill = NOTCH_BORDER_2;
    visuals.selection.stroke = egui::Stroke::new(1.0, NOTCH_BORDER);
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

#[cfg(test)]
mod tests {
    use super::*;

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
