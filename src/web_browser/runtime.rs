//! Scheduler-backed shared browser runtime and request/event routing.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use crate::web_browser_pool::{PoolEffect, PoolTurn, Slot, SlotEvent, WebGptPoolScheduler};
use crate::web_browser_protocol::{WebGptTurnCorrelation, WebGptTurnRequest};

#[cfg(test)]
use super::{BrowserHostCommand, cancel_script_event};
use super::{WebGptBrowserController, WebGptBrowserEvent, WebGptBrowserState};

struct SharedBrowserInner {
    controller: WebGptBrowserController,
    ui_events: VecDeque<WebGptBrowserEvent>,
    worker_events: VecDeque<WebGptBrowserEvent>,
    /// Capacity-1 scheduling authority. Slots/queue/generation live here.
    scheduler: WebGptPoolScheduler,
    /// Full owner + message payload for every queued or in-flight turn, keyed by
    /// request id, so scheduler leases can be expanded back to full correlations.
    turn_payloads: HashMap<String, TurnPayload>,
    /// The exact full correlation of the currently leased turn, for event gating.
    active_correlation: Option<WebGptTurnCorrelation>,
    /// Whether the helper can accept a physical submit. A scheduler lease may be
    /// held while unavailable, but it is not sent until LoggedIn resumes it.
    browser_ready: bool,
    /// Distinguishes a scheduler lease from a command already sent to WebView2.
    active_dispatched: bool,
    /// Bounded diagnostics for duplicate/no-capacity/stale rejections that must
    /// not be routed into a UI/worker event queue or mutate another turn.
    diagnostics: VecDeque<String>,
}

struct TurnPayload {
    request: WebGptTurnRequest,
    text: String,
    paths: Vec<std::path::PathBuf>,
}

#[derive(Clone)]
pub struct SharedWebGptBrowser {
    inner: Arc<Mutex<SharedBrowserInner>>,
}

impl SharedWebGptBrowser {
    pub fn spawn() -> Self {
        Self::from_controller(WebGptBrowserController::spawn())
    }

    pub fn disabled(message: &str) -> Self {
        Self::from_controller(WebGptBrowserController::disabled(message))
    }

    fn from_controller(controller: WebGptBrowserController) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SharedBrowserInner {
                controller,
                ui_events: VecDeque::new(),
                worker_events: VecDeque::new(),
                scheduler: WebGptPoolScheduler::new(1),
                turn_payloads: HashMap::new(),
                active_correlation: None,
                browser_ready: true,
                active_dispatched: false,
                diagnostics: VecDeque::new(),
            })),
        }
    }

    pub fn show_login(&self) {
        self.inner
            .lock()
            .expect("browser mutex poisoned")
            .controller
            .show_login();
    }

    pub fn hide(&self) {
        self.inner
            .lock()
            .expect("browser mutex poisoned")
            .controller
            .hide();
    }

    pub fn wake(&self, request_id: String) {
        self.inner
            .lock()
            .expect("browser mutex poisoned")
            .controller
            .wake(request_id);
    }

    pub fn submit_chat(&self, request: WebGptTurnRequest, text: String) {
        self.submit_chat_with_attachments(request, text, Vec::new());
    }

    pub fn submit_chat_with_attachments(
        &self,
        request: WebGptTurnRequest,
        text: String,
        paths: Vec<std::path::PathBuf>,
    ) {
        let mut inner = self.inner.lock().expect("browser mutex poisoned");
        let pool_turn = request_to_pool_turn(&request);
        let request_id = request.request_id.clone();
        if !inner.turn_payloads.contains_key(&request_id) {
            // Only insert on first sight. A duplicate request id must never
            // overwrite the legitimate active/queued payload.
            inner.turn_payloads.insert(
                request_id.clone(),
                TurnPayload {
                    request,
                    text,
                    paths,
                },
            );
        }
        let effects = inner.scheduler.enqueue(pool_turn);
        process_scheduler_effects(&mut inner, effects);
    }

    pub fn cancel_chat(&self, request: WebGptTurnRequest) {
        let mut inner = self.inner.lock().expect("browser mutex poisoned");
        if let Some(active) = inner.active_correlation.clone()
            && request_matches_correlation(&request, &active)
        {
            let slot_event = correlation_slot_event(&active);
            let effects = inner.scheduler.cancel(slot_event);
            process_scheduler_effects(&mut inner, effects);
            return;
        }
        // Not the active turn. Only a queued turn whose stored owner matches the
        // request exactly may be cancelled; a wrong-owner request sharing a queued
        // request id must not touch the legitimate queued turn.
        match inner.turn_payloads.get(&request.request_id) {
            Some(payload) if payload.request == request => {
                let effects = inner.scheduler.cancel_queued(&request.request_id);
                process_scheduler_effects(&mut inner, effects);
            }
            _ => {
                let request_id = request.request_id;
                inner.push_diagnostic(format!(
                    "Web GPT queued cancel ignored for unknown/mismatched owner: {request_id}"
                ));
            }
        }
    }

    pub fn reload(&self) {
        self.inner
            .lock()
            .expect("browser mutex poisoned")
            .controller
            .reload();
    }

    pub fn drain_ui(&self) -> Vec<WebGptBrowserEvent> {
        self.drain(false)
    }

    pub fn drain_worker(&self) -> Vec<WebGptBrowserEvent> {
        self.drain(true)
    }

    fn drain(&self, worker: bool) -> Vec<WebGptBrowserEvent> {
        let mut inner = self.inner.lock().expect("browser mutex poisoned");
        for event in inner.controller.drain() {
            handle_shared_event(&mut inner, event);
        }
        if worker {
            inner.worker_events.drain(..).collect()
        } else {
            inner.ui_events.drain(..).collect()
        }
    }
}

