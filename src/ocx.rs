use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use serde::Deserialize;

const OCX_BASE: &str = "http://127.0.0.1:10100";
const POLL_INTERVAL: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSegment {
    pub label: String,
    pub percent: Option<f64>,
    pub reset_at: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaWindow {
    pub label: String,
    pub percent: Option<f64>,
    pub reset_at: Option<f64>,
    #[serde(default)]
    pub value_label: Option<String>,
    #[serde(default)]
    pub segments: Vec<QuotaSegment>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quota {
    pub five_hour_percent: Option<f64>,
    pub five_hour_reset_at: Option<f64>,
    pub weekly_percent: Option<f64>,
    pub weekly_reset_at: Option<f64>,
    pub monthly_percent: Option<f64>,
    pub monthly_reset_at: Option<f64>,
    #[serde(default)]
    pub custom_windows: Vec<QuotaWindow>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaReport {
    pub provider: String,
    pub label: Option<String>,
    #[serde(default)]
    pub quota: Quota,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaResponse {
    #[serde(default)]
    pub reports: Vec<QuotaReport>,
}

/// One displayable quota bar, mirroring the ocx-notch `QuotaBarRow` shape.
#[derive(Debug, Clone)]
pub struct QuotaBar {
    pub label: String,
    pub percent: f64,
    pub reset_at: Option<f64>,
    pub value_label: Option<String>,
}

pub fn quota_bars(quota: &Quota) -> Vec<QuotaBar> {
    let mut bars = Vec::new();
    if let Some(percent) = quota.five_hour_percent {
        bars.push(QuotaBar {
            label: "5h limit".into(),
            percent,
            reset_at: quota.five_hour_reset_at,
            value_label: None,
        });
    }
    if let Some(percent) = quota.weekly_percent {
        bars.push(QuotaBar {
            label: "Weekly limit".into(),
            percent,
            reset_at: quota.weekly_reset_at,
            value_label: None,
        });
    }
    if let Some(percent) = quota.monthly_percent {
        bars.push(QuotaBar {
            label: "Monthly limit".into(),
            percent,
            reset_at: quota.monthly_reset_at,
            value_label: None,
        });
    }
    for window in &quota.custom_windows {
        if let Some(percent) = window.percent {
            bars.push(QuotaBar {
                label: window.label.clone(),
                percent,
                reset_at: window.reset_at,
                value_label: window.value_label.clone(),
            });
        }
    }
    bars
}

#[derive(Debug, Clone)]
pub enum OcxEvent {
    Updated { reports: Vec<QuotaReport> },
    Error(String),
}

#[derive(Debug)]
enum OcxCommand {
    Refresh,
    Shutdown,
}

pub struct OcxQuotaController {
    commands: Sender<OcxCommand>,
    events: Receiver<OcxEvent>,
}

impl OcxQuotaController {
    pub fn spawn() -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        thread::Builder::new()
            .name("roche-ocx-quota".to_owned())
            .spawn(move || ocx_worker(command_rx, event_tx))
            .expect("failed to start OCX quota worker");
        Self {
            commands: command_tx,
            events: event_rx,
        }
    }

    pub fn refresh(&self) {
        let _ = self.commands.send(OcxCommand::Refresh);
    }

    pub fn drain(&self) -> Vec<OcxEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            events.push(event);
        }
        events
    }
}

impl Drop for OcxQuotaController {
    fn drop(&mut self) {
        let _ = self.commands.send(OcxCommand::Shutdown);
    }
}

fn ocx_worker(commands: Receiver<OcxCommand>, events: Sender<OcxEvent>) {
    loop {
        match fetch_provider_quotas() {
            Ok(reports) => {
                let _ = events.send(OcxEvent::Updated { reports });
            }
            Err(error) => {
                let _ = events.send(OcxEvent::Error(error));
            }
        }
        match commands.recv_timeout(POLL_INTERVAL) {
            Ok(OcxCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(OcxCommand::Refresh) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn management_token() -> Option<String> {
    for name in ["OPENCODEX_ADMIN_AUTH_TOKEN", "OPENCODEX_API_AUTH_TOKEN"] {
        if let Ok(token) = std::env::var(name) {
            let token = token.trim();
            if !token.is_empty() && !token.contains(['\r', '\n']) {
                return Some(token.to_owned());
            }
        }
    }
    let config_dir = std::env::var_os("OPENCODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".opencodex"))
        })?;
    let token = std::fs::read_to_string(config_dir.join("admin-api-token")).ok()?;
    let token = token.trim();
    token.starts_with("ocx_admin_").then(|| token.to_owned())
}

pub fn fetch_provider_quotas() -> Result<Vec<QuotaReport>, String> {
    let url = format!("{OCX_BASE}/api/provider-quotas");
    let mut request = ureq::get(&url).timeout(REQUEST_TIMEOUT);
    if let Some(token) = management_token() {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }
    let response = request
        .call()
        .map_err(|error| format!("OCX unavailable: {error}"))?;
    let body = response
        .into_string()
        .map_err(|error| format!("Could not read OCX response: {error}"))?;
    let parsed: QuotaResponse = serde_json::from_str(&body)
        .map_err(|error| format!("Invalid OCX quota response: {error}"))?;
    Ok(parsed.reports)
}

/// Sampled native memory for a live process, mirroring ocx-notch.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessMemory {
    pub working_set: u64,
    pub private_commit: u64,
    pub peak_working_set: u64,
}

#[cfg(windows)]
pub fn sample_process(pid: u32) -> Option<ProcessMemory> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
        },
        System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ},
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut counters: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let ok = K32GetProcessMemoryInfo(
            handle,
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        );
        let _ = CloseHandle(handle);
        if ok == 0 {
            return None;
        }
        Some(ProcessMemory {
            working_set: counters.WorkingSetSize as u64,
            private_commit: counters.PrivateUsage as u64,
            peak_working_set: counters.PeakWorkingSetSize as u64,
        })
    }
}

