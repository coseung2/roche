//! WebView JavaScript submission, cancellation, login, and progress probes.

use std::sync::mpsc::Sender;

use super::{WebGptBrowserEvent, WebGptBrowserState, WebGptTurnCorrelation};

pub(super) fn probe_login_state(webview: &wry::WebView, events: Sender<WebGptBrowserEvent>) {
    const SCRIPT: &str = r#"(() => {
        const composer = document.querySelector('#prompt-textarea')
            || document.querySelector('textarea[placeholder]')
            || document.querySelector('[contenteditable="true"][role="textbox"]');
        const loginControl = document.querySelector(
            'a[href*="/auth/login"], a[href*="/auth/"], button[data-testid="login-button"]'
        );
        return Boolean(composer) && !loginControl;
    })()"#;
    let callback_events = events.clone();
    if let Err(error) = webview.evaluate_script_with_callback(SCRIPT, move |value| {
        let logged_in = value.trim().eq_ignore_ascii_case("true");
        let state = if logged_in {
            WebGptBrowserState::LoggedIn
        } else {
            WebGptBrowserState::LoginRequired
        };
        let _ = callback_events.send(WebGptBrowserEvent::State(state));
    }) {
        let _ = events.send(WebGptBrowserEvent::Error(format!(
            "Could not inspect ChatGPT login state: {error}"
        )));
    }
}

pub(super) fn submit_wake_prompt(
    webview: &wry::WebView,
    request_id: String,
    events: Sender<WebGptBrowserEvent>,
) {
    let wake_text = format!(
        "Use the Roche app for native request {request_id}. Read that pending request through Roche MCP, orchestrate any Web GPT or Codex worker sessions you need, and post the final user-facing answer back through Roche. Rust is the deterministic session/task source of truth."
    );
    submit_prompt(
        webview,
        None,
        request_id,
        wake_text,
        Vec::new(),
        events,
        false,
    );
}

pub(super) fn submit_chat_prompt(
    webview: &wry::WebView,
    correlation: WebGptTurnCorrelation,
    text: String,
    attachments: Vec<super::BrowserAttachment>,
    events: Sender<WebGptBrowserEvent>,
) {
    let request_id = correlation.request_id.clone();
    submit_prompt(
        webview,
        Some(correlation),
        request_id,
        text,
        attachments,
        events,
        true,
    );
}

pub(super) fn cancel_chat_prompt(
    webview: &wry::WebView,
    correlation: WebGptTurnCorrelation,
    events: Sender<WebGptBrowserEvent>,
) {
    let request_id = correlation.request_id.clone();
    let encoded_request_id =
        serde_json::to_string(&request_id).unwrap_or_else(|_| "\"roche-web\"".into());
    let encoded_correlation =
        serde_json::to_string(&correlation).unwrap_or_else(|_| "null".to_owned());
    let script = format!(
        r#"(() => {{
            const requestId = {encoded_request_id};
            const correlation = {encoded_correlation};
            let pending = null;
            try {{ pending = JSON.parse(sessionStorage.getItem('__rochePendingChat') || 'null'); }} catch {{}}
            if (!pending || pending.requestId !== requestId) return false;
            if (JSON.stringify(pending.correlation) !== JSON.stringify(correlation)) return false;
            pending.cancelRequested = true;
            sessionStorage.setItem('__rochePendingChat', JSON.stringify(pending));
            const stop = document.querySelector('[data-testid="stop-button"]')
                || document.querySelector('button[aria-label*="Stop"]')
                || document.querySelector('button[aria-label*="중지"]');
            if (!stop) return 'pending';
            stop.click();
            sessionStorage.removeItem('__rochePendingChat');
            return 'cancelled';
        }})()"#
    );
    let callback_events = events.clone();
    let callback_correlation = correlation.clone();
    if let Err(error) = webview.evaluate_script_with_callback(&script, move |value| {
        if let Some(event) = super::cancel_script_event(&value, &callback_correlation) {
            let _ = callback_events.send(event);
        }
    }) {
        let _ = events.send(WebGptBrowserEvent::Error(format!(
            "Could not cancel Web GPT request {request_id}: {error}"
        )));
    }
}