impl SharedBrowserInner {
    fn push_diagnostic(&mut self, message: String) {
        const MAX_DIAGNOSTICS: usize = 64;
        if self.diagnostics.len() >= MAX_DIAGNOSTICS {
            self.diagnostics.pop_front();
        }
        self.diagnostics.push_back(message);
    }
}

fn request_to_pool_turn(request: &WebGptTurnRequest) -> PoolTurn {
    PoolTurn {
        request_id: request.request_id.clone(),
        account: Some(request.account_id.clone()),
    }
}

fn correlation_slot_event(correlation: &WebGptTurnCorrelation) -> SlotEvent {
    SlotEvent {
        slot: Slot {
            index: correlation.lease.slot_id as usize,
            generation: correlation.lease.generation,
        },
        request_id: correlation.request_id.clone(),
        account: Some(correlation.account_id.clone()),
    }
}

fn submit_active_turn_if_ready(inner: &mut SharedBrowserInner) {
    if !inner.browser_ready || inner.active_dispatched {
        return;
    }
    let Some(correlation) = inner.active_correlation.clone() else {
        return;
    };
    let Some(payload) = inner.turn_payloads.get(&correlation.request_id) else {
        inner.push_diagnostic(format!(
            "Web GPT active lease had no stored payload: {}",
            correlation.request_id
        ));
        return;
    };
    let text = payload.text.clone();
    let paths = payload.paths.clone();
    inner.active_dispatched = true;
    inner
        .controller
        .submit_chat_with_attachments(correlation, text, paths);
}

/// Apply the scheduler's observable effects to the single physical controller and
/// the routed event queues. Effects are processed in order: a terminal frees and
/// then a queued dispatch leases the freed slot.
fn process_scheduler_effects(inner: &mut SharedBrowserInner, effects: Vec<PoolEffect>) {
    for effect in effects {
        match effect {
            PoolEffect::Dispatch(leased) => {
                let Some(payload) = inner.turn_payloads.get(&leased.request_id) else {
                    inner.push_diagnostic(format!(
                        "Web GPT dispatch had no stored payload: {}",
                        leased.request_id
                    ));
                    inner.active_correlation = None;
                    continue;
                };
                let correlation = payload
                    .request
                    .clone()
                    .lease(leased.slot.index as u32, leased.slot.generation);
                inner.active_correlation = Some(correlation.clone());
                inner.active_dispatched = false;
                submit_active_turn_if_ready(inner);
            }
            PoolEffect::Complete(leased) | PoolEffect::CancelAck(leased) => {
                if inner
                    .active_correlation
                    .as_ref()
                    .map(|correlation| correlation.request_id.as_str())
                    == Some(leased.request_id.as_str())
                {
                    inner.active_correlation = None;
                    inner.active_dispatched = false;
                }
                inner.turn_payloads.remove(&leased.request_id);
            }
            PoolEffect::CancelRequest(leased) => {
                if let Some(correlation) = inner.active_correlation.clone()
                    && correlation.request_id == leased.request_id
                {
                    inner.controller.cancel_chat(correlation);
                }
            }
            PoolEffect::CancelQueued(pool_turn) => {
                let Some(payload) = inner.turn_payloads.remove(&pool_turn.request_id) else {
                    continue;
                };
                let worker = payload.request.task_id.is_some();
                let event = WebGptBrowserEvent::ChatQueueCancelled {
                    request: payload.request,
                };
                if worker {
                    inner.worker_events.push_back(event);
                } else {
                    inner.ui_events.push_back(event);
                }
            }
            PoolEffect::RejectDuplicate { request_id } => {
                // We never inserted a duplicate payload (preserve the original),
                // so nothing is removed here; only a bounded diagnostic is made.
                inner.push_diagnostic(format!("Web GPT duplicate request rejected: {request_id}"));
            }
            PoolEffect::RejectStale {
                request_id, reason, ..
            } => {
                // A stale terminal/diagnostic must never mutate a running turn,
                // release a lease, or cross-route. It only surfaces a bounded log.
                inner.push_diagnostic(format!(
                    "Web GPT stale event rejected ({reason:?}): {request_id}"
                ));
            }
        }
    }
}

