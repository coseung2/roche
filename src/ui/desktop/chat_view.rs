//! Chat transcript, activity, composer, attachment, and IME rendering.

use super::*;

impl DesktopApp {
    fn render_activity_group(&mut self, ui: &mut egui::Ui, group: &ChatActivityGroup) {
        let open = self.sessions.expanded_activity_groups.contains(&group.id);
        let phase = group.phase();
        let color = match phase {
            CodexActivityPhase::Running => NOTCH_TEXT_SUB,
            CodexActivityPhase::Completed => NOTCH_TEXT,
            CodexActivityPhase::Failed => NOTCH_DANGER,
        };
        let chevron = if open { "⌄" } else { "›" };
        let header = format!("{{}} {} {chevron}", group.status_label());
        if ui
            .add(
                egui::Button::new(RichText::new(header).color(color))
                    .frame(false)
                    .min_size(egui::vec2(0.0, UI_CONTROL_HEIGHT)),
            )
            .clicked()
        {
            if open {
                self.sessions.expanded_activity_groups.remove(&group.id);
            } else {
                self.sessions
                    .expanded_activity_groups
                    .insert(group.id.clone());
            }
        }

        if open {
            ui.indent((&group.id, "activity_detail"), |ui| {
                for (index, entry) in group.entries.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&entry.title).color(NOTCH_TEXT_SUB).small());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let (label, color) = match entry.phase {
                                CodexActivityPhase::Running => ("진행 중", NOTCH_TEXT_MUTED),
                                CodexActivityPhase::Completed => ("완료", NOTCH_TEXT_MUTED),
                                CodexActivityPhase::Failed => ("실패", NOTCH_DANGER),
                            };
                            ui.small(RichText::new(label).color(color));
                        });
                    });
                    if !entry.detail.is_empty() && entry.detail != entry.title {
                        match group.kind {
                            CodexActivityKind::Terminal => {
                                ui.small(
                                    RichText::new(&entry.detail)
                                        .monospace()
                                        .color(NOTCH_TEXT_MUTED),
                                );
                            }
                            _ => {
                                ui.small(RichText::new(&entry.detail).color(NOTCH_TEXT_MUTED));
                            }
                        }
                    }
                    if index + 1 < group.entries.len() {
                        ui.add_space(4.0);
                    }
                }
            });
        }
    }

    fn update_ime_composition(&mut self, ctx: &egui::Context) -> bool {
        let mut committed_this_frame = false;
        ctx.input(|input| {
            for event in &input.events {
                let egui::Event::Ime(ime) = event else {
                    continue;
                };
                committed_this_frame |= apply_ime_event(&mut self.composer.ime_composing, ime);
            }
        });
        self.composer.ime_composing || committed_this_frame
    }

    pub(super) fn render_chat(&mut self, ui: &mut egui::Ui) {
        let ime_submit_blocked = self.update_ime_composition(ui.ctx());
        // The footer is content-sized and must reserve its space before the transcript.
        // Do not replace this with a fixed transcript-height subtraction: status rows and
        // attachment chips make the composer height dynamic and previously erased the bottom gap.
        egui::Panel::bottom("unified_chat_footer")
            .show_separator_line(false)
            .frame(egui::Frame::NONE)
            .show(ui, |ui| self.render_chat_footer(ui, ime_submit_blocked));

        egui::ScrollArea::vertical()
            .id_salt("unified_chat_transcript")
            .max_height(ui.available_height())
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let session_id = self.selected_session_key();
                let messages = self
                    .sessions
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
                                let max_width = ui.available_width().min(CHAT_USER_MAX_WIDTH);
                                ui.allocate_ui_with_layout(
                                    egui::vec2(max_width, 0.0),
                                    egui::Layout::top_down(egui::Align::Max),
                                    |ui| {
                                        ui.set_max_width(max_width);
                                        egui::Frame::NONE
                                            .fill(NOTCH_PANEL)
                                            .stroke(egui::Stroke::new(1.0, NOTCH_BORDER_2))
                                            .corner_radius(egui::CornerRadius::same(8))
                                            .inner_margin(egui::Margin::symmetric(10, 6))
                                            .show(ui, |ui| {
                                                ui.set_max_width((max_width - 20.0).max(0.0));
                                                ui.with_layout(
                                                    egui::Layout::top_down(egui::Align::Min),
                                                    |ui| {
                                                        ui.add(
                                                            egui::Label::new(&message.text)
                                                                .halign(egui::Align::Min),
                                                        );
                                                    },
                                                );
                                            });
                                    },
                                );
                            });
                        }
                        ChatRole::Assistant => {
                            ui.allocate_ui_with_layout(
                                egui::vec2(ui.available_width().min(CHAT_ASSISTANT_MAX_WIDTH), 0.0),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    ui.set_max_width(CHAT_ASSISTANT_MAX_WIDTH);
                                    if message.text.trim().is_empty() && message.streaming {
                                        ui.label(RichText::new("생각 중…").color(NOTCH_TEXT_MUTED));
                                    } else {
                                        ui.label(&message.text);
                                    }
                                },
                            );
                            if message.streaming && !message.text.trim().is_empty() {
                                ui.small("응답 중…");
                            }
                        }
                        ChatRole::Activity => {
                            if let Some(group) = message.activity.as_ref() {
                                self.render_activity_group(ui, group);
                            } else {
                                ui.small(RichText::new(&message.text).color(NOTCH_TEXT_MUTED));
                            }
                        }
                        ChatRole::Tool => {
                            if let Some(group) = message.activity.as_ref() {
                                // Backward compatibility for a persisted pre-Activity row.
                                self.render_activity_group(ui, group);
                            } else if message.model == ChatModel::WebGpt56Sol {
                                ui.small(
                                    RichText::new(format!("활동 · {}", message.text))
                                        .color(NOTCH_TEXT_MUTED),
                                );
                            } else {
                                ui.small(RichText::new(&message.text).monospace());
                            }
                        }
                    }
                    ui.add_space(14.0);
                }
            });
    }

    fn render_chat_footer(&mut self, ui: &mut egui::Ui, ime_submit_blocked: bool) {
        if let Some(message) = self.runtime_message.as_deref() {
            ui.small(RichText::new(message).color(NOTCH_TEXT_MUTED));
            ui.add_space(4.0);
        }

        if let Some(path) = self.workspace.selected_workspace.as_deref() {
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
                    TextEdit::multiline(&mut self.composer.prompt)
                        .id_salt("roche_chat_input")
                        .return_key(egui::KeyboardShortcut::new(
                            egui::Modifiers::SHIFT,
                            egui::Key::Enter,
                        ))
                        .hint_text("메시지를 입력하세요")
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .frame(egui::Frame::NONE),
                );

                if !self.composer.attachments.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        let mut remove_index = None;
                        for (index, attachment) in self.composer.attachments.iter().enumerate() {
                            let label = format!("{}  ×", attachment.label());
                            if ui.small_button(label).clicked() {
                                remove_index = Some(index);
                            }
                        }
                        if let Some(index) = remove_index {
                            self.composer.attachments.remove(index);
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
                        if self.composer.selected_model == ChatModel::Codex
                            && self.runtime.codex_turn_id.is_some()
                        {
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new("■").size(13.0).color(NOTCH_TEXT_SUB),
                                    )
                                    .min_size(egui::vec2(UI_CONTROL_HEIGHT, UI_CONTROL_HEIGHT)),
                                )
                                .clicked()
                            {
                                self.runtime.codex.interrupt();
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
                                    self.selected_reasoning_label(),
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
        if !response.has_focus() {
            self.composer.ime_composing = false;
        }
        if self.composer.focus_on_start {
            self.composer.focus_on_start = false;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
            response.request_focus();
        }

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
                self.composer.popover_open = !self.composer.popover_open;
                if self.composer.popover_open {
                    self.composer.popover_page = ChatPopoverPage::Root;
                }
            }

            if self.composer.popover_open {
                let popover_width = 496.0;
                let popover_height = match self.composer.popover_page {
                    ChatPopoverPage::Root => 92.0,
                    ChatPopoverPage::Model => self.model_popover_height(),
                    ChatPopoverPage::Reasoning => self.reasoning_popover_height(),
                };
                let viewport = ui.ctx().content_rect();
                let margin = 8.0;
                let popover_x = (settings_response.rect.right() - popover_width).clamp(
                    viewport.left() + margin,
                    viewport.right() - popover_width - margin,
                );
                let above_y = settings_response.rect.top() - popover_height - margin;
                let popover_y = if above_y >= viewport.top() + margin {
                    above_y
                } else {
                    (settings_response.rect.bottom() + margin)
                        .min(viewport.bottom() - popover_height - margin)
                };
                let popover_pos = egui::pos2(popover_x, popover_y);

                let popover = egui::Area::new(egui::Id::new("chat_settings_popover"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(popover_pos)
                    .show(ui.ctx(), |ui| {
                        ui.set_min_size(egui::vec2(popover_width, popover_height));
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            match self.composer.popover_page {
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
                                                    self.composer.popover_page =
                                                        ChatPopoverPage::Model;
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
                                                            self.selected_reasoning_label(),
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
                                                    self.composer.popover_page =
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
                                        self.composer.popover_page = ChatPopoverPage::Root;
                                    }
                                    self.model_row(ui, ChatModel::Codex);
                                    self.model_row(ui, ChatModel::WebGpt56Sol);

                                    if !self.runtime.codex_catalog.is_empty() {
                                        let catalog = self.runtime.codex_catalog.clone();
                                        egui::ScrollArea::vertical()
                                            .id_salt("chat_model_catalog")
                                            .max_height(448.0)
                                            .auto_shrink([false, false])
                                            .show(ui, |ui| {
                                                for model in &catalog {
                                                    let selected =
                                                        self.runtime.selected_codex_slug.as_deref()
                                                            == Some(model.slug.as_str());
                                                    if ui
                                                        .selectable_label(
                                                            selected,
                                                            &model.display_name,
                                                        )
                                                        .clicked()
                                                    {
                                                        self.composer.selected_model =
                                                            ChatModel::Codex;
                                                        self.runtime.selected_codex_slug =
                                                            Some(model.slug.clone());
                                                        self.normalize_reasoning_effort();
                                                        self.composer.popover_open = false;
                                                        self.composer.popover_page =
                                                            ChatPopoverPage::Root;
                                                        self.refocus_composer();
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
                                        self.composer.popover_page = ChatPopoverPage::Root;
                                    }
                                    for level in self.available_reasoning_levels() {
                                        let selected =
                                            self.composer.reasoning_effort == level.effort;
                                        let mut row = egui::text::LayoutJob::default();
                                        row.append(
                                            Self::reasoning_effort_label(&level.effort),
                                            0.0,
                                            body_text_format(if selected {
                                                NOTCH_ACCENT
                                            } else {
                                                NOTCH_TEXT
                                            }),
                                        );
                                        if let Some(description) = level.description.as_deref() {
                                            row.append(
                                                "\n",
                                                0.0,
                                                body_text_format(NOTCH_TEXT_MUTED),
                                            );
                                            row.append(
                                                description,
                                                0.0,
                                                egui::TextFormat {
                                                    font_id: egui::FontId::proportional(
                                                        UI_FONT_SMALL,
                                                    ),
                                                    color: NOTCH_TEXT_MUTED,
                                                    ..Default::default()
                                                },
                                            );
                                        }
                                        if ui
                                            .add(egui::Button::new(row).frame(false).min_size(
                                                egui::vec2(
                                                    ui.available_width(),
                                                    if level.description.is_some() {
                                                        46.0
                                                    } else {
                                                        UI_CONTROL_HEIGHT
                                                    },
                                                ),
                                            ))
                                            .clicked()
                                        {
                                            self.composer.reasoning_effort = level.effort;
                                            self.composer.popover_open = false;
                                            self.composer.popover_page = ChatPopoverPage::Root;
                                            self.refocus_composer();
                                        }
                                    }
                                }
                            }
                        });
                    });

                if self.composer.popover_open
                    && ui.ctx().input(|input| input.pointer.any_pressed())
                    && let Some(pointer_pos) = ui.ctx().input(|input| input.pointer.interact_pos())
                    && !settings_response.rect.contains(pointer_pos)
                    && !popover.response.rect.contains(pointer_pos)
                {
                    self.composer.popover_open = false;
                    self.composer.popover_page = ChatPopoverPage::Root;
                }
            }
        }
    }
}
