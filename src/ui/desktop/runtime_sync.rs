//! Desktop command dispatch, channel draining, polling, and runtime projections.

use super::*;

impl DesktopApp {
    pub(super) fn send_chat_message(&mut self) {
        let prompt = self.composer.prompt.trim().to_owned();
        if prompt.is_empty() && self.composer.attachments.is_empty() {
            return;
        }
        let model = self.composer.selected_model;
        if matches!(
            self.runtime.codex_connection,
            CodexConnection::Offline { .. }
        ) && model == ChatModel::Codex
        {
            self.runtime_message = Some(
                "Codex CLI가 연결되지 않아 전송하지 않았습니다 (LOCAL CODEX OFFLINE)".to_owned(),
            );
            return;
        }
        if model == ChatModel::WebGpt56Sol
            && !matches!(self.runtime.web_browser_state, WebGptBrowserState::LoggedIn)
        {
            self.runtime_message = Some("[WEB] GPT 로그인이 필요합니다".to_owned());
            self.runtime.web_browser.show_login();
            return;
        }
        let session_id = self.selected_session_key();
        let attachments = std::mem::take(&mut self.composer.attachments);
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
                activity: None,
            },
        );
        self.composer.prompt.clear();

        match model {
            ChatModel::Codex => {
                self.sessions
                    .pending_codex_sessions
                    .push_back(session_id.clone());
                if let Some(thread_id) = self
                    .sessions
                    .codex_session_threads
                    .get(&session_id)
                    .cloned()
                {
                    self.runtime.codex.send_with_attachments_to_thread(
                        prompt,
                        attachment_paths,
                        self.composer.reasoning_effort.clone(),
                        self.runtime.selected_codex_slug.clone(),
                        Some(thread_id),
                    );
                } else {
                    self.runtime.codex.send_with_attachments_to_thread(
                        prompt,
                        attachment_paths,
                        self.composer.reasoning_effort.clone(),
                        self.runtime.selected_codex_slug.clone(),
                        None,
                    );
                }
            }
            ChatModel::WebGpt56Sol => {
                let request_id = format!(
                    "web-chat-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                );
                let request =
                    WebGptTurnRequest::native_chat(session_id.clone(), request_id.clone());
                self.sessions
                    .web_local_sessions
                    .insert(request_id.clone(), session_id);
                self.sessions.web_local_correlations.remove(&request_id);
                self.runtime.web_browser.submit_chat_with_attachments(
                    request,
                    prompt,
                    attachment_paths,
                );
                self.runtime_message = Some("[WEB] GPT 요청 전송 중…".to_owned());
            }
        }
    }

    pub(super) fn drain_webgpt(&mut self) {
        for event in self.runtime.webgpt.drain() {
            match event {
                WebGptRuntimeEvent::SessionsUpdated { sessions } => {
                    self.apply_session_snapshot(sessions);
                }
                WebGptRuntimeEvent::SessionCreated { session } => {
                    self.sessions.selected_id = Some(session.id.clone());
                    if !self.sessions.tabs.iter().any(|tab| tab.id == session.id) {
                        self.sessions.tabs.push(session);
                    }
                    self.workspace.selected_tab = WorkspaceTab::Chat;
                    self.refocus_composer();
                }
                WebGptRuntimeEvent::SessionRenamed { session } => {
                    self.apply_session_rename(session);
                    self.runtime_message = Some("세션 이름을 변경했습니다.".to_owned());
                }
                WebGptRuntimeEvent::SessionDeleted { session_ids } => {
                    self.remove_session_tabs(&session_ids);
                    self.runtime_message = Some("세션을 삭제했습니다.".to_owned());
                }
                WebGptRuntimeEvent::WorkerApproved { session_id } => {
                    self.runtime_message = Some(format!("워커 결과를 승인했습니다 · {session_id}"));
                }
                WebGptRuntimeEvent::Error { message, .. } => {
                    self.runtime_message = Some(message);
                }
                WebGptRuntimeEvent::Submitted { .. }
                | WebGptRuntimeEvent::Answered { .. }
                | WebGptRuntimeEvent::Cancelled { .. } => {}
            }
        }
    }

    pub(super) fn drain_web_browser(&mut self) {
        for event in self.runtime.web_browser.drain_ui() {
            match event {
                WebGptBrowserEvent::State(state) => {
                    self.runtime.web_browser_state = state;
                }
                WebGptBrowserEvent::WakeSubmitted { request_id } => {
                    self.runtime_message =
                        Some(format!("[WEB] GPT 오케스트레이션 실행 중 · {request_id}"));
                }
                WebGptBrowserEvent::ChatSubmitted { correlation } => {
                    let Some(session_id) = latch_native_web_correlation(
                        &self.sessions.web_local_sessions,
                        &mut self.sessions.web_local_correlations,
                        &correlation,
                    ) else {
                        continue;
                    };
                    let request_id = correlation.request_id.clone();
                    let messages = self.sessions.chat_messages.entry(session_id).or_default();
                    if !messages.iter().any(|message| {
                        message.role == ChatRole::Assistant
                            && message.model == ChatModel::WebGpt56Sol
                            && message.turn_id.as_deref() == Some(request_id.as_str())
                    }) {
                        messages.push(ChatMessage {
                            role: ChatRole::Assistant,
                            model: ChatModel::WebGpt56Sol,
                            text: String::new(),
                            turn_id: Some(request_id.clone()),
                            streaming: true,
                            image: None,
                            activity: None,
                        });
                    }
                    self.runtime_message = Some(format!("[WEB] GPT 생각 중… · {request_id}"));
                }
                WebGptBrowserEvent::ChatProgress {
                    correlation,
                    text,
                    activity,
                    thinking,
                } => {
                    let Some(session_id) = latch_native_web_correlation(
                        &self.sessions.web_local_sessions,
                        &mut self.sessions.web_local_correlations,
                        &correlation,
                    ) else {
                        continue;
                    };
                    let request_id = correlation.request_id.clone();
                    {
                        let messages = self
                            .sessions
                            .chat_messages
                            .entry(session_id.clone())
                            .or_default();
                        if let Some(message) = messages.iter_mut().rev().find(|message| {
                            message.role == ChatRole::Assistant
                                && message.model == ChatModel::WebGpt56Sol
                                && message.turn_id.as_deref() == Some(request_id.as_str())
                        }) {
                            if let Some(text) = text {
                                message.text = sanitize_web_assistant_text(&text);
                            }
                            message.streaming = true;
                        } else {
                            messages.push(ChatMessage {
                                role: ChatRole::Assistant,
                                model: ChatModel::WebGpt56Sol,
                                text: text
                                    .as_deref()
                                    .map(sanitize_web_assistant_text)
                                    .unwrap_or_default(),
                                turn_id: Some(request_id.clone()),
                                streaming: true,
                                image: None,
                                activity: None,
                            });
                        }
                    }
                    if let Some(activity) = activity
                        .as_deref()
                        .and_then(|value| web_activity_from_visible_text(&request_id, value))
                    {
                        self.upsert_activity(
                            &session_id,
                            &request_id,
                            ChatModel::WebGpt56Sol,
                            activity,
                        );
                    }
                    self.runtime_message = if thinking {
                        Some(format!("[WEB] GPT 생각 중… · {request_id}"))
                    } else {
                        Some(format!("[WEB] GPT 응답 중… · {request_id}"))
                    };
                }
                WebGptBrowserEvent::ChatAnswered { correlation, text } => {
                    if latch_native_web_correlation(
                        &self.sessions.web_local_sessions,
                        &mut self.sessions.web_local_correlations,
                        &correlation,
                    )
                    .is_none()
                    {
                        continue;
                    }
                    let request_id = correlation.request_id.clone();
                    if !apply_web_answer(
                        &mut self.sessions.web_local_sessions,
                        &mut self.sessions.chat_messages,
                        &request_id,
                        text,
                    ) {
                        continue;
                    }
                    self.sessions.web_local_correlations.remove(&request_id);
                    self.runtime_message = None;
                }
                WebGptBrowserEvent::ChatCancelled { correlation } => {
                    if latch_native_web_correlation(
                        &self.sessions.web_local_sessions,
                        &mut self.sessions.web_local_correlations,
                        &correlation,
                    )
                    .is_none()
                    {
                        continue;
                    }
                    let request_id = correlation.request_id.clone();
                    if !finish_web_local_session(
                        &mut self.sessions.web_local_sessions,
                        &mut self.sessions.chat_messages,
                        &request_id,
                    ) {
                        continue;
                    }
                    self.sessions.web_local_correlations.remove(&request_id);
                    self.runtime_message = Some("[WEB] GPT 요청 취소됨".to_owned());
                }
                WebGptBrowserEvent::ChatFailed {
                    correlation,
                    message,
                } => {
                    if latch_native_web_correlation(
                        &self.sessions.web_local_sessions,
                        &mut self.sessions.web_local_correlations,
                        &correlation,
                    )
                    .is_none()
                    {
                        continue;
                    }
                    let request_id = correlation.request_id.clone();
                    if !finish_web_local_session(
                        &mut self.sessions.web_local_sessions,
                        &mut self.sessions.chat_messages,
                        &request_id,
                    ) {
                        continue;
                    }
                    self.sessions.web_local_correlations.remove(&request_id);
                    self.runtime_message = Some(format!("[WEB] GPT 요청 실패 · {message}"));
                }
                WebGptBrowserEvent::ChatQueueCancelled { request } => {
                    if request.account_id != DEFAULT_LOCAL_WEB_GPT_ACCOUNT_ID
                        || request.task_id.is_some()
                    {
                        continue;
                    }
                    let Some(session_id) =
                        self.sessions.web_local_sessions.get(&request.request_id)
                    else {
                        continue;
                    };
                    if session_id != &request.session_id
                        || self
                            .sessions
                            .web_local_correlations
                            .contains_key(&request.request_id)
                    {
                        continue;
                    }
                    self.sessions.web_local_sessions.remove(&request.request_id);
                    self.runtime_message = Some("[WEB] GPT 요청 취소됨".to_owned());
                }
                WebGptBrowserEvent::Error(message) => {
                    self.runtime_message = Some(format!("[WEB] 브라우저 오류 · {message}"));
                }
            }
        }
    }

    pub(super) fn drain_codex(&mut self) {
        for event in self.runtime.codex.drain() {
            match event {
                CodexEvent::Connection(connection) => self.runtime.codex_connection = connection,
                CodexEvent::ThreadStarted { thread_id, model } => {
                    self.runtime.codex_thread_id = Some(thread_id.clone());
                    self.runtime.codex_model = model;
                    if let Some(session_id) = self.sessions.pending_codex_sessions.front().cloned()
                    {
                        self.sessions
                            .codex_session_threads
                            .insert(session_id, thread_id);
                    }
                }
                CodexEvent::StoredThreads { threads } => self.merge_codex_threads(threads),
                CodexEvent::ThreadResumeFailed { thread_id, message } => {
                    self.runtime_message = Some(format!(
                        "Codex 세션 {thread_id} 복구 실패 · 새 thread로 전환: {message}"
                    ));
                }
                CodexEvent::ThreadHistoryLoaded {
                    thread_id,
                    messages,
                } => self.apply_codex_history(&thread_id, messages),
                CodexEvent::TurnStarted { turn_id, .. } => {
                    let session_id = self
                        .sessions
                        .pending_codex_sessions
                        .pop_front()
                        .unwrap_or_else(|| self.selected_session_key());
                    self.sessions
                        .codex_turn_sessions
                        .insert(turn_id.clone(), session_id);
                    self.runtime.codex_turn_id = Some(turn_id);
                }
                CodexEvent::AssistantDelta { turn_id, delta, .. } => {
                    let session_id = self
                        .sessions
                        .codex_turn_sessions
                        .get(&turn_id)
                        .cloned()
                        .unwrap_or_else(|| self.selected_session_key());
                    let messages = self.sessions.chat_messages.entry(session_id).or_default();
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
                            activity: None,
                        });
                    }
                }
                CodexEvent::AssistantCompleted { turn_id, text, .. } => {
                    let session_id = self
                        .sessions
                        .codex_turn_sessions
                        .get(&turn_id)
                        .cloned()
                        .unwrap_or_else(|| self.selected_session_key());
                    let messages = self.sessions.chat_messages.entry(session_id).or_default();
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
                            activity: None,
                        });
                    }
                }
                CodexEvent::Activity {
                    turn_id, activity, ..
                } => {
                    let session_id = self
                        .sessions
                        .codex_turn_sessions
                        .get(&turn_id)
                        .cloned()
                        .unwrap_or_else(|| self.selected_session_key());
                    self.upsert_native_worker_session(&session_id, &turn_id, &activity);
                    self.upsert_activity(&session_id, &turn_id, ChatModel::Codex, activity);
                }
                CodexEvent::TurnCompleted {
                    turn_id, status, ..
                } => {
                    if self.runtime.codex_turn_id.as_deref() == Some(turn_id.as_str()) {
                        self.runtime.codex_turn_id = None;
                    }
                    if let Some(session_id) = self.sessions.codex_turn_sessions.remove(&turn_id)
                        && let Some(messages) = self.sessions.chat_messages.get_mut(&session_id)
                    {
                        for message in messages.iter_mut().rev() {
                            if message.turn_id.as_deref() == Some(turn_id.as_str()) {
                                message.streaming = false;
                                if let Some(group) = message.activity.as_mut() {
                                    for entry in &mut group.entries {
                                        if entry.phase == CodexActivityPhase::Running {
                                            entry.phase =
                                                if status.eq_ignore_ascii_case("completed") {
                                                    CodexActivityPhase::Completed
                                                } else {
                                                    CodexActivityPhase::Failed
                                                };
                                        }
                                    }
                                }
                            }
                        }
                    }
                    self.runtime_message = Some(format!("Codex turn {status}"));
                }
                CodexEvent::CatalogLoaded { source, models } => {
                    self.runtime.codex_catalog_source = Some(source);
                    self.runtime.codex_catalog = models;
                }
                CodexEvent::Notice(message) => self.runtime_message = Some(message),
                CodexEvent::Error(message) => self.runtime_message = Some(message),
            }
        }
    }

    pub(super) fn selected_model_label(&self) -> String {
        if let Some(slug) = self.runtime.selected_codex_slug.as_deref() {
            return self
                .runtime
                .codex_catalog
                .iter()
                .find(|model| model.slug == slug)
                .map(|model| model.display_name.clone())
                .unwrap_or_else(|| slug.to_owned());
        }
        self.composer.selected_model.label().to_owned()
    }

    pub(super) fn selected_catalog_model(&self) -> Option<&CodexCatalogModel> {
        let slug = self.runtime.selected_codex_slug.as_deref()?;
        self.runtime
            .codex_catalog
            .iter()
            .find(|model| model.slug == slug)
    }

    pub(super) fn available_reasoning_levels(&self) -> Vec<CodexReasoningLevel> {
        if self.composer.selected_model == ChatModel::Codex
            && let Some(model) = self.selected_catalog_model()
            && !model.supported_reasoning_levels.is_empty()
        {
            return model.supported_reasoning_levels.clone();
        }

        ["low", "medium", "high", "xhigh"]
            .into_iter()
            .map(|effort| CodexReasoningLevel {
                effort: effort.to_owned(),
                description: None,
            })
            .collect()
    }

    pub(super) fn reasoning_effort_label(effort: &str) -> &str {
        match effort {
            "low" => "낮음",
            "medium" => "보통",
            "high" => "높음",
            "xhigh" => "매우 높음",
            "max" => "최대",
            "ultra" => "울트라",
            other => other,
        }
    }

    pub(super) fn selected_reasoning_label(&self) -> &str {
        Self::reasoning_effort_label(&self.composer.reasoning_effort)
    }

    pub(super) fn normalize_reasoning_effort(&mut self) {
        let levels = self.available_reasoning_levels();
        if levels
            .iter()
            .any(|level| level.effort == self.composer.reasoning_effort)
        {
            return;
        }
        self.composer.reasoning_effort = self
            .selected_catalog_model()
            .and_then(|model| model.default_reasoning_level.clone())
            .filter(|default| levels.iter().any(|level| level.effort == default.as_str()))
            .or_else(|| levels.first().map(|level| level.effort.clone()))
            .unwrap_or_else(|| "medium".to_owned());
    }

    pub(super) fn model_popover_height(&self) -> f32 {
        let catalog_rows = (self.runtime.codex_catalog.len() as f32 * 32.0).min(384.0);
        52.0 + 64.0 + catalog_rows
    }

    pub(super) fn reasoning_popover_height(&self) -> f32 {
        let rows = self
            .available_reasoning_levels()
            .iter()
            .map(|level| {
                if level.description.is_some() {
                    50.0
                } else {
                    30.0
                }
            })
            .sum::<f32>();
        (52.0 + rows).min(416.0)
    }

    pub(super) fn select_chat_model(&mut self, model: ChatModel) {
        self.composer.selected_model = model;
        self.runtime.selected_codex_slug = None;
        self.normalize_reasoning_effort();
        self.composer.popover_open = false;
        self.composer.popover_page = ChatPopoverPage::Root;
        self.refocus_composer();
    }

    pub(super) fn model_row(&mut self, ui: &mut egui::Ui, model: ChatModel) {
        let selected =
            self.composer.selected_model == model && self.runtime.selected_codex_slug.is_none();
        if ui.selectable_label(selected, model.label()).clicked() {
            self.select_chat_model(model);
        }
    }

    pub(super) fn drain_ocx(&mut self) {
        for event in self.ocx_ui.controller.drain() {
            match event {
                OcxEvent::Updated { reports } => {
                    self.ocx_ui.reports = reports;
                    self.ocx_ui.online = true;
                    self.ocx_ui.status = None;
                    self.ocx_ui.power_pending = false;
                }
                OcxEvent::Error(message) => {
                    self.ocx_ui.online = false;
                    self.ocx_ui.status = Some(message);
                    self.ocx_ui.power_pending = false;
                }
            }
        }
    }

    pub(super) fn sample_memory(&mut self) {
        let now = Instant::now();
        if self.ocx_ui.memory_sample_pending
            || now.duration_since(self.ocx_ui.last_mem_sample) < Duration::from_secs(2)
        {
            return;
        }
        self.ocx_ui.last_mem_sample = now;
        self.ocx_ui.memory_sample_pending = true;
        let ocx_online = self.ocx_ui.online;
        let events = self.ocx_ui.telemetry_tx.clone();
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

    pub(super) fn drain_telemetry(&mut self) {
        while let Ok(event) = self.ocx_ui.telemetry_rx.try_recv() {
            match event {
                DesktopTelemetryEvent::Memory {
                    roche_memory,
                    mem_headroom,
                    ocx_pid,
                    ocx_memory,
                } => {
                    self.ocx_ui.roche_memory = roche_memory;
                    self.ocx_ui.mem_headroom = mem_headroom;
                    self.ocx_ui.pid = ocx_pid;
                    self.ocx_ui.memory = ocx_memory;
                    self.ocx_ui.memory_sample_pending = false;
                }
                DesktopTelemetryEvent::ProviderPools {
                    pools,
                    auto_switch_threshold,
                } => {
                    self.ocx_ui.pools = pools;
                    self.ocx_ui.auto_switch_threshold = auto_switch_threshold.min(100);
                    for pool in &self.ocx_ui.pools {
                        if !self.ocx_ui.provider_order.contains(&pool.provider) {
                            self.ocx_ui.provider_order.push(pool.provider.clone());
                        }
                    }
                    self.ocx_ui
                        .provider_order
                        .retain(|name| self.ocx_ui.pools.iter().any(|pool| pool.provider == *name));
                    self.ocx_ui.account_poll_pending = false;
                }
                DesktopTelemetryEvent::AutoSwitchUpdated(result) => {
                    self.ocx_ui.auto_switch_busy = false;
                    match result {
                        Ok(threshold) => {
                            self.ocx_ui.auto_switch_threshold = threshold.min(100);
                            self.ocx_ui.last_account_poll = Instant::now() - Duration::from_secs(5);
                        }
                        Err(error) => self.runtime_message = Some(error),
                    }
                }
                DesktopTelemetryEvent::AccountActionFinished { busy_key, result } => {
                    if self.ocx_ui.account_busy.as_deref() == Some(busy_key.as_str()) {
                        self.ocx_ui.account_busy = None;
                    }
                    if let Err(error) = result {
                        self.runtime_message = Some(error);
                    }
                    self.ocx_ui.last_account_poll = Instant::now() - Duration::from_secs(5);
                    self.ocx_ui.controller.refresh();
                }
                DesktopTelemetryEvent::OcxSettingsLoaded(result) => {
                    self.ocx_ui.settings_poll_pending = false;
                    match result {
                        Ok((models, subagent_models, injection_settings)) => {
                            self.ocx_ui.models = models;
                            self.ocx_ui.subagent_models = subagent_models;
                            self.ocx_ui.injection_settings = injection_settings;
                        }
                        Err(error) => self.runtime_message = Some(error),
                    }
                }
                DesktopTelemetryEvent::OcxSettingsAction(result) => {
                    self.ocx_ui.settings_action_pending = false;
                    if let Err(error) = result {
                        self.runtime_message = Some(error);
                    }
                    self.ocx_ui.last_settings_poll = Instant::now() - Duration::from_secs(5);
                }
            }
        }
    }

    pub(super) fn toggle_power(&mut self) {
        if self.ocx_ui.power_pending {
            return;
        }
        self.ocx_ui.power_pending = true;
        let action = if self.ocx_ui.online { "stop" } else { "start" }.to_owned();
        std::thread::spawn(move || {
            let _ = run_ocx(&action);
        });
        self.ocx_ui.controller.refresh();
    }

    pub(super) fn poll_accounts(&mut self) {
        let now = Instant::now();
        if self.ocx_ui.account_poll_pending
            || now.duration_since(self.ocx_ui.last_account_poll) < Duration::from_secs(5)
        {
            return;
        }
        self.ocx_ui.last_account_poll = now;
        if !self.ocx_ui.online {
            self.ocx_ui.pools.clear();
            return;
        }

        // Network-backed account discovery must never run on the egui UI thread.
        let report_providers: Vec<String> = self
            .ocx_ui
            .reports
            .iter()
            .map(|report| report.provider.clone())
            .collect();
        self.ocx_ui.account_poll_pending = true;
        let events = self.ocx_ui.telemetry_tx.clone();
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

    pub(super) fn poll_ocx_settings(&mut self) {
        let now = Instant::now();
        if self.ocx_ui.settings_poll_pending
            || now.duration_since(self.ocx_ui.last_settings_poll) < Duration::from_secs(5)
            || !self.ocx_ui.online
        {
            return;
        }
        self.ocx_ui.last_settings_poll = now;
        self.ocx_ui.settings_poll_pending = true;
        let events = self.ocx_ui.telemetry_tx.clone();
        std::thread::spawn(move || {
            let result = fetch_ocx_models().and_then(|models| {
                fetch_subagent_models().and_then(|subagent_models| {
                    fetch_injection_settings()
                        .map(|injection_settings| (models, subagent_models, injection_settings))
                })
            });
            let _ = events.send(DesktopTelemetryEvent::OcxSettingsLoaded(result));
        });
    }

    pub(super) fn save_model_visibility(&mut self, model: OcxModel, enabled: bool) {
        if self.ocx_ui.settings_action_pending {
            return;
        }
        self.ocx_ui.settings_action_pending = true;
        let events = self.ocx_ui.telemetry_tx.clone();
        std::thread::spawn(move || {
            let result = set_model_visibility(&model, enabled);
            let _ = events.send(DesktopTelemetryEvent::OcxSettingsAction(result));
        });
    }

    pub(super) fn save_subagent_models(&mut self, models: Vec<String>) {
        if self.ocx_ui.settings_action_pending {
            return;
        }
        self.ocx_ui.settings_action_pending = true;
        let events = self.ocx_ui.telemetry_tx.clone();
        std::thread::spawn(move || {
            let result = set_subagent_models(&models);
            let _ = events.send(DesktopTelemetryEvent::OcxSettingsAction(result));
        });
    }

    pub(super) fn save_injection_settings(&mut self, settings: OcxInjectionSettings) {
        if self.ocx_ui.settings_action_pending {
            return;
        }
        self.ocx_ui.settings_action_pending = true;
        let events = self.ocx_ui.telemetry_tx.clone();
        std::thread::spawn(move || {
            let result = set_injection_settings(&settings);
            let _ = events.send(DesktopTelemetryEvent::OcxSettingsAction(result));
        });
    }

    pub(super) fn run_account_action(
        &mut self,
        provider: &str,
        account: &ProviderAccount,
        action: AccountAction,
    ) {
        if self.ocx_ui.account_busy.is_some() {
            return;
        }
        let provider = provider.to_owned();
        let kind = account.kind.clone();
        let id = account.id.clone();
        let was_paused = account.paused;
        let busy_key = format!("{provider}:{id}");
        self.ocx_ui.account_busy = Some(busy_key.clone());
        let events = self.ocx_ui.telemetry_tx.clone();
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

    pub(super) fn update_auto_switch_threshold(&mut self, threshold: u32) {
        if self.ocx_ui.auto_switch_busy {
            return;
        }
        let threshold = threshold.min(100);
        self.ocx_ui.auto_switch_threshold = threshold;
        self.ocx_ui.auto_switch_busy = true;
        let events = self.ocx_ui.telemetry_tx.clone();
        std::thread::spawn(move || {
            let result = set_auto_switch_threshold(threshold).map(|_| threshold);
            let _ = events.send(DesktopTelemetryEvent::AutoSwitchUpdated(result));
        });
    }

    pub(super) fn add_workspace(&mut self, path: PathBuf) {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if !path.is_dir() {
            self.runtime_message = Some(format!("폴더가 아닙니다: {}", path.display()));
            return;
        }
        if let Some(existing) = self
            .workspace
            .workspaces
            .iter()
            .find(|entry| entry.path == path)
        {
            self.activate_workspace(existing.path.clone());
            return;
        }
        let entry = WorkspaceEntry::from_path(path.clone());
        self.workspace.workspaces.push(entry);
        save_workspaces(&self.workspace.workspaces_store, &self.workspace.workspaces);
        self.activate_workspace(path);
    }
}