fn handle_shared_event(inner: &mut SharedBrowserInner, event: WebGptBrowserEvent) {
    if let WebGptBrowserEvent::State(state) = &event {
        handle_browser_state(inner, state.clone());
        return;
    }
    if let Some(correlation) = event_chat_correlation(&event).cloned() {
        // Gate on the exact stored full correlation.
        let Some(active) = inner.active_correlation.clone() else {
            inner.push_diagnostic(format!(
                "Web GPT chat event rejected with no active turn: {}",
                correlation.request_id
            ));
            return;
        };
        if active != correlation {
            inner.push_diagnostic(format!(
                "Web GPT stale chat event rejected for request {}",
                correlation.request_id
            ));
            return;
        }

        let terminal = matches!(
            &event,
            WebGptBrowserEvent::ChatAnswered { .. }
                | WebGptBrowserEvent::ChatCancelled { .. }
                | WebGptBrowserEvent::ChatFailed { .. }
        );
        if !terminal {
            // ChatSubmitted / ChatProgress: route only.
            route_correlation_event(inner, event);
            return;
        }

        // Transition first so we only route a terminal that actually freed.
        let slot_event = correlation_slot_event(&correlation);
        let effects = if matches!(&event, WebGptBrowserEvent::ChatCancelled { .. }) {
            inner.scheduler.cancel_ack(slot_event)
        } else {
            inner.scheduler.complete(slot_event)
        };
        if !matches!(
            effects.first(),
            Some(PoolEffect::Complete(_) | PoolEffect::CancelAck(_))
        ) {
            // Scheduler rejected the terminal (e.g. a cancel ack with no prior
            // cancel request): surface a bounded diagnostic and drop the event.
            inner.push_diagnostic(format!(
                "Web GPT terminal rejected for request {}",
                correlation.request_id
            ));
            return;
        }
        route_correlation_event(inner, event);
        process_scheduler_effects(inner, effects);
        return;
    }

    // Wake / State / Error / ChatQueueCancelled.
    route_other_event(inner, event);
}

fn handle_browser_state(inner: &mut SharedBrowserInner, state: WebGptBrowserState) {
    route_other_event(inner, WebGptBrowserEvent::State(state.clone()));
    match state {
        WebGptBrowserState::LoggedIn => {
            inner.browser_ready = true;
            submit_active_turn_if_ready(inner);
        }
        WebGptBrowserState::Starting => {
            inner.browser_ready = false;
        }
        WebGptBrowserState::LoginRequired => {
            inner.browser_ready = false;
            if let Some(correlation) = inner.active_correlation.clone() {
                handle_shared_event(
                    inner,
                    WebGptBrowserEvent::ChatFailed {
                        correlation,
                        message: "ChatGPT login is required before the request can continue"
                            .to_owned(),
                    },
                );
            }
        }
        WebGptBrowserState::Offline(message) => {
            inner.browser_ready = false;
            if let Some(correlation) = inner.active_correlation.clone() {
                handle_shared_event(
                    inner,
                    WebGptBrowserEvent::ChatFailed {
                        correlation,
                        message: format!("Web GPT browser went offline: {message}"),
                    },
                );
            }
        }
    }
}

fn route_correlation_event(inner: &mut SharedBrowserInner, event: WebGptBrowserEvent) {
    if event_chat_correlation(&event).is_some_and(|correlation| correlation.is_worker()) {
        inner.worker_events.push_back(event);
    } else {
        inner.ui_events.push_back(event);
    }
}