#[cfg(not(windows))]
pub fn sample_process(_pid: u32) -> Option<ProcessMemory> {
    None
}

pub fn sample_current_process() -> Option<ProcessMemory> {
    sample_process(std::process::id())
}

/// Free commit headroom (bytes) captured from the system commit limit, used to
/// derive the notch-style `Max` value for a private-commit gauge.
#[cfg(windows)]
pub fn commit_headroom() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
    unsafe {
        let mut perf: PERFORMANCE_INFORMATION = std::mem::zeroed();
        perf.cb = std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32;
        if GetPerformanceInfo(&mut perf, perf.cb) == 0 {
            return None;
        }
        let page = perf.PageSize as u64;
        if page == 0 {
            return None;
        }
        let headroom = perf.CommitLimit.saturating_sub(perf.CommitTotal) as u64;
        Some(headroom.saturating_mul(page))
    }
}

#[cfg(not(windows))]
pub fn commit_headroom() -> Option<u64> {
    None
}

#[derive(Debug, Deserialize)]
struct Health {
    pid: u32,
}

/// Ask the running OCX service for its own process id, if it is reachable.
pub fn ocx_health_pid() -> Option<u32> {
    let url = format!("{OCX_BASE}/healthz");
    let response = ureq::get(&url).timeout(REQUEST_TIMEOUT).call().ok()?;
    let body = response.into_string().ok()?;
    serde_json::from_str::<Health>(&body)
        .ok()
        .map(|health| health.pid)
}

