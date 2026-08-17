use std::time::{Duration, Instant};

use eframe::egui::{self, RichText, TextEdit};

use crate::{
    models::{AgentRuntimeStatus, ProjectId, Task, TaskStatus},
    perf::{
        SyntheticActor, SyntheticSession, SyntheticToolKind, SyntheticWorkload, TerminalRingBuffer,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceTab {
    Conversation,
    Tools,
    Terminal,
    Task,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionFilter {
    All,
    Attention,
    Working,
    Idle,
    Done,
}

pub struct DesktopApp {
    project_id: ProjectId,
    workload: SyntheticWorkload,
    terminal: TerminalRingBuffer,
    tasks: Vec<Task>,
    selected_session_id: Option<u64>,
    selected_tab: WorkspaceTab,
    session_filter: SessionFilter,
    session_query: String,
    filtered_session_indices: Vec<usize>,
    new_task_title: String,
    last_filter_key: String,
    started_at: Instant,
}

impl DesktopApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let workload = SyntheticWorkload::standard();
        let selected_session_id = workload.sessions.first().map(|session| session.id);
        let mut terminal = TerminalRingBuffer::new(500);
        for index in 0..2_000 {
            terminal.push_complete_line(&format!(
                "[codex] synthetic terminal line {index:04}: workspace remains responsive while history stays bounded"
            ));
        }

        let mut app = Self {
            project_id: ProjectId::new(),
            workload,
            terminal,
            tasks: Vec::new(),
            selected_session_id,
            selected_tab: WorkspaceTab::Conversation,
            session_filter: SessionFilter::All,
            session_query: String::new(),
            filtered_session_indices: Vec::new(),
            new_task_title: String::new(),
            last_filter_key: String::new(),
            started_at: Instant::now(),
        };
        app.rebuild_session_filter();
        app
    }

    fn filter_key(&self) -> String {
        format!("{:?}|{}", self.session_filter, self.session_query.trim())
    }

    fn rebuild_session_filter(&mut self) {
        let query = self.session_query.trim().to_ascii_lowercase();
        self.filtered_session_indices = self
            .workload
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| {
                let matches_status = match self.session_filter {
                    SessionFilter::All => true,
                    SessionFilter::Attention => session.attention_required,
                    SessionFilter::Working => session.runtime_status == AgentRuntimeStatus::Working,
                    SessionFilter::Idle => session.runtime_status == AgentRuntimeStatus::Idle,
                    SessionFilter::Done => session.runtime_status == AgentRuntimeStatus::Done,
                };
                let matches_query = query.is_empty()
                    || session.task_name.to_ascii_lowercase().contains(&query)
                    || session
                        .recent_activity
                        .to_ascii_lowercase()
                        .contains(&query);
                matches_status && matches_query
            })
            .map(|(index, _)| index)
            .collect();
        self.last_filter_key = self.filter_key();
    }

    fn selected_session(&self) -> Option<&SyntheticSession> {
        let selected = self.selected_session_id?;
        self.workload
            .sessions
            .iter()
            .find(|session| session.id == selected)
    }

    fn create_local_task(&mut self) {
        let title = self.new_task_title.trim();
        if title.is_empty() {
            return;
        }

        self.tasks.push(Task::new(self.project_id, title));
        self.new_task_title.clear();
        self.selected_tab = WorkspaceTab::Task;
    }

    fn render_top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top_bar")
            .default_size(48.0)
            .min_size(48.0)
            .max_size(48.0)
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.heading("Roche AI Workstation");
                    ui.separator();
                    ui.label(RichText::new("LOCAL PERF MODE").strong());
                    ui.separator();
                    ui.label(format!(
                        "{} sessions · {} messages · {} tool events",
                        self.workload.sessions.len(),
                        self.workload.messages.len(),
                        self.workload.tool_events.len()
                    ));
                    ui.separator();
                    ui.label(format!("up {}s", self.started_at.elapsed().as_secs()));
                });
            });
    }

    fn render_sidebar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("session_sidebar")
            .resizable(true)
            .default_size(300.0)
            .min_size(230.0)
            .max_size(440.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("PROJECT / AGENTS");
                    ui.label(format!("{} visible", self.filtered_session_indices.len()));
                });
                ui.add_space(6.0);

                let query_changed = ui
                    .add(
                        TextEdit::singleline(&mut self.session_query)
                            .hint_text("Filter task or activity…")
                            .desired_width(f32::INFINITY),
                    )
                    .changed();

                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(&mut self.session_filter, SessionFilter::All, "All");
                    ui.selectable_value(
                        &mut self.session_filter,
                        SessionFilter::Attention,
                        "Attention",
                    );
                    ui.selectable_value(
                        &mut self.session_filter,
                        SessionFilter::Working,
                        "Working",
                    );
                    ui.selectable_value(&mut self.session_filter, SessionFilter::Idle, "Idle");
                    ui.selectable_value(&mut self.session_filter, SessionFilter::Done, "Done");
                });

                if query_changed || self.filter_key() != self.last_filter_key {
                    self.rebuild_session_filter();
                }

                ui.separator();
                let row_height = 68.0;
                let total = self.filtered_session_indices.len();
                egui::ScrollArea::vertical()
                    .id_salt("session_list")
                    .auto_shrink([false, false])
                    .show_rows(ui, row_height, total, |ui, row_range| {
                        for filtered_index in row_range {
                            let source_index = self.filtered_session_indices[filtered_index];
                            let session = &self.workload.sessions[source_index];
                            let selected = self.selected_session_id == Some(session.id);
                            let status = runtime_status_label(session.runtime_status);
                            let title = format!("{}  ·  {}", session.task_name, status);
                            let response =
                                ui.selectable_label(selected, RichText::new(title).strong());
                            ui.horizontal(|ui| {
                                ui.small(format!("{} files", session.changed_files));
                                if session.attention_required {
                                    ui.small(RichText::new("NEEDS ATTENTION").strong());
                                }
                            });
                            ui.small(&session.recent_activity);
                            if response.clicked() {
                                self.selected_session_id = Some(session.id);
                            }
                            ui.add_space(3.0);
                        }
                    });

                ui.separator();
                ui.horizontal(|ui| {
                    let response = ui.add(
                        TextEdit::singleline(&mut self.new_task_title)
                            .hint_text("New task…")
                            .desired_width(190.0),
                    );
                    let submitted = response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    if ui.button("Create").clicked() || submitted {
                        self.create_local_task();
                    }
                });
            });
    }

    fn render_workspace(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            let selected_summary = self
                .selected_session()
                .map(|session| {
                    format!(
                        "{} · {} · {} changed files",
                        session.task_name,
                        runtime_status_label(session.runtime_status),
                        session.changed_files
                    )
                })
                .unwrap_or_else(|| "No session selected".to_owned());

            ui.horizontal(|ui| {
                ui.heading(selected_summary);
            });
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.selected_tab, WorkspaceTab::Conversation, "Chat");
                ui.selectable_value(&mut self.selected_tab, WorkspaceTab::Tools, "Tools");
                ui.selectable_value(&mut self.selected_tab, WorkspaceTab::Terminal, "Terminal");
                ui.selectable_value(&mut self.selected_tab, WorkspaceTab::Task, "Task");
            });
            ui.separator();

            match self.selected_tab {
                WorkspaceTab::Conversation => self.render_conversation(ui),
                WorkspaceTab::Tools => self.render_tools(ui),
                WorkspaceTab::Terminal => self.render_terminal(ui),
                WorkspaceTab::Task => self.render_tasks(ui),
            }
        });
    }

    fn render_conversation(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Virtualized conversation");
            ui.label("100,000 rows · only viewport rows are materialized");
        });
        ui.separator();

        let total = self.workload.messages.len();
        egui::ScrollArea::vertical()
            .id_salt("conversation")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show_rows(ui, 62.0, total, |ui, row_range| {
                for index in row_range {
                    let message = &self.workload.messages[index];
                    let actor = match message.actor {
                        SyntheticActor::User => "YOU",
                        SyntheticActor::Assistant => "CODEX",
                    };
                    ui.horizontal(|ui| {
                        ui.strong(format!("{actor}  #{:06}", message.id));
                    });
                    ui.label(&message.markdown);
                    ui.separator();
                }
            });
    }

    fn render_tools(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Tool activity");
            ui.label("raw payloads stay unloaded until a future inspector requests them");
        });
        ui.separator();

        let total = self.workload.tool_events.len();
        egui::ScrollArea::vertical()
            .id_salt("tool_events")
            .auto_shrink([false, false])
            .show_rows(ui, 34.0, total, |ui, row_range| {
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
                        ui.small(format!("{} B payload", event.payload_bytes));
                    });
                }
            });
    }

    fn render_terminal(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Recent terminal output");
            ui.label(format!(
                "{} resident lines · {} total lines ingested",
                self.terminal.resident_lines(),
                self.terminal.total_lines_ingested()
            ));
        });
        ui.separator();

        let lines = self.terminal.lines().collect::<Vec<_>>();
        egui::ScrollArea::vertical()
            .id_salt("terminal")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show_rows(ui, 20.0, lines.len(), |ui, row_range| {
                for index in row_range {
                    ui.monospace(lines[index]);
                }
            });
    }

    fn render_tasks(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Local tasks");
            ui.label("Herdr launch wiring is the next integration step");
        });
        ui.separator();

        if self.tasks.is_empty() {
            ui.label("No local tasks yet. Create one from the bottom of the sidebar.");
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            for task in self.tasks.iter().rev() {
                ui.horizontal(|ui| {
                    ui.strong(&task.title);
                    ui.label(task_status_label(task.status));
                });
                ui.small(format!("Task ID: {}", task.id.0));
                ui.separator();
            }
        });
    }
}

impl eframe::App for DesktopApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.render_top_bar(ui);
        self.render_sidebar(ui);
        self.render_workspace(ui);
        ui.ctx().request_repaint_after(Duration::from_secs(1));
    }
}

fn runtime_status_label(status: AgentRuntimeStatus) -> &'static str {
    match status {
        AgentRuntimeStatus::Working => "WORKING",
        AgentRuntimeStatus::Blocked => "BLOCKED",
        AgentRuntimeStatus::Idle => "IDLE",
        AgentRuntimeStatus::Done => "DONE",
        AgentRuntimeStatus::Unknown => "UNKNOWN",
    }
}

fn task_status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "QUEUED",
        TaskStatus::Preparing => "PREPARING",
        TaskStatus::RunningCodex => "RUNNING CODEX",
        TaskStatus::Verifying => "VERIFYING",
        TaskStatus::NeedsReview => "NEEDS REVIEW",
        TaskStatus::Failed => "FAILED",
        TaskStatus::Cancelled => "CANCELLED",
        TaskStatus::Completed => "COMPLETED",
    }
}