fn route_other_event(inner: &mut SharedBrowserInner, event: WebGptBrowserEvent) {
    let active_worker = inner
        .active_correlation
        .as_ref()
        .is_some_and(|correlation| correlation.is_worker());
    match &event {
        WebGptBrowserEvent::WakeSubmitted { request_id } => {
            if request_id.starts_with("web-worker-") {
                inner.worker_events.push_back(event);
            } else {
                inner.ui_events.push_back(event);
            }
        }
        WebGptBrowserEvent::State(_) => {
            inner.ui_events.push_back(event.clone());
            inner.worker_events.push_back(event);
        }
        WebGptBrowserEvent::Error(_) if active_worker => {
            inner.worker_events.push_back(event);
        }
        WebGptBrowserEvent::Error(_) if inner.active_correlation.is_some() => {
            inner.ui_events.push_back(event);
        }
        WebGptBrowserEvent::Error(_) => {
            inner.ui_events.push_back(event.clone());
            inner.worker_events.push_back(event);
        }
        WebGptBrowserEvent::ChatQueueCancelled { request } => {
            if request.task_id.is_some() {
                inner.worker_events.push_back(event);
            } else {
                inner.ui_events.push_back(event);
            }
        }
        WebGptBrowserEvent::ChatSubmitted { .. }
        | WebGptBrowserEvent::ChatProgress { .. }
        | WebGptBrowserEvent::ChatAnswered { .. }
        | WebGptBrowserEvent::ChatCancelled { .. }
        | WebGptBrowserEvent::ChatFailed { .. } => {}
    }
}

fn event_chat_correlation(event: &WebGptBrowserEvent) -> Option<&WebGptTurnCorrelation> {
    match event {
        WebGptBrowserEvent::ChatSubmitted { correlation }
        | WebGptBrowserEvent::ChatProgress { correlation, .. }
        | WebGptBrowserEvent::ChatAnswered { correlation, .. }
        | WebGptBrowserEvent::ChatCancelled { correlation }
        | WebGptBrowserEvent::ChatFailed { correlation, .. } => Some(correlation),
        WebGptBrowserEvent::WakeSubmitted { .. }
        | WebGptBrowserEvent::State(_)
        | WebGptBrowserEvent::Error(_)
        | WebGptBrowserEvent::ChatQueueCancelled { .. } => None,
    }
}

