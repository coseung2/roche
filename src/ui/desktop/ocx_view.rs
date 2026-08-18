//! OCX process, quota, provider, and account dashboard rendering.

use super::*;

impl DesktopApp {
    pub(super) fn render_ocx_dashboard(&mut self, ui: &mut egui::Ui) {
        let mut power_clicked = false;
        ui.horizontal(|ui| {
            let power_color = if self.ocx_ui.online {
                NOTCH_ACCENT
            } else {
                NOTCH_TEXT_SUB
            };
            let size = egui::vec2(UI_LINE_HEIGHT, UI_LINE_HEIGHT);
            let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
            power_clicked = response.clicked();
            draw_power_icon(ui.painter(), rect, power_color);
            ui.vertical(|ui| {
                ui.strong(RichText::new("OpenCodex").color(NOTCH_TEXT));
                ui.small(
                    RichText::new(if self.ocx_ui.online {
                        "127.0.0.1:10100 · 연결됨"
                    } else {
                        "127.0.0.1:10100 · 연결 안 됨"
                    })
                    .color(power_color),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(icon_rich_text(LUCIDE_REFRESH, NOTCH_TEXT_SUB))
                            .frame(false),
                    )
                    .on_hover_text("새로고침")
                    .clicked()
                {
                    self.ocx_ui.controller.refresh();
                }
            });
        });
        if power_clicked {
            self.toggle_power();
        }
        if let Some(status) = self.ocx_ui.status.as_deref() {
            ui.small(RichText::new(status).color(NOTCH_TEXT_MUTED));
        }
        ui.add_space(8.0);
        self.render_process_memory(ui, "OCX", self.ocx_ui.memory);
        self.render_process_memory(ui, "Roche", self.ocx_ui.roche_memory);
        ui.separator();
        self.render_account_pool(ui);
    }

    fn render_process_memory(&self, ui: &mut egui::Ui, name: &str, memory: ProcessMemory) {
        let headroom = self.ocx_ui.mem_headroom;
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
        let expanded = self.ocx_ui.expanded_providers.contains(&pool.provider);
        let account_count = pool.accounts.len();
        let provider_label = self
            .ocx_ui
            .reports
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
            .ocx_ui
            .reports
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
        let mut threshold = self.ocx_ui.auto_switch_threshold.min(100);
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
                    .color(if self.ocx_ui.auto_switch_busy {
                        NOTCH_TEXT_MUTED
                    } else {
                        NOTCH_ACCENT
                    }),
                );
                let response = ui
                    .add_enabled(
                        !self.ocx_ui.auto_switch_busy,
                        egui::Slider::new(&mut threshold, 0..=100).show_value(false),
                    )
                    .on_hover_text("0%는 자동 풀 순환을 끕니다");
                if response.changed() {
                    self.ocx_ui.auto_switch_threshold = threshold;
                    if !response.dragged() {
                        commit = Some(threshold);
                    }
                }
                if response.drag_stopped() {
                    commit = Some(self.ocx_ui.auto_switch_threshold);
                }
            });
        });
        if let Some(threshold) = commit {
            self.update_auto_switch_threshold(threshold);
        }
        ui.add_space(4.0);
    }

    fn render_account_pool(&mut self, ui: &mut egui::Ui) {
        if self.ocx_ui.pools.is_empty() {
            ui.small(RichText::new("No account pools").color(NOTCH_TEXT_MUTED));
            return;
        }
        let mut pools_by_name: std::collections::HashMap<String, ProviderPool> = self
            .ocx_ui
            .pools
            .iter()
            .map(|p| (p.provider.clone(), p.clone()))
            .collect();

        let drag_state_id = egui::Id::new("dragging_provider");
        let pointer_pos = ui.ctx().input(|input| input.pointer.hover_pos());
        let mut target_index: Option<usize> = None;

        for (idx, name) in self.ocx_ui.provider_order.clone().iter().enumerate() {
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
                let item = self.ocx_ui.provider_order.remove(src);
                self.ocx_ui.provider_order.insert(tgt, item);
            }
            ui.data_mut(|data| data.remove::<usize>(drag_state_id));
        }

        for pool in pools_by_name.values() {
            let _ = self.render_provider_pool(ui, pool);
        }
    }

    fn toggle_provider(&mut self, name: &str) {
        let name = name.to_owned();
        if !self.ocx_ui.expanded_providers.insert(name.clone()) {
            self.ocx_ui.expanded_providers.remove(&name);
        }
    }

    fn render_quota_bar(&self, ui: &mut egui::Ui, bar: &QuotaBar) {
        let threshold = self.ocx_ui.auto_switch_threshold.min(100);
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
        let threshold = self.ocx_ui.auto_switch_threshold.min(100);
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
        let busy = self.ocx_ui.account_busy.as_deref() == Some(busy_key.as_str());
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
}