pub(super) fn submit_prompt(
    webview: &wry::WebView,
    correlation: Option<WebGptTurnCorrelation>,
    request_id: String,
    text: String,
    attachments: Vec<super::BrowserAttachment>,
    events: Sender<WebGptBrowserEvent>,
    capture_answer: bool,
) {
    let encoded_text = serde_json::to_string(&text).unwrap_or_else(|_| "\"Roche request\"".into());
    let encoded_attachments = serde_json::to_string(&attachments).unwrap_or_else(|_| "[]".into());
    let encoded_request_id =
        serde_json::to_string(&request_id).unwrap_or_else(|_| "\"roche-web\"".into());
    let encoded_correlation = correlation
        .as_ref()
        .map(|corr| serde_json::to_string(corr).unwrap_or_else(|_| "null".to_owned()))
        .unwrap_or_else(|| "null".to_owned());
    let capture = if capture_answer { "true" } else { "false" };
    let script = format!(
        r#"(() => {{
            const rawText = {encoded_text};
            const attachments = {encoded_attachments};
            const text = rawText || (attachments.length ? '첨부 파일을 확인해 주세요.' : rawText);
            const requestId = {encoded_request_id};
            const correlation = {encoded_correlation};
            const composer = document.querySelector('#prompt-textarea')
                || document.querySelector('textarea[placeholder]')
                || document.querySelector('[contenteditable="true"][role="textbox"]');
            if (!composer) return 'login_required';
            if (attachments.length) {{
                const fileInput = document.querySelector('input[type="file"]');
                if (!fileInput) return 'attachment_input_unavailable';
                const transfer = new DataTransfer();
                for (const attachment of attachments) {{
                    const binary = atob(attachment.data_base64);
                    const bytes = new Uint8Array(binary.length);
                    for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
                    transfer.items.add(new File([bytes], attachment.name, {{ type: attachment.mime }}));
                }}
                fileInput.files = transfer.files;
                fileInput.dispatchEvent(new Event('input', {{ bubbles: true }}));
                fileInput.dispatchEvent(new Event('change', {{ bubbles: true }}));
            }}
            if ({capture}) {{
                sessionStorage.setItem('__rochePendingChat', JSON.stringify({{
                    requestId,
                    correlation,
                    text,
                    clicked: false,
                    failed: false,
                    submittedEmitted: false,
                    cancelRequested: false,
                    attempts: 0,
                    lastText: '',
                    lastActivity: '',
                    lastThinking: false
                }}));
            }}
            composer.focus();
            if (composer instanceof HTMLTextAreaElement || composer instanceof HTMLInputElement) {{
                const proto = composer instanceof HTMLTextAreaElement
                    ? HTMLTextAreaElement.prototype
                    : HTMLInputElement.prototype;
                const setter = Object.getOwnPropertyDescriptor(proto, 'value')?.set;
                if (setter) setter.call(composer, text); else composer.value = text;
                composer.dispatchEvent(new Event('input', {{ bubbles: true }}));
                composer.dispatchEvent(new Event('change', {{ bubbles: true }}));
            }} else {{
                const selection = window.getSelection();
                const range = document.createRange();
                range.selectNodeContents(composer);
                selection?.removeAllRanges();
                selection?.addRange(range);
                document.execCommand('insertText', false, text);
                composer.dispatchEvent(new InputEvent('input', {{
                    bubbles: true,
                    inputType: 'insertText',
                    data: text
                }}));
            }}
            const findSend = () => composer.closest('form')?.querySelector('button[type="submit"]')
                || document.querySelector('button[data-testid="send-button"]')
                || document.querySelector('button[data-testid*="send"]')
                || document.querySelector('button[aria-label*="Send"]')
                || document.querySelector('button[aria-label*="send"]')
                || document.querySelector('button[aria-label*="전송"]');
            let attempts = 0;
            const clickWhenReady = () => {{
                const button = findSend();
                if (button && !button.disabled) {{
                    button.click();
                    if ({capture}) {{
                        const raw = sessionStorage.getItem('__rochePendingChat');
                        if (raw) {{
                            const pending = JSON.parse(raw);
                            if (pending.requestId === requestId) {{
                                pending.clicked = true;
                                pending.attempts = attempts;
                                sessionStorage.setItem('__rochePendingChat', JSON.stringify(pending));
                            }}
                        }}
                    }}
                    return;
                }}
                attempts += 1;
                if ({capture}) {{
                    const raw = sessionStorage.getItem('__rochePendingChat');
                    if (raw) {{
                        const pending = JSON.parse(raw);
                        if (pending.requestId === requestId) {{
                            pending.attempts = attempts;
                            if (attempts >= 30) pending.failed = true;
                            sessionStorage.setItem('__rochePendingChat', JSON.stringify(pending));
                        }}
                    }}
                }}
                if (attempts < 30) setTimeout(clickWhenReady, 100);
            }};
            setTimeout(clickWhenReady, 75);
            return 'scheduled';
        }})()"#
    );
    let callback_events = events.clone();
    let callback_correlation = correlation.clone();
    let callback_request_id = request_id.clone();
    if let Err(error) = webview.evaluate_script_with_callback(&script, move |value| {
        let result = value.trim().trim_matches('"');
        match result {
            "scheduled" | "submitted" if capture_answer => {}
            "scheduled" | "submitted" => {
                let _ = callback_events.send(WebGptBrowserEvent::WakeSubmitted {
                    request_id: callback_request_id.clone(),
                });
            }
            "attachment_input_unavailable" => {
                if let Some(correlation) = &callback_correlation {
                    let _ = callback_events.send(WebGptBrowserEvent::ChatFailed {
                        correlation: correlation.clone(),
                        message: "ChatGPT file input was not available for attachment upload"
                            .to_owned(),
                    });
                }
                let _ = callback_correlation.clone();
            }
            "login_required" => {
                let _ = callback_events
                    .send(WebGptBrowserEvent::State(WebGptBrowserState::LoginRequired));
            }
            other => {
                if capture_answer {
                    if let Some(correlation) = &callback_correlation {
                        let _ = callback_events.send(WebGptBrowserEvent::ChatFailed {
                            correlation: correlation.clone(),
                            message: format!("ChatGPT request was not submitted: {other}"),
                        });
                    }
                } else {
                    let _ = callback_events.send(WebGptBrowserEvent::Error(format!(
                        "Web GPT wake request {callback_request_id} was not submitted: {other}"
                    )));
                }
            }
        }
    }) {
        if capture_answer {
            if let Some(correlation) = &correlation {
                let _ = events.send(WebGptBrowserEvent::ChatFailed {
                    correlation: correlation.clone(),
                    message: format!("Could not submit ChatGPT request: {error}"),
                });
            }
        } else {
            let _ = events.send(WebGptBrowserEvent::Error(format!(
                "Could not submit Web GPT wake request {request_id}: {error}"
            )));
        }
    }
}
pub(super) fn probe_chat_state(webview: &wry::WebView, events: Sender<WebGptBrowserEvent>) {
    const SCRIPT: &str = r#"(() => {
        const raw = sessionStorage.getItem('__rochePendingChat');
        if (!raw) return null;
        const pending = JSON.parse(raw);
        if (pending.failed) {
            const result = JSON.stringify({
                kind: 'error',
                request_id: pending.requestId,
                correlation: pending.correlation,
                detail: `send button unavailable after ${pending.attempts} attempts`
            });
            sessionStorage.removeItem('__rochePendingChat');
            return result;
        }

        const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
        const activityLine = /^\s*(inProgress|completed|failed|warnings?)\s*:/i;
        const runtimeNoiseLine = /^\s*Codex:\s+.*(?:ERROR|WARN|failed to connect|websocket)/i;
        const sanitizeAssistantText = value => (value || '')
            .split(/\r?\n/)
            .filter(line => !activityLine.test(line) && !runtimeNoiseLine.test(line))
            .join('\n')
            .trim();
        const activitySelector = [
            '[data-testid*="tool"]',
            '[data-testid*="search"]',
            '[data-testid*="connector"]',
            '[data-testid*="browse"]',
            '[data-testid*="reasoning"]',
            '[role="status"]'
        ].join(', ');
        const assistantTextWithoutActivity = node => {
            if (!node) return '';
            const clone = node.cloneNode(true);
            clone.querySelectorAll?.(activitySelector).forEach(activity => activity.remove());
            return sanitizeAssistantText(clone.innerText || clone.textContent || node.innerText || node.textContent || '');
        };
        const expected = normalize(pending.text);
        const messages = Array.from(document.querySelectorAll('[data-message-author-role]'));
        let userIndex = -1;
        for (let index = messages.length - 1; index >= 0; index -= 1) {
            const node = messages[index];
            if (node.getAttribute('data-message-author-role') !== 'user') continue;
            const observed = normalize(node.innerText || node.textContent);
            if (observed === expected || observed.includes(expected)) {
                userIndex = index;
                break;
            }
        }

        const mainText = normalize(document.querySelector('main')?.innerText || '');
        const promptIndex = mainText.lastIndexOf(expected);
        if (userIndex < 0 && promptIndex < 0) {
            return JSON.stringify({
                kind: 'probe',
                request_id: pending.requestId,
                correlation: pending.correlation,
                detail: JSON.stringify({
                    href: location.href,
                    clicked: pending.clicked,
                    failed: pending.failed,
                    attempts: pending.attempts,
                    messageCount: messages.length,
                    articleCount: document.querySelectorAll('article').length,
                    mainText: mainText.slice(-1200),
                    bodyTail: normalize(document.body?.innerText || '').slice(-1200),
                    iframeCount: document.querySelectorAll('iframe').length,
                    composerText: normalize((document.querySelector('#prompt-textarea')?.innerText || document.querySelector('#prompt-textarea')?.textContent || document.querySelector('#prompt-textarea')?.value || ''))
                })
            });
        }

        if (!pending.submittedEmitted) {
            pending.submittedEmitted = true;
            sessionStorage.setItem('__rochePendingChat', JSON.stringify(pending));
            return JSON.stringify({
                kind: 'submitted',
                request_id: pending.requestId,
                correlation: pending.correlation
            });
        }

        const generating = document.querySelector('button[data-testid="stop-button"]')
            || document.querySelector('button[aria-label*="Stop"]')
            || document.querySelector('button[aria-label*="stop"]')
            || document.querySelector('button[aria-label*="중지"]');

        if (pending.cancelRequested && generating) {
            generating.click();
            const result = JSON.stringify({
                kind: 'cancelled',
                request_id: pending.requestId,
                correlation: pending.correlation
            });
            sessionStorage.removeItem('__rochePendingChat');
            return result;
        }

        let text = '';
        let assistantRawText = '';
        if (userIndex >= 0) {
            let assistant = null;
            for (let index = userIndex + 1; index < messages.length; index += 1) {
                if (messages[index].getAttribute('data-message-author-role') === 'assistant') {
                    assistant = messages[index];
                }
            }
            assistantRawText = (assistant?.innerText || assistant?.textContent || '').trim();
            text = assistantTextWithoutActivity(assistant);
        }

        if (!text && promptIndex >= 0) {
            const afterPrompt = mainText.slice(promptIndex + expected.length);
            const answerMarkers = ['ChatGPT의 말:', 'ChatGPT said:', 'Assistant:', 'ChatGPT:'];
            let answerStart = -1;
            let markerLength = 0;
            for (const marker of answerMarkers) {
                const index = afterPrompt.indexOf(marker);
                if (index >= 0 && (answerStart < 0 || index < answerStart)) {
                    answerStart = index;
                    markerLength = marker.length;
                }
            }
            if (answerStart >= 0) {
                let answer = afterPrompt.slice(answerStart + markerLength).trim();
                const endMarkers = [
                    'ChatGPT는 AI라 실수할 수 있습니다.',
                    'ChatGPT can make mistakes.',
                    'OpenAI OpCo, LLC'
                ];
                let answerEnd = answer.length;
                for (const marker of endMarkers) {
                    const index = answer.indexOf(marker);
                    if (index >= 0) answerEnd = Math.min(answerEnd, index);
                }
                text = sanitizeAssistantText(answer.slice(0, answerEnd));
            }
        }

        const visibleText = node => {
            if (!node) return '';
            const rect = node.getBoundingClientRect?.();
            if (rect && (rect.width <= 0 || rect.height <= 0)) return '';
            return normalize(node.innerText || node.textContent || node.getAttribute?.('aria-label') || '');
        };
        const activityNodes = Array.from(document.querySelectorAll('main ' + activitySelector.replaceAll(', ', ', main ')));
        const inlineActivity = assistantRawText
            .split(/\r?\n/)
            .map(line => line.trim())
            .filter(line => activityLine.test(line))
            .slice(-1)[0] || '';
        const activity = activityNodes
            .map(visibleText)
            .filter(value => value && value !== text && value !== expected && !value.includes(expected))
            .filter(value => value.length <= 280)
            .slice(-1)[0] || inlineActivity;

        if (generating) {
            const thinking = !text;
            const changed = text !== (pending.lastText || '')
                || activity !== (pending.lastActivity || '')
                || thinking !== Boolean(pending.lastThinking);
            if (!changed) return null;
            pending.lastText = text;
            pending.lastActivity = activity;
            pending.lastThinking = thinking;
            sessionStorage.setItem('__rochePendingChat', JSON.stringify(pending));
            return JSON.stringify({
                kind: 'progress',
                request_id: pending.requestId,
                correlation: pending.correlation,
                text: text || null,
                activity: activity || null,
                thinking
            });
        }

        if (!text) return null;
        const result = JSON.stringify({
            kind: 'answered',
            request_id: pending.requestId,
            correlation: pending.correlation,
            text
        });
        sessionStorage.removeItem('__rochePendingChat');
        return result;
    })()"#;
    let callback_events = events.clone();
    if let Err(error) = webview.evaluate_script_with_callback(SCRIPT, move |value| {
        let Ok(encoded) = serde_json::from_str::<String>(value.trim()) else {
            return;
        };
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&encoded) else {
            return;
        };
        let Some(correlation) = payload
            .get("correlation")
            .cloned()
            .and_then(|value| serde_json::from_value::<WebGptTurnCorrelation>(value).ok())
        else {
            return;
        };
        match payload.get("kind").and_then(serde_json::Value::as_str) {
            Some("submitted") => {
                let _ = callback_events.send(WebGptBrowserEvent::ChatSubmitted {
                    correlation: correlation.clone(),
                });
            }
            Some("progress") => {
                let text = payload
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                let activity = payload
                    .get("activity")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                let thinking = payload
                    .get("thinking")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let _ = callback_events.send(WebGptBrowserEvent::ChatProgress {
                    correlation: correlation.clone(),
                    text,
                    activity,
                    thinking,
                });
            }
            Some("answered") => {
                let Some(text) = payload.get("text").and_then(serde_json::Value::as_str) else {
                    return;
                };
                let _ = callback_events.send(WebGptBrowserEvent::ChatAnswered {
                    correlation: correlation.clone(),
                    text: text.to_owned(),
                });
            }
            Some("cancelled") => {
                let _ = callback_events.send(WebGptBrowserEvent::ChatCancelled {
                    correlation: correlation.clone(),
                });
            }
            Some("error") => {
                let detail = payload
                    .get("detail")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown ChatGPT submit error");
                let _ = callback_events.send(WebGptBrowserEvent::ChatFailed {
                    correlation: correlation.clone(),
                    message: format!("ChatGPT request failed: {detail}"),
                });
            }
            Some("probe") if std::env::var_os("ROCHE_WEBGPT_DIAGNOSTICS").is_some() => {
                let detail = payload
                    .get("detail")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("no probe detail");
                let _ = callback_events.send(WebGptBrowserEvent::Error(format!(
                    "ChatGPT probe {}: {detail}",
                    correlation.request_id
                )));
            }
            _ => {}
        }
    }) {
        let _ = events.send(WebGptBrowserEvent::Error(format!(
            "Could not inspect ChatGPT response state: {error}"
        )));
    }
}