/// Compare the unleased ownership of a request against an active lease.
fn request_matches_correlation(
    request: &WebGptTurnRequest,
    correlation: &WebGptTurnCorrelation,
) -> bool {
    request.account_id == correlation.account_id
        && request.session_id == correlation.session_id
        && request.task_id == correlation.task_id
        && request.request_id == correlation.request_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web_browser_protocol::WebGptSlotLease;

    fn test_browser() -> SharedWebGptBrowser {
        SharedWebGptBrowser::disabled("test browser")
    }

    fn worker_request(request_id: &str) -> WebGptTurnRequest {
        WebGptTurnRequest::worker(
            "session-a".to_owned(),
            "task-a".to_owned(),
            request_id.to_owned(),
        )
    }

    fn native_request(request_id: &str) -> WebGptTurnRequest {
        WebGptTurnRequest::native_chat("session-a".to_owned(), request_id.to_owned())
    }

    fn active_request_id(inner: &SharedBrowserInner) -> &str {
        &inner
            .active_correlation
            .as_ref()
            .expect("active turn")
            .request_id
    }

    #[test]
    fn stale_terminal_cannot_release_new_fifo_turn_or_route_late_payload() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());
        browser.submit_chat(worker_request("req-b"), "B".to_owned());
        browser.submit_chat(worker_request("req-c"), "C".to_owned());

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let active_a = inner.active_correlation.clone().expect("active A");
        // A matching completion frees A and dispatches B.
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: active_a.clone(),
                text: "A answer".to_owned(),
            },
        );
        assert_eq!(active_request_id(&inner), "req-b");
        assert_eq!(inner.scheduler.queued_count(), 1);
        assert_eq!(inner.scheduler.in_flight_count(), 1);
        assert_eq!(inner.worker_events.len(), 1);

        // A late terminal from the old lease (active is now B) is dropped.
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: active_a,
                text: "late A payload".to_owned(),
            },
        );
        assert_eq!(active_request_id(&inner), "req-b");
        assert_eq!(inner.scheduler.queued_count(), 1);
        assert_eq!(inner.worker_events.len(), 1);
        assert!(!inner.worker_events.iter().any(|event| {
            matches!(
                event,
                WebGptBrowserEvent::ChatAnswered { text, .. } if text == "late A payload"
            )
        }));
    }

    #[test]
    fn matching_terminal_advances_fifo_exactly_once() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());
        browser.submit_chat(worker_request("req-b"), "B".to_owned());
        browser.submit_chat(worker_request("req-c"), "C".to_owned());

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let active_a = inner.active_correlation.clone().expect("active A");
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatFailed {
                correlation: active_a.clone(),
                message: "A failed".to_owned(),
            },
        );
        assert_eq!(active_request_id(&inner), "req-b");
        assert_eq!(inner.scheduler.queued_count(), 1);
        assert_eq!(inner.worker_events.len(), 1);

        // Re-delivering the same terminal for A must not advance again.
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatFailed {
                correlation: active_a,
                message: "late A failure".to_owned(),
            },
        );
        assert_eq!(active_request_id(&inner), "req-b");
        assert_eq!(inner.scheduler.queued_count(), 1);
        assert_eq!(inner.worker_events.len(), 1);
        drop(inner);

        // Request a cancel on B: it must move to Cancelling without freeing.
        browser.cancel_chat(worker_request("req-b"));
        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        assert_eq!(active_request_id(&inner), "req-b");
        assert_eq!(inner.scheduler.in_flight_count(), 1);
        assert_eq!(inner.scheduler.queued_count(), 1);

        // The matching cancel acknowledgment frees B and dispatches C.
        let active_b = inner.active_correlation.clone().expect("active B");
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatCancelled {
                correlation: active_b.clone(),
            },
        );
        assert_eq!(active_request_id(&inner), "req-c");
        assert_eq!(inner.scheduler.queued_count(), 0);
        assert_eq!(inner.worker_events.len(), 2);

        // Replaying the same ack does not advance again.
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatCancelled {
                correlation: active_b,
            },
        );
        assert_eq!(active_request_id(&inner), "req-c");
        assert_eq!(inner.worker_events.len(), 2);
    }

    #[test]
    fn generic_error_is_diagnostic_and_does_not_advance_fifo() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());
        browser.submit_chat(worker_request("req-b"), "B".to_owned());

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::Error("runtime noise".to_owned()),
        );
        assert_eq!(active_request_id(&inner), "req-a");
        assert_eq!(inner.scheduler.queued_count(), 1);
        assert!(matches!(
            inner.worker_events.as_slices().0.first(),
            Some(WebGptBrowserEvent::Error(message)) if message == "runtime noise"
        ));
    }

    #[test]
    fn cancel_control_error_is_nonterminal() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());
        browser.submit_chat(worker_request("req-b"), "B".to_owned());

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let active_a = inner.active_correlation.clone().expect("active A");
        // A cancel-control failure is routed but never releases the active turn.
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::Error(format!(
                "Could not cancel Web GPT request {}: script failed",
                active_a.request_id
            )),
        );
        assert_eq!(active_request_id(&inner), "req-a");
        assert_eq!(inner.scheduler.queued_count(), 1);
    }

    #[test]
    fn pending_cancel_script_result_is_not_an_acknowledgement() {
        let correlation = worker_request("req-a").lease(0, 3);

        assert_eq!(cancel_script_event("\"pending\"", &correlation), None);
        assert_eq!(
            cancel_script_event("\"cancelled\"", &correlation),
            Some(WebGptBrowserEvent::ChatCancelled { correlation })
        );
    }

    #[test]
    fn unavailable_state_reconciles_active_and_defers_next_submit_until_logged_in() {
        for unavailable in [
            WebGptBrowserState::LoginRequired,
            WebGptBrowserState::Offline("host ended".to_owned()),
        ] {
            let browser = test_browser();
            browser.submit_chat(worker_request("req-a"), "A".to_owned());
            browser.submit_chat(worker_request("req-b"), "B".to_owned());

            let mut inner = browser.inner.lock().expect("browser mutex poisoned");
            let active_a = inner.active_correlation.clone().expect("active A");
            assert!(inner.active_dispatched);
            handle_shared_event(&mut inner, WebGptBrowserEvent::State(unavailable.clone()));

            assert_eq!(active_request_id(&inner), "req-b");
            assert_eq!(inner.scheduler.in_flight_count(), 1);
            assert_eq!(inner.scheduler.queued_count(), 0);
            assert!(!inner.browser_ready);
            assert!(!inner.active_dispatched);
            assert!(inner.worker_events.iter().any(|event| {
                matches!(
                    event,
                    WebGptBrowserEvent::ChatFailed { correlation, .. }
                        if correlation == &active_a
                )
            }));

            handle_shared_event(
                &mut inner,
                WebGptBrowserEvent::State(WebGptBrowserState::LoggedIn),
            );
            assert!(inner.browser_ready);
            assert!(inner.active_dispatched);
            assert_eq!(active_request_id(&inner), "req-b");
        }
    }

    #[test]
    fn wake_submitted_keeps_its_independent_routing_without_active_chat_turn() {
        let browser = test_browser();
        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::WakeSubmitted {
                request_id: "web-worker-wake".to_owned(),
            },
        );
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::WakeSubmitted {
                request_id: "web-chat-wake".to_owned(),
            },
        );
        assert!(matches!(
            inner.worker_events.as_slices().0.first(),
            Some(WebGptBrowserEvent::WakeSubmitted { request_id })
                if request_id == "web-worker-wake"
        ));
        assert!(matches!(
            inner.ui_events.as_slices().0.first(),
            Some(WebGptBrowserEvent::WakeSubmitted { request_id }) if request_id == "web-chat-wake"
        ));
    }

    #[test]
    fn queued_cancel_emits_queue_cancelled_only_for_explicit_pending_request() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-active"), "A".to_owned());
        browser.submit_chat(worker_request("req-pending"), "B".to_owned());

        let inner = browser.inner.lock().expect("browser mutex poisoned");
        assert_eq!(active_request_id(&inner), "req-active");
        assert_eq!(inner.scheduler.queued_count(), 1);
        let before = inner.worker_events.len();
        drop(inner);

        // Removing a concrete queued request emits ChatQueueCancelled exactly once.
        browser.cancel_chat(worker_request("req-pending"));
        let inner = browser.inner.lock().expect("browser mutex poisoned");
        assert_eq!(active_request_id(&inner), "req-active");
        assert_eq!(inner.scheduler.queued_count(), 0);
        assert_eq!(inner.worker_events.len(), before + 1);
        assert!(matches!(
            inner.worker_events.back(),
            Some(WebGptBrowserEvent::ChatQueueCancelled { request })
                if request.request_id == "req-pending"
        ));

        // Cancelling a request that is neither active nor queued emits nothing.
        drop(inner);
        browser.cancel_chat(worker_request("req-unknown"));
        let inner = browser.inner.lock().expect("browser mutex poisoned");
        assert_eq!(inner.worker_events.len(), before + 1);
        // The unknown cancel only surfaced a bounded diagnostic, no cross-routing.
        assert!(!inner.diagnostics.is_empty());
    }

    #[test]
    fn same_request_id_reused_across_generations_is_distinct() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-reused"), "first".to_owned());
        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let first = inner.active_correlation.clone().expect("first lease");
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: first.clone(),
                text: "one".to_owned(),
            },
        );
        assert!(inner.active_correlation.is_none());
        drop(inner);

        // Reusing the same request id gets a fresh generation on slot 0.
        browser.submit_chat(worker_request("req-reused"), "second".to_owned());
        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let second = inner.active_correlation.clone().expect("second lease");
        assert_eq!(first.request_id, second.request_id);
        assert_eq!(first.lease.slot_id, second.lease.slot_id);
        assert_ne!(first.lease.generation, second.lease.generation);

        // A late terminal from the first lease must not release the new turn.
        let before = inner.worker_events.len();
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: first,
                text: "stale".to_owned(),
            },
        );
        assert_eq!(inner.active_correlation, Some(second.clone()));
        assert_eq!(inner.worker_events.len(), before);
    }

    #[test]
    fn mismatched_owner_and_lease_are_rejected() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let active_a = inner.active_correlation.clone().expect("active A");

        // Wrong generation on the same slot.
        let wrong_lease = WebGptTurnCorrelation {
            lease: WebGptSlotLease {
                slot_id: active_a.lease.slot_id,
                generation: active_a.lease.generation + 1,
            },
            ..active_a.clone()
        };
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: wrong_lease,
                text: "wrong lease".to_owned(),
            },
        );

        // Wrong account, session, and task ownership (same request id).
        let wrong_account = WebGptTurnCorrelation {
            account_id: "other-account".to_owned(),
            ..active_a.clone()
        };
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatCancelled {
                correlation: wrong_account,
            },
        );
        let wrong_session = WebGptTurnCorrelation {
            session_id: "other-session".to_owned(),
            ..active_a.clone()
        };
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: wrong_session,
                text: "wrong session".to_owned(),
            },
        );
        let wrong_task = WebGptTurnCorrelation {
            task_id: None,
            ..active_a.clone()
        };
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatCancelled {
                correlation: wrong_task,
            },
        );

        // None of the mismatched events were routed or released the active turn.
        assert_eq!(inner.active_correlation, Some(active_a));
        assert!(inner.worker_events.is_empty());
        assert!(inner.ui_events.is_empty());
        assert_eq!(inner.scheduler.in_flight_count(), 1);
        assert_eq!(inner.scheduler.queued_count(), 0);
        assert!(!inner.diagnostics.is_empty());
    }

    #[test]
    fn native_and_worker_events_route_by_task_ownership() {
        let browser = test_browser();
        browser.submit_chat(native_request("req-native"), "native".to_owned());
        assert_eq!(browser.inner.lock().unwrap().ui_events.len(), 0);
        assert_eq!(browser.inner.lock().unwrap().worker_events.len(), 0);

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let active = inner.active_correlation.clone().expect("active native");
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: active.clone(),
                text: "native answer".to_owned(),
            },
        );
        assert!(matches!(
            inner.ui_events.as_slices().0.first(),
            Some(WebGptBrowserEvent::ChatAnswered { text, .. }) if text == "native answer"
        ));
        assert!(inner.worker_events.is_empty());
        assert!(inner.active_correlation.is_none());
    }

    #[test]
    fn host_command_serde_round_trip_preserves_full_correlation() {
        let correlation = worker_request("req-a").lease(0, 7);
        let chat = BrowserHostCommand::Chat {
            correlation: correlation.clone(),
            text: "hello".to_owned(),
            attachments: Vec::new(),
        };
        let decoded: BrowserHostCommand =
            serde_json::from_str(&serde_json::to_string(&chat).unwrap()).unwrap();
        assert_eq!(decoded, chat);

        let cancel = BrowserHostCommand::Cancel {
            correlation: correlation.clone(),
        };
        let decoded: BrowserHostCommand =
            serde_json::from_str(&serde_json::to_string(&cancel).unwrap()).unwrap();
        assert_eq!(decoded, cancel);
    }

    #[test]
    fn chat_event_serde_round_trip_preserves_full_correlation() {
        let correlation = worker_request("req-a").lease(1, 9);
        let answered = WebGptBrowserEvent::ChatAnswered {
            correlation: correlation.clone(),
            text: "answer".to_owned(),
        };
        let decoded: WebGptBrowserEvent =
            serde_json::from_str(&serde_json::to_string(&answered).unwrap()).unwrap();
        assert_eq!(decoded, answered);

        let request = worker_request("req-q");
        let queued = WebGptBrowserEvent::ChatQueueCancelled {
            request: request.clone(),
        };
        let decoded: WebGptBrowserEvent =
            serde_json::from_str(&serde_json::to_string(&queued).unwrap()).unwrap();
        assert_eq!(decoded, queued);
    }

    #[test]
    fn cancel_request_alone_does_not_dispatch_queued_turn() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());
        browser.submit_chat(worker_request("req-c"), "C".to_owned());

        {
            let inner = browser.inner.lock().expect("browser mutex poisoned");
            assert_eq!(active_request_id(&inner), "req-a");
            assert_eq!(inner.scheduler.queued_count(), 1);
            assert_eq!(inner.scheduler.in_flight_count(), 1);
        }

        // Requesting a cancel moves A to Cancelling but must not free it or
        // dispatch the queued C into the draining WebView.
        browser.cancel_chat(worker_request("req-a"));
        let inner = browser.inner.lock().expect("browser mutex poisoned");
        assert_eq!(active_request_id(&inner), "req-a");
        assert_eq!(inner.scheduler.in_flight_count(), 1);
        assert_eq!(inner.scheduler.queued_count(), 1);
    }

    #[test]
    fn cancel_acknowledgment_dispatches_queued_turn_once() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());
        browser.submit_chat(worker_request("req-c"), "C".to_owned());
        browser.cancel_chat(worker_request("req-a"));

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let active_a = inner.active_correlation.clone().expect("active A");
        assert_eq!(inner.scheduler.queued_count(), 1);

        // The correlated acknowledgment frees A and dispatches C exactly once.
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatCancelled {
                correlation: active_a.clone(),
            },
        );
        assert_eq!(active_request_id(&inner), "req-c");
        assert_eq!(inner.scheduler.in_flight_count(), 1);
        assert_eq!(inner.scheduler.queued_count(), 0);
        assert_eq!(inner.worker_events.len(), 1);

        // A second (stale) ack does not advance again.
        let before = inner.worker_events.len();
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatCancelled {
                correlation: active_a,
            },
        );
        assert_eq!(active_request_id(&inner), "req-c");
        assert_eq!(inner.worker_events.len(), before);
    }

    #[test]
    fn completion_while_cancelling_is_safe() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());
        browser.submit_chat(worker_request("req-c"), "C".to_owned());
        browser.cancel_chat(worker_request("req-a"));

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        let active_a = inner.active_correlation.clone().expect("active A");
        // A natural completion while Cancelling is a safe terminal that frees the
        // slot and schedules the waiting turn.
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: active_a,
                text: "completed anyway".to_owned(),
            },
        );
        assert_eq!(active_request_id(&inner), "req-c");
        assert_eq!(inner.scheduler.in_flight_count(), 1);
        assert_eq!(inner.scheduler.queued_count(), 0);
    }

    #[test]
    fn duplicate_request_rejection_does_not_disturb_active_turn() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-a"), "A".to_owned());
        let before = {
            let inner = browser.inner.lock().expect("browser mutex poisoned");
            (inner.worker_events.len(), inner.scheduler.in_flight_count())
        };
        assert_eq!(before.1, 1);

        // Submitting the same request id again is rejected without redispatch.
        browser.submit_chat(worker_request("req-a"), "A again".to_owned());

        let mut inner = browser.inner.lock().expect("browser mutex poisoned");
        assert_eq!(active_request_id(&inner), "req-a");
        assert_eq!(inner.scheduler.in_flight_count(), 1);
        assert_eq!(inner.scheduler.queued_count(), 0);
        assert_eq!(inner.worker_events.len(), before.0);
        // Only the original active payload remains; the duplicate was removed.
        assert_eq!(
            inner
                .turn_payloads
                .get("req-a")
                .map(|payload| payload.text.as_str()),
            Some("A")
        );
        assert!(!inner.diagnostics.is_empty());

        // The original active turn still completes normally and can dispatch the
        // next FIFO turn, proving the duplicate never corrupted the lease.
        let active_a = inner.active_correlation.clone().expect("active A");
        handle_shared_event(
            &mut inner,
            WebGptBrowserEvent::ChatAnswered {
                correlation: active_a,
                text: "A final".to_owned(),
            },
        );
        assert!(inner.active_correlation.is_none());
        assert_eq!(inner.scheduler.in_flight_count(), 0);
        drop(inner);

        browser.submit_chat(worker_request("req-c"), "C".to_owned());
        let inner = browser.inner.lock().expect("browser mutex poisoned");
        assert_eq!(active_request_id(&inner), "req-c");
        assert_eq!(
            inner
                .turn_payloads
                .get("req-c")
                .map(|payload| payload.text.as_str()),
            Some("C")
        );
    }

    #[test]
    fn wrong_owner_queued_cancel_leaves_queued_turn_untouched() {
        let browser = test_browser();
        browser.submit_chat(worker_request("req-active"), "A".to_owned());
        browser.submit_chat(worker_request("req-pending"), "B".to_owned());

        {
            let inner = browser.inner.lock().expect("browser mutex poisoned");
            assert_eq!(inner.scheduler.queued_count(), 1);
            assert_eq!(inner.scheduler.in_flight_count(), 1);
            assert_eq!(inner.turn_payloads.len(), 2);
        }

        // A wrong owner sharing the queued request id (different session) must
        // not cancel the legitimate queued turn.
        let wrong_owner = WebGptTurnRequest::worker(
            "other-session".to_owned(),
            "task-a".to_owned(),
            "req-pending".to_owned(),
        );
        browser.cancel_chat(wrong_owner);

        let inner = browser.inner.lock().expect("browser mutex poisoned");
        assert_eq!(active_request_id(&inner), "req-active");
        assert_eq!(inner.scheduler.queued_count(), 1);
        assert_eq!(inner.turn_payloads.len(), 2);
        assert!(
            !inner
                .worker_events
                .iter()
                .any(|event| { matches!(event, WebGptBrowserEvent::ChatQueueCancelled { .. }) })
        );
        assert!(!inner.diagnostics.is_empty());
    }
}