/// Launch an OCX lifecycle command as a detached background command.
pub fn run_ocx(action: &str) -> Result<(), String> {
    let action = match action {
        "ensure" | "start" | "stop" | "restart" => action,
        _ => return Err("Invalid OCX action".into()),
    };
    let command = format!("ocx {action}");
    std::process::Command::new("cmd.exe")
        .args(["/D", "/S", "/C", command.as_str()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|_| format!("Could not launch ocx {action}"))
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.0} MB", value / MB)
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{value} B")
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountQuota {
    pub weekly_percent: Option<f64>,
    pub weekly_reset_at: Option<f64>,
    pub monthly_percent: Option<f64>,
    pub monthly_reset_at: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAccount {
    pub id: String,
    pub alias: Option<String>,
    pub email: Option<String>,
    #[serde(default)]
    pub is_main: bool,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub needs_reauth: bool,
    #[serde(default)]
    pub reset_credits: Option<u32>,
    pub quota: Option<AccountQuota>,
    pub health_label: Option<String>,
    pub health_summary: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CodexAccountsResponse {
    #[serde(default)]
    pub accounts: Vec<CodexAccount>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    pub name: String,
    pub auth_mode: Option<String>,
    #[serde(default)]
    pub disabled: bool,
}

/// A model row exposed by OCX's management Models page.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcxModel {
    pub provider: String,
    pub id: String,
    pub namespaced: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub native: bool,
    pub display_name: Option<String>,
    pub context_window: Option<u64>,
}

/// The live recommendation list and complete pickable list from OCX.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OcxSubagentModels {
    #[serde(default)]
    pub chosen: Vec<String>,
    #[serde(default)]
    pub available: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcxInjectionSettings {
    #[serde(default)]
    pub multi_agent_guidance_enabled: bool,
    #[serde(default)]
    pub sync_codex_subagent_defaults: bool,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub prompt: Option<String>,
    #[serde(default)]
    pub efforts: Vec<String>,
    #[serde(default)]
    pub available: Vec<OcxInjectionModel>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OcxInjectionModel {
    pub namespaced: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexActiveState {
    pub active_codex_account_id: Option<String>,
    #[serde(default = "default_auto_switch_threshold")]
    pub auto_switch_threshold: u32,
}

impl Default for CodexActiveState {
    fn default() -> Self {
        Self {
            active_codex_account_id: None,
            auto_switch_threshold: default_auto_switch_threshold(),
        }
    }
}

fn default_auto_switch_threshold() -> u32 {
    80
}

/// One account shown in a provider's pool row (unified across Codex, OAuth,
/// and per-provider API-key pools).
#[derive(Debug, Clone, Default)]
pub struct ProviderAccount {
    pub id: String,
    pub identity: String,
    pub kind: String,
    pub active: bool,
    pub paused: bool,
    pub is_main: bool,
    pub needs_reauth: bool,
    pub health: String,
    pub reset_credits: Option<u32>,
    pub weekly_percent: Option<f64>,
    pub weekly_reset_at: Option<f64>,
    pub monthly_percent: Option<f64>,
    pub monthly_reset_at: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct ProviderPool {
    pub provider: String,
    pub accounts: Vec<ProviderAccount>,
}

fn ocx_get_json<T: for<'de> Deserialize<'de>>(path: &str) -> Result<T, String> {
    let url = format!("{OCX_BASE}{path}");
    let mut request = ureq::get(&url).timeout(REQUEST_TIMEOUT);
    if let Some(token) = management_token() {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }
    let body = request
        .call()
        .map_err(|error| format!("OCX GET {path} failed: {error}"))?
        .into_string()
        .map_err(|error| format!("Could not read OCX {path}: {error}"))?;
    serde_json::from_str(&body).map_err(|error| format!("Invalid OCX {path} response: {error}"))
}

fn encode_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn ocx_request(method: &str, path: &str, body: Option<&str>) -> Result<(), String> {
    let url = format!("{OCX_BASE}{path}");
    let mut builder = match method {
        "PUT" => ureq::put(&url),
        "POST" => ureq::post(&url),
        _ => return Err("Invalid OCX method".into()),
    }
    .timeout(REQUEST_TIMEOUT);
    if let Some(token) = management_token() {
        builder = builder.set("Authorization", &format!("Bearer {token}"));
    }
    let result = match body {
        Some(payload) => builder.send_string(payload),
        None => builder.call(),
    };
    result
        .map(|_| ())
        .map_err(|error| format!("OCX {method} {path} failed: {error}"))
}

pub fn fetch_codex_accounts() -> Result<Vec<CodexAccount>, String> {
    ocx_get_json::<CodexAccountsResponse>("/api/codex-auth/accounts")
        .map(|response| response.accounts)
}

pub fn fetch_provider_configs() -> Result<Vec<ProviderConfig>, String> {
    ocx_get_json("/api/providers")
}

pub fn fetch_ocx_models() -> Result<Vec<OcxModel>, String> {
    ocx_get_json("/api/models")
}

pub fn fetch_subagent_models() -> Result<OcxSubagentModels, String> {
    ocx_get_json("/api/subagent-models")
}

pub fn fetch_injection_settings() -> Result<OcxInjectionSettings, String> {
    ocx_get_json("/api/injection-model")
}

pub fn fetch_codex_active_state() -> Result<CodexActiveState, String> {
    ocx_get_json("/api/codex-auth/active")
}

pub fn fetch_codex_active_account() -> Option<String> {
    fetch_codex_active_state()
        .ok()
        .and_then(|state| state.active_codex_account_id)
}

fn parse_provider_pool_value(
    provider: String,
    kind: &str,
    value: &serde_json::Value,
) -> ProviderPool {
    let active_id = value
        .get("activeAccountId")
        .or_else(|| value.get("activeId"))
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let accounts = value
        .get("accounts")
        .or_else(|| value.get("keys"))
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_string();
            let identity = entry
                .get("label")
                .or_else(|| entry.get("masked"))
                .or_else(|| entry.get("email"))
                .and_then(|value| value.as_str())
                .unwrap_or(&id)
                .to_string();
            let active = entry
                .get("active")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
                || id == active_id;
            let paused = entry
                .get("paused")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let needs_reauth = entry
                .get("needsReauth")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let health = entry
                .get("healthLabel")
                .or_else(|| entry.get("healthSummary"))
                .and_then(|value| value.as_str())
                .unwrap_or(if active { "Active" } else { "Available" })
                .to_string();
            let quota = entry
                .get("quota")
                .and_then(|value| serde_json::from_value::<Quota>(value.clone()).ok());
            Some(ProviderAccount {
                id,
                identity,
                kind: kind.to_owned(),
                active,
                paused,
                is_main: false,
                needs_reauth,
                health,
                reset_credits: None,
                weekly_percent: quota.as_ref().and_then(|quota| quota.weekly_percent),
                weekly_reset_at: quota.as_ref().and_then(|quota| quota.weekly_reset_at),
                monthly_percent: quota.as_ref().and_then(|quota| quota.monthly_percent),
                monthly_reset_at: quota.as_ref().and_then(|quota| quota.monthly_reset_at),
            })
        })
        .collect();
    ProviderPool { provider, accounts }
}

/// Fetch a provider account pool using OCX's configured auth mode. The caller
/// supplies the Codex active id so OpenAI's selected account matches the same
/// `/api/codex-auth/active` state used by ocx-notch.
pub fn fetch_provider_pool(config: &ProviderConfig, codex_active_id: Option<&str>) -> ProviderPool {
    let provider = config.name.clone();
    if provider == "openai" {
        let accounts = fetch_codex_accounts()
            .unwrap_or_default()
            .into_iter()
            .map(|account| {
                let is_main = account.is_main || account.id == "__main__";
                let identity = account
                    .alias
                    .clone()
                    .or_else(|| account.email.clone())
                    .unwrap_or_else(|| account.id.clone());
                let health = if account.needs_reauth {
                    "Reauth required".to_string()
                } else {
                    account
                        .health_label
                        .or(account.health_summary)
                        .unwrap_or_else(|| "Available".to_string())
                };
                let active = codex_active_id
                    .map(|active_id| account.id == active_id)
                    .unwrap_or(is_main);
                ProviderAccount {
                    id: account.id.clone(),
                    identity,
                    kind: "codex".into(),
                    active,
                    paused: account.paused,
                    is_main,
                    needs_reauth: account.needs_reauth,
                    health,
                    reset_credits: account.reset_credits,
                    weekly_percent: account
                        .quota
                        .as_ref()
                        .and_then(|quota| quota.weekly_percent),
                    weekly_reset_at: account
                        .quota
                        .as_ref()
                        .and_then(|quota| quota.weekly_reset_at),
                    monthly_percent: account
                        .quota
                        .as_ref()
                        .and_then(|quota| quota.monthly_percent),
                    monthly_reset_at: account
                        .quota
                        .as_ref()
                        .and_then(|quota| quota.monthly_reset_at),
                }
            })
            .collect();
        return ProviderPool { provider, accounts };
    }

    let mode = config.auth_mode.as_deref().unwrap_or_default();
    let (kind, path) = match mode {
        "oauth" => (
            "oauth",
            format!(
                "/api/oauth/accounts?provider={}",
                encode_component(&config.name)
            ),
        ),
        "key" | "" => (
            "key",
            format!(
                "/api/providers/keys?name={}",
                encode_component(&config.name)
            ),
        ),
        _ => {
            return ProviderPool {
                provider,
                accounts: Vec::new(),
            };
        }
    };
    let value = ocx_get_json::<serde_json::Value>(&path).unwrap_or_default();
    parse_provider_pool_value(provider, kind, &value)
}

pub fn set_active_account(provider: &str, kind: &str, account_id: &str) -> Result<(), String> {
    let (path, body) = match kind {
        "codex" => (
            "/api/codex-auth/active",
            serde_json::json!({ "accountId": account_id }),
        ),
        "oauth" => (
            "/api/oauth/accounts/active",
            serde_json::json!({ "provider": provider, "accountId": account_id }),
        ),
        "key" => (
            "/api/providers/keys/active",
            serde_json::json!({ "name": provider, "id": account_id }),
        ),
        _ => return Err("Unknown account kind".into()),
    };
    ocx_request("PUT", path, Some(&body.to_string()))
}

pub fn set_account_paused(
    provider: &str,
    kind: &str,
    account_id: &str,
    paused: bool,
) -> Result<(), String> {
    let (path, body) = match kind {
        "codex" => (
            "/api/codex-auth/accounts/pause",
            serde_json::json!({ "id": account_id, "paused": paused }),
        ),
        "oauth" => (
            "/api/oauth/accounts/pause",
            serde_json::json!({
                "provider": provider,
                "accountId": account_id,
                "paused": paused,
            }),
        ),
        _ => return Err("This account type cannot be paused".into()),
    };
    ocx_request("PUT", path, Some(&body.to_string()))
}

pub fn set_auto_switch_threshold(threshold: u32) -> Result<(), String> {
    let threshold = threshold.min(100);
    let body = serde_json::json!({ "threshold": threshold }).to_string();
    ocx_request("PUT", "/api/codex-auth/auto-switch", Some(&body))
}

pub fn set_model_visibility(model: &OcxModel, enabled: bool) -> Result<(), String> {
    let body = serde_json::json!({
        "scope": "models",
        "provider": model.provider,
        "enabled": enabled,
        "targets": [{ "id": model.id, "native": model.native }],
    })
    .to_string();
    ocx_request("PUT", "/api/model-visibility", Some(&body))
}

pub fn set_subagent_models(models: &[String]) -> Result<(), String> {
    let body = serde_json::json!({ "models": models }).to_string();
    ocx_request("PUT", "/api/subagent-models", Some(&body))
}

pub fn set_injection_settings(settings: &OcxInjectionSettings) -> Result<(), String> {
    let body = serde_json::json!({
        "multiAgentGuidanceEnabled": settings.multi_agent_guidance_enabled,
        "syncCodexSubagentDefaults": settings.sync_codex_subagent_defaults,
        "model": settings.model,
        "effort": settings.effort,
        "prompt": settings.prompt,
    })
    .to_string();
    ocx_request("PUT", "/api/injection-model", Some(&body))
}

pub fn consume_reset_credit(account_id: &str) -> Result<(), String> {
    let body = serde_json::json!({ "accountId": account_id }).to_string();
    ocx_request("POST", "/api/codex-auth/reset-credits/consume", Some(&body))
}

pub fn request_account_reauth(provider: &str, kind: &str, account_id: &str) -> Result<(), String> {
    let (path, body) = match kind {
        "codex" => (
            "/api/codex-auth/login",
            serde_json::json!({ "id": account_id, "reauth": true }),
        ),
        "oauth" => (
            "/api/oauth/login",
            serde_json::json!({
                "provider": provider,
                "accountId": account_id,
                "reauth": true,
            }),
        ),
        _ => return Err("This account type cannot be reauthenticated".into()),
    };
    ocx_request("POST", path, Some(&body.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_response_parses_notch_shape() {
        let response: QuotaResponse = serde_json::from_str(
            r#"{
                "reports": [
                    {
                        "provider": "kiro",
                        "label": "Kiro",
                        "quota": {
                            "weeklyPercent": 42.0,
                            "weeklyResetAt": 1785600000000.0,
                            "customWindows": [
                                {
                                    "label": "Pro 5h",
                                    "percent": 73.5,
                                    "segments": [
                                        {"label": "claude", "percent": 31.0}
                                    ]
                                }
                            ]
                        }
                    }
                ]
            }"#,
        )
        .expect("notch quota shape parses");
        assert_eq!(response.reports.len(), 1);
        assert_eq!(response.reports[0].provider, "kiro");
        let bars = quota_bars(&response.reports[0].quota);
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].label, "Weekly limit");
        assert_eq!(bars[0].percent, 42.0);
        assert_eq!(bars[1].label, "Pro 5h");
        assert_eq!(bars[1].percent, 73.5);
    }

    #[test]
    fn missing_quota_fields_yield_no_bars() {
        let quota = Quota::default();
        assert!(quota_bars(&quota).is_empty());
    }

    #[test]
    fn active_state_carries_rotation_threshold() {
        let state: CodexActiveState =
            serde_json::from_str(r#"{"activeCodexAccountId":"pool-b","autoSwitchThreshold":67}"#)
                .expect("active state parses");
        assert_eq!(state.active_codex_account_id.as_deref(), Some("pool-b"));
        assert_eq!(state.auto_switch_threshold, 67);

        let defaulted: CodexActiveState = serde_json::from_str(r#"{}"#).expect("defaults parse");
        assert_eq!(defaulted.auto_switch_threshold, 80);
    }

    #[test]
    fn oauth_pool_preserves_active_pause_and_reauth_state() {
        let value = serde_json::json!({
            "activeAccountId": "account-b",
            "accounts": [
                {"id": "account-a", "email": "a***@example.com"},
                {
                    "id": "account-b",
                    "email": "b***@example.com",
                    "paused": true,
                    "needsReauth": true
                }
            ]
        });
        let pool = parse_provider_pool_value("kiro".into(), "oauth", &value);
        assert_eq!(pool.accounts.len(), 2);
        assert!(!pool.accounts[0].active);
        assert!(pool.accounts[1].active);
        assert!(pool.accounts[1].paused);
        assert!(pool.accounts[1].needs_reauth);
        assert_eq!(pool.accounts[1].kind, "oauth");
    }
}
