//! Codex, account, model, and subagent settings rendering.

use super::*;

impl DesktopApp {
    pub(super) fn render_settings(&mut self, ui: &mut egui::Ui) {
        self.poll_ocx_settings();
        egui::ScrollArea::vertical()
            .id_salt("ocx_settings")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(148.0);
                        ui.heading(RichText::new("설정").color(NOTCH_TEXT));
                        ui.add_space(10.0);
                        for page in OcxSettingsPage::ALL {
                            if ui
                                .selectable_label(
                                    self.ocx_ui.settings_page == page,
                                    RichText::new(page.label()).color(NOTCH_TEXT),
                                )
                                .clicked()
                            {
                                self.ocx_ui.settings_page = page;
                            }
                        }
                    });
                    ui.separator();
                    ui.add_space(12.0);
                    ui.vertical(|ui| match self.ocx_ui.settings_page {
                        OcxSettingsPage::CodexAuth => self.render_ocx_codex_auth_settings(ui),
                        OcxSettingsPage::Providers => self.render_ocx_provider_settings(ui),
                        OcxSettingsPage::Models => self.render_ocx_model_settings(ui),
                        OcxSettingsPage::Subagents => self.render_ocx_subagent_settings(ui),
                    });
                });
            });
    }

    fn render_settings_account(
        &mut self,
        ui: &mut egui::Ui,
        provider: &str,
        account: &ProviderAccount,
    ) {
        let busy =
            self.ocx_ui.account_busy.as_deref() == Some(&format!("{provider}:{}", account.id));
        let mut action = None;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.strong(RichText::new(&account.identity).color(NOTCH_TEXT));
                    ui.small(RichText::new(&account.kind).color(NOTCH_TEXT_MUTED));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if account.needs_reauth {
                        if ui.small_button("재인증").clicked() {
                            action = Some(AccountAction::Reauth);
                        }
                    } else if account.paused {
                        if ui.small_button("다시 사용").clicked() {
                            action = Some(AccountAction::Activate);
                        }
                    } else if !account.active && ui.small_button("선택").clicked() {
                        action = Some(AccountAction::Activate);
                    }
                    if !account.is_main && !account.paused && ui.small_button("일시 중지").clicked()
                    {
                        action = Some(AccountAction::Pause);
                    }
                    if account.active {
                        ui.small(RichText::new("현재").color(NOTCH_ACCENT));
                    }
                    if busy {
                        ui.small("저장 중…");
                    }
                });
            });
            if !account.health.is_empty() {
                ui.small(RichText::new(&account.health).color(NOTCH_TEXT_MUTED));
            }
        });
        if let Some(action) = action {
            self.run_account_action(provider, account, action);
        }
        ui.add_space(6.0);
    }

    fn render_ocx_codex_auth_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading(RichText::new("Codex 인증").color(NOTCH_TEXT));
        ui.small(RichText::new("OCX에 연결된 Codex 계정과 전환 정책").color(NOTCH_TEXT_MUTED));
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("사용량 기반 선제 전환").color(NOTCH_TEXT));
            let mut enabled = self.ocx_ui.auto_switch_threshold > 0;
            if ui.checkbox(&mut enabled, "").changed() && !enabled {
                self.update_auto_switch_threshold(0);
            }
            ui.add_enabled_ui(enabled, |ui| {
                let mut threshold = self.ocx_ui.auto_switch_threshold;
                if ui
                    .add(egui::Slider::new(&mut threshold, 1..=100).suffix("%"))
                    .changed()
                {
                    self.update_auto_switch_threshold(threshold);
                }
            });
        });
        ui.add_space(12.0);
        let accounts = self
            .ocx_ui
            .pools
            .iter()
            .find(|pool| pool.provider == "openai")
            .map(|pool| pool.accounts.clone())
            .unwrap_or_default();
        if accounts.is_empty() {
            ui.small(RichText::new("OCX Codex 계정을 불러오는 중입니다.").color(NOTCH_TEXT_MUTED));
        } else {
            ui.label(RichText::new("계정 풀").color(NOTCH_TEXT).strong());
            ui.add_space(6.0);
            for account in &accounts {
                self.render_settings_account(ui, "openai", account);
            }
        }
    }

    fn render_ocx_provider_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading(RichText::new("프로바이더").color(NOTCH_TEXT));
        ui.small(RichText::new("연결된 프로바이더와 해당 계정 풀").color(NOTCH_TEXT_MUTED));
        ui.add_space(14.0);
        let pools = self.ocx_ui.pools.clone();
        if self
            .ocx_ui
            .settings_provider
            .as_ref()
            .is_none_or(|name| !pools.iter().any(|pool| &pool.provider == name))
        {
            self.ocx_ui.settings_provider = pools.first().map(|pool| pool.provider.clone());
        }
        ui.columns(2, |columns| {
            columns[0].vertical(|ui| {
                ui.label(RichText::new("프로바이더").color(NOTCH_TEXT_MUTED).small());
                for pool in &pools {
                    let selected =
                        self.ocx_ui.settings_provider.as_deref() == Some(pool.provider.as_str());
                    if ui
                        .selectable_label(
                            selected,
                            format!("{}  {}", pool.provider, pool.accounts.len()),
                        )
                        .clicked()
                    {
                        self.ocx_ui.settings_provider = Some(pool.provider.clone());
                    }
                }
            });
            columns[1].vertical(|ui| {
                if let Some(pool) = pools.iter().find(|pool| {
                    Some(pool.provider.as_str()) == self.ocx_ui.settings_provider.as_deref()
                }) {
                    ui.label(RichText::new(&pool.provider).color(NOTCH_TEXT).strong());
                    ui.small(RichText::new("계정 및 연결 상태").color(NOTCH_TEXT_MUTED));
                    ui.add_space(6.0);
                    for account in &pool.accounts {
                        self.render_settings_account(ui, &pool.provider, account);
                    }
                } else {
                    ui.small("연결된 프로바이더가 없습니다.");
                }
            });
        });
    }

    fn render_ocx_model_settings(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(RichText::new("모델").color(NOTCH_TEXT));
            ui.small(
                RichText::new(format!(
                    "{}/{} 표시",
                    self.ocx_ui
                        .models
                        .iter()
                        .filter(|model| !model.disabled)
                        .count(),
                    self.ocx_ui.models.len()
                ))
                .color(NOTCH_TEXT_MUTED),
            );
        });
        ui.small(
            RichText::new("켜진 모델만 OCX 카탈로그와 Roche 모델 선택기에 표시됩니다.")
                .color(NOTCH_TEXT_MUTED),
        );
        ui.add_space(12.0);
        let mut models = self.ocx_ui.models.clone();
        models.sort_by(|left, right| {
            left.provider
                .cmp(&right.provider)
                .then(left.namespaced.cmp(&right.namespaced))
        });
        let mut current_provider = String::new();
        for model in models {
            if current_provider != model.provider {
                current_provider = model.provider.clone();
                ui.add_space(8.0);
                ui.label(RichText::new(&current_provider).color(NOTCH_TEXT).strong());
            }
            let mut enabled = !model.disabled;
            let label = model
                .display_name
                .clone()
                .unwrap_or_else(|| model.namespaced.clone());
            if ui.checkbox(&mut enabled, label).changed() {
                self.save_model_visibility(model, enabled);
            }
        }
        if self.ocx_ui.models.is_empty() {
            ui.small(
                RichText::new("OCX 모델 카탈로그를 불러오는 중입니다.").color(NOTCH_TEXT_MUTED),
            );
        }
    }

    fn render_ocx_subagent_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading(RichText::new("서브에이전트").color(NOTCH_TEXT));
        ui.small(
            RichText::new("추천 순서는 Codex 피커와 spawn_agent 기본 후보 순서를 결정합니다.")
                .color(NOTCH_TEXT_MUTED),
        );
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            for (index, label) in [
                format!("추천 {}/5", self.ocx_ui.subagent_models.chosen.len()),
                format!("모델 {}", self.ocx_ui.subagent_models.available.len()),
                "설정".to_owned(),
            ]
            .into_iter()
            .enumerate()
            {
                if ui
                    .selectable_label(self.ocx_ui.subagent_panel == index, label)
                    .clicked()
                {
                    self.ocx_ui.subagent_panel = index;
                }
            }
        });
        ui.separator();
        let chosen = self.ocx_ui.subagent_models.chosen.clone();
        match self.ocx_ui.subagent_panel {
            0 => {
                let mut next = None;
                for (index, model) in chosen.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{}", index + 1)).color(NOTCH_TEXT_MUTED));
                        ui.monospace(model);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("제거").clicked() {
                                let mut values = chosen.clone();
                                values.remove(index);
                                next = Some(values);
                            }
                            if index + 1 < chosen.len() && ui.small_button("↓").clicked() {
                                let mut values = chosen.clone();
                                values.swap(index, index + 1);
                                next = Some(values);
                            }
                            if index > 0 && ui.small_button("↑").clicked() {
                                let mut values = chosen.clone();
                                values.swap(index, index - 1);
                                next = Some(values);
                            }
                        });
                    });
                }
                if let Some(models) = next {
                    self.save_subagent_models(models);
                }
            }
            1 => {
                let mut next = None;
                for model in self.ocx_ui.subagent_models.available.clone() {
                    let selected = chosen.contains(&model);
                    ui.horizontal(|ui| {
                        ui.monospace(&model);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let label = if selected {
                                "추천에서 제거"
                            } else {
                                "추천에 추가"
                            };
                            if ui.small_button(label).clicked() {
                                let mut values = chosen.clone();
                                if selected {
                                    values.retain(|item| item != &model);
                                } else if values.len() < 5 {
                                    values.push(model.clone());
                                }
                                next = Some(values);
                            }
                        });
                    });
                }
                if let Some(models) = next {
                    self.save_subagent_models(models);
                }
            }
            _ => {
                let mut next = self.ocx_ui.injection_settings.clone();
                ui.label(RichText::new("먼저 부를 모델").color(NOTCH_TEXT).strong());
                ui.small("Codex가 일을 나눌 때 우선 사용할 모델과 추론 강도입니다.");
                egui::ComboBox::from_id_salt("ocx_injection_model")
                    .selected_text(next.model.as_deref().unwrap_or("선택 안 함"))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut next.model, None, "선택 안 함");
                        for model in &next.available {
                            ui.selectable_value(
                                &mut next.model,
                                Some(model.namespaced.clone()),
                                &model.namespaced,
                            );
                        }
                    });
                egui::ComboBox::from_id_salt("ocx_injection_effort")
                    .selected_text(next.effort.as_deref().unwrap_or("기본값"))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut next.effort, None, "기본값");
                        for effort in &next.efforts {
                            ui.selectable_value(&mut next.effort, Some(effort.clone()), effort);
                        }
                    });
                ui.add_space(10.0);
                ui.checkbox(
                    &mut next.sync_codex_subagent_defaults,
                    "Codex 설정에도 기본값으로 저장",
                );
                ui.checkbox(
                    &mut next.multi_agent_guidance_enabled,
                    "일 나누는 방법 알려주기",
                );
                if next.model != self.ocx_ui.injection_settings.model
                    || next.effort != self.ocx_ui.injection_settings.effort
                    || next.sync_codex_subagent_defaults
                        != self.ocx_ui.injection_settings.sync_codex_subagent_defaults
                    || next.multi_agent_guidance_enabled
                        != self.ocx_ui.injection_settings.multi_agent_guidance_enabled
                {
                    self.ocx_ui.injection_settings = next.clone();
                    self.save_injection_settings(next);
                }
            }
        }
    }
}
