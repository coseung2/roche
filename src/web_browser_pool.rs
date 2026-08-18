//! Production-disconnected pure-Rust scheduler/state for bounded Web GPT slots.
//!
//! This module is intentionally self-contained: it performs no I/O, spawns no
//! processes or threads, and depends only on the standard library. It models the
//! *scheduling* of bounded, parallel Web GPT turns so the logic can be reasoned
//! about and tested deterministically before it is wired into the live browser
//! runtime. It never touches `WebView2`, Wry, Tao, or the current
//! [`SharedWebGptBrowser`](crate::web_browser::SharedWebGptBrowser) runtime path.
//!
//! Contract summary:
//! - Slots are bounded and addressable by a stable [`Slot`] identity; the default
//!   concurrency is two. A zero-slot pool is an explicit "no capacity" state and
//!   never silently becomes one slot.
//! - Waiting turns run strictly FIFO; the earliest free slot is immediately leased
//!   to the earliest waiting turn. Capacity accounting treats both `Running` and
//!   `Cancelling` leases as occupied, so a slot is never redispatched until a free
//!   terminal state is reached.
//! - Cancellation is a two-phase drain. Requesting a cancel moves a `Running` slot
//!   to `Cancelling` and emits an observable cancel-*request* effect, but does not
//!   free the slot. Only a correlated cancel *acknowledgment* (or a correlated safe
//!   terminal completion) frees the slot and dispatches the next FIFO turn. This
//!   prevents a queued turn from being submitted into a WebView before the previous
//!   turn's cancellation has actually drained.
//! - Every dispatch hands out a [`Slot`] (`index` + `generation`). A browser event
//!   must present the matching `slot` identity plus its `request_id` and `account`;
//!   any mismatch (wrong slot, wrong account, or an old generation after reuse) is
//!   rejected as stale. In-flight turns are never silently moved or replayed, and
//!   duplicate enqueues are refused.

use std::collections::VecDeque;

/// Default number of concurrent Web GPT turns a pool may run.
///
/// The shared contract sets the default future concurrency to two.
pub const DEFAULT_SLOT_COUNT: usize = 2;

/// Stable identity of a slot: its fixed index plus a per-lease generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// Position of the slot within the pool. Stable for the pool's lifetime.
    pub index: usize,
    /// Lease generation, incremented on every dispatch into this slot. Lets a
    /// stale event for a previously reused slot be detected even if a request id
    /// is accidentally reused later.
    pub generation: u64,
}

/// A unit of Web GPT work tracked by the scheduler.
///
/// The scheduler is content-agnostic: it only needs identity and correlation, so
/// the payload of the turn is intentionally not carried here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolTurn {
    /// Unique identifier that correlates a submitted turn with its events.
    pub request_id: String,
    /// Optional account label carried alongside the request for correlation.
    pub account: Option<String>,
}

/// A turn currently in flight on a specific slot lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeasedTurn {
    /// The slot lease this turn occupies.
    pub slot: Slot,
    /// The correlated request identifier.
    pub request_id: String,
    /// The correlated account label.
    pub account: Option<String>,
}

/// A browser/terminal event that must correlate with an in-flight slot lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotEvent {
    /// The slot identity the event claims to come from.
    pub slot: Slot,
    /// The request identifier the event claims to belong to.
    pub request_id: String,
    /// The account the event claims to belong to.
    pub account: Option<String>,
}

/// Why a [`SlotEvent`] was rejected as stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleReason {
    /// The pool has no capacity (zero slots) and accepted no turn.
    NoCapacity,
    /// The slot index is outside the pool's bounds.
    UnknownSlot,
    /// The slot index is valid but currently holds no lease.
    NotInFlight,
    /// The slot's generation no longer matches (it was reused).
    GenerationMismatch,
    /// The request id does not match the slot's current lease.
    WrongRequest,
    /// The account label does not match the slot's current lease.
    WrongAccount,
    /// A cancel acknowledgment arrived for a slot that never requested a cancel.
    NotCancelling,
}

/// The run/drain phase of an occupied slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotPhase {
    /// The turn is actively running toward a natural completion.
    Running,
    /// A cancel was requested; the slot stays occupied until acknowledged.
    Cancelling,
}

/// An observable scheduling outcome produced by [`WebGptPoolScheduler`].
///
/// These are the effects the caller should translate into its own event stream;
/// they describe behavior rather than internal state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolEffect {
    /// A queued turn was leased to an in-flight slot.
    Dispatch(LeasedTurn),
    /// A correlated safe terminal completion freed the slot (Running or Cancelling).
    Complete(LeasedTurn),
    /// A cancel was requested; the turn moved Running -> Cancelling. `Cancelling`
    /// slots stay occupied and are not redispatched.
    CancelRequest(LeasedTurn),
    /// A correlated cancel acknowledgment freed a `Cancelling` slot.
    CancelAck(LeasedTurn),
    /// A still-queued (not yet leased) turn was cancelled.
    CancelQueued(PoolTurn),
    /// A terminal/unknown event was rejected as stale.
    RejectStale {
        slot: Option<Slot>,
        request_id: String,
        reason: StaleReason,
    },
    /// A duplicate enqueue was refused to prevent replay/ambiguity.
    RejectDuplicate { request_id: String },
}

/// The lease held by a slot for its current in-flight turn, including its phase.
#[derive(Debug, Clone)]
struct Lease {
    generation: u64,
    request_id: String,
    account: Option<String>,
    phase: SlotPhase,
}

/// Per-slot state: generation (advanced on each dispatch) and the current lease.
#[derive(Debug)]
struct SlotState {
    generation: u64,
    lease: Option<Lease>,
}

/// Deterministic scheduler/state machine for bounded parallel Web GPT turns.
#[derive(Debug)]
pub struct WebGptPoolScheduler {
    slots: Vec<SlotState>,
    queue: VecDeque<PoolTurn>,
}

impl WebGptPoolScheduler {
    /// Create a scheduler with exactly `slots` in-flight capacity.
    ///
    /// Zero slots is an explicit "no capacity" state: `enqueue` is refused and no
    /// turn is ever dispatched. This is intentional so a disabled pool cannot be
    /// mistaken for a running single-slot pool.
    pub fn new(slots: usize) -> Self {
        Self {
            slots: (0..slots)
                .map(|_| SlotState {
                    generation: 0,
                    lease: None,
                })
                .collect(),
            queue: VecDeque::new(),
        }
    }

    /// The configured bound on concurrent in-flight turns.
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Number of slots currently holding a turn, whether `Running` or `Cancelling`.
    pub fn in_flight_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.lease.is_some())
            .count()
    }

    /// Number of turns waiting for a free slot.
    pub fn queued_count(&self) -> usize {
        self.queue.len()
    }

    /// Queue `turn` for scheduling and return the effects it produced.
    ///
    /// If a free slot exists the turn is dispatched immediately; otherwise it
    /// waits at the tail of the FIFO queue. A duplicate `request_id` (already in
    /// flight or already queued) is rejected, and a zero-capacity pool refuses to
    /// queue anything.
    pub fn enqueue(&mut self, turn: PoolTurn) -> Vec<PoolEffect> {
        if self.slots.is_empty() {
            return vec![PoolEffect::RejectStale {
                slot: None,
                request_id: turn.request_id,
                reason: StaleReason::NoCapacity,
            }];
        }
        if self
            .slots
            .iter()
            .filter_map(|slot| slot.lease.as_ref())
            .any(|lease| lease.request_id == turn.request_id)
            || self
                .queue
                .iter()
                .any(|queued| queued.request_id == turn.request_id)
        {
            return vec![PoolEffect::RejectDuplicate {
                request_id: turn.request_id,
            }];
        }
        self.queue.push_back(turn);
        let mut effects = Vec::new();
        self.dispatch_ready(&mut effects);
        effects
    }

    /// Report a correlated terminal completion.
    ///
    /// The event must match the exact slot lease (index, generation, `request_id`
    /// and `account`). A completion is a safe terminal either while `Running` or
    /// while `Cancelling`; in both cases the slot is freed and the next waiting
    /// turn is leased. Any mismatch is rejected as stale.
    pub fn complete(&mut self, event: SlotEvent) -> Vec<PoolEffect> {
        let lease = match self.validate(&event) {
            Ok(lease) => lease.clone(),
            Err(reason) => {
                return vec![PoolEffect::RejectStale {
                    slot: Some(event.slot),
                    request_id: event.request_id,
                    reason,
                }];
            }
        };
        self.slots[event.slot.index].lease = None;
        let leased = LeasedTurn {
            slot: event.slot,
            request_id: lease.request_id,
            account: lease.account,
        };
        let mut effects = vec![PoolEffect::Complete(leased)];
        self.dispatch_ready(&mut effects);
        effects
    }

    /// Request cancellation of the in-flight turn correlated by `event`.
    ///
    /// Transitions `Running` -> `Cancelling` and emits [`PoolEffect::CancelRequest`],
    /// but does not free the slot or dispatch the next turn. Duplicate cancel
    /// requests for the same lease are idempotent: they re-emit the request without
    /// freeing anything.
    pub fn cancel(&mut self, event: SlotEvent) -> Vec<PoolEffect> {
        let lease = match self.validate(&event) {
            Ok(lease) => lease.clone(),
            Err(reason) => {
                return vec![PoolEffect::RejectStale {
                    slot: Some(event.slot),
                    request_id: event.request_id,
                    reason,
                }];
            }
        };
        self.slots[event.slot.index]
            .lease
            .as_mut()
            .expect("validated lease")
            .phase = SlotPhase::Cancelling;
        vec![PoolEffect::CancelRequest(LeasedTurn {
            slot: event.slot,
            request_id: lease.request_id,
            account: lease.account,
        })]
    }

    /// Acknowledge that the WebView finished draining a requested cancellation.
    ///
    /// Only valid for a lease in `Cancelling`: frees the slot and dispatches the
    /// next FIFO turn. An acknowledgment for a slot that never requested a cancel
    /// is rejected as stale.
    pub fn cancel_ack(&mut self, event: SlotEvent) -> Vec<PoolEffect> {
        let lease = match self.validate(&event) {
            Ok(lease) => lease.clone(),
            Err(reason) => {
                return vec![PoolEffect::RejectStale {
                    slot: Some(event.slot),
                    request_id: event.request_id,
                    reason,
                }];
            }
        };
        if lease.phase != SlotPhase::Cancelling {
            return vec![PoolEffect::RejectStale {
                slot: Some(event.slot),
                request_id: event.request_id,
                reason: StaleReason::NotCancelling,
            }];
        }
        self.slots[event.slot.index].lease = None;
        let leased = LeasedTurn {
            slot: event.slot,
            request_id: lease.request_id,
            account: lease.account,
        };
        let mut effects = vec![PoolEffect::CancelAck(leased)];
        self.dispatch_ready(&mut effects);
        effects
    }

    /// Cancel a still-queued turn by `request_id` without touching in-flight turns.
    ///
    /// Queued turns have no slot lease, so this is keyed on `request_id` only.
    pub fn cancel_queued(&mut self, request_id: &str) -> Vec<PoolEffect> {
        if let Some(pos) = self
            .queue
            .iter()
            .position(|turn| turn.request_id == request_id)
        {
            let turn = self
                .queue
                .remove(pos)
                .expect("position came from the same queue scan");
            return vec![PoolEffect::CancelQueued(turn)];
        }
        vec![PoolEffect::RejectStale {
            slot: None,
            request_id: request_id.to_owned(),
            reason: StaleReason::NotInFlight,
        }]
    }

    /// Validate that `event` matches the pool's ground-truth slot lease.
    fn validate(&self, event: &SlotEvent) -> Result<&Lease, StaleReason> {
        let slot_state = self
            .slots
            .get(event.slot.index)
            .ok_or(StaleReason::UnknownSlot)?;
        let lease = slot_state.lease.as_ref().ok_or(StaleReason::NotInFlight)?;
        if lease.generation != event.slot.generation {
            return Err(StaleReason::GenerationMismatch);
        }
        if lease.request_id != event.request_id {
            return Err(StaleReason::WrongRequest);
        }
        if lease.account != event.account {
            return Err(StaleReason::WrongAccount);
        }
        Ok(lease)
    }

    /// Lease the earliest waiting turn to the earliest free slot.
    ///
    /// Only slots with no lease (neither `Running` nor `Cancelling`) are eligible,
    /// so a draining slot is never redispatched early.
    fn dispatch_ready(&mut self, effects: &mut Vec<PoolEffect>) {
        while !self.queue.is_empty() {
            let Some(free) = self.slots.iter().position(|slot| slot.lease.is_none()) else {
                break;
            };
            let turn = self.queue.pop_front().expect("queue checked non-empty");
            self.slots[free].generation += 1;
            let generation = self.slots[free].generation;
            let leased = LeasedTurn {
                slot: Slot {
                    index: free,
                    generation,
                },
                request_id: turn.request_id.clone(),
                account: turn.account.clone(),
            };
            self.slots[free].lease = Some(Lease {
                generation,
                request_id: turn.request_id,
                account: turn.account,
                phase: SlotPhase::Running,
            });
            effects.push(PoolEffect::Dispatch(leased));
        }
    }
}

impl Default for WebGptPoolScheduler {
    fn default() -> Self {
        Self::new(DEFAULT_SLOT_COUNT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(request_id: &str, account: Option<&str>) -> PoolTurn {
        PoolTurn {
            request_id: request_id.to_owned(),
            account: account.map(str::to_owned),
        }
    }

    /// Extract the single dispatch lease from a ready-queue result.
    fn dispatch_turn(effects: &[PoolEffect]) -> LeasedTurn {
        match effects {
            [PoolEffect::Dispatch(leased)] => leased.clone(),
            other => panic!("expected a single dispatch effect, got: {other:?}"),
        }
    }

    fn completion(leased: &LeasedTurn) -> SlotEvent {
        SlotEvent {
            slot: leased.slot,
            request_id: leased.request_id.clone(),
            account: leased.account.clone(),
        }
    }

    #[test]
    fn two_turns_complete_in_reverse_order_with_two_slots() {
        let mut pool = WebGptPoolScheduler::new(2);
        assert_eq!(pool.slot_count(), 2);

        let alice = dispatch_turn(&pool.enqueue(turn("req-a", Some("account-a"))));
        let bob = dispatch_turn(&pool.enqueue(turn("req-b", Some("account-b"))));
        assert_eq!(alice.slot.index, 0);
        assert_eq!(alice.slot.generation, 1);
        assert_eq!(bob.slot.index, 1);
        assert_eq!(bob.slot.generation, 1);
        assert_eq!(pool.in_flight_count(), 2);
        assert_eq!(pool.queued_count(), 0);

        // Bob finishes first; Alice is unaffected and can finish later.
        assert_eq!(
            pool.complete(completion(&bob)),
            vec![PoolEffect::Complete(bob)]
        );
        assert_eq!(pool.in_flight_count(), 1);
        assert_eq!(
            pool.complete(completion(&alice)),
            vec![PoolEffect::Complete(alice)]
        );
        assert_eq!(pool.in_flight_count(), 0);
    }

    #[test]
    fn delayed_cancel_does_not_dispatch_queued_turn() {
        let mut pool = WebGptPoolScheduler::new(1);
        let first = dispatch_turn(&pool.enqueue(turn("req-a", None)));
        pool.enqueue(turn("req-c", None));

        // Requesting a cancel moves the slot to Cancelling but keeps it occupied,
        // so the queued turn is not submitted into the draining WebView.
        assert_eq!(
            pool.cancel(completion(&first)),
            vec![PoolEffect::CancelRequest(first.clone())]
        );
        assert_eq!(pool.in_flight_count(), 1);
        assert_eq!(pool.queued_count(), 1);
    }

    #[test]
    fn cancel_acknowledgment_dispatches_queued_turn() {
        let mut pool = WebGptPoolScheduler::new(1);
        let first = dispatch_turn(&pool.enqueue(turn("req-a", None)));
        pool.enqueue(turn("req-c", None));
        pool.cancel(completion(&first));

        // Only the correlated acknowledgment frees the slot and runs the next turn.
        let effects = pool.cancel_ack(completion(&first));
        assert_eq!(effects[0], PoolEffect::CancelAck(first.clone()));
        let next = dispatch_turn(&effects[1..]);
        assert_eq!(next.request_id, "req-c");
        assert_eq!(next.slot.index, first.slot.index);
        assert_eq!(next.slot.generation, first.slot.generation + 1);
        assert_eq!(pool.in_flight_count(), 1);
        assert_eq!(pool.queued_count(), 0);
    }

    #[test]
    fn cancellation_is_isolated_to_the_requested_turn() {
        let mut pool = WebGptPoolScheduler::new(2);
        let alice = dispatch_turn(&pool.enqueue(turn("req-a", None)));
        let bob = dispatch_turn(&pool.enqueue(turn("req-b", None)));
        assert_eq!(pool.in_flight_count(), 2);

        // Requesting a cancel on Alice keeps her slot occupied and leaves Bob
        // untouched; nothing is freed or dispatched.
        assert_eq!(
            pool.cancel(completion(&alice)),
            vec![PoolEffect::CancelRequest(alice.clone())]
        );
        assert_eq!(pool.in_flight_count(), 2);
        assert_eq!(pool.queued_count(), 0);

        // The acknowledgment frees only Alice's drained slot.
        assert_eq!(
            pool.cancel_ack(completion(&alice)),
            vec![PoolEffect::CancelAck(alice.clone())]
        );
        assert_eq!(pool.in_flight_count(), 1);

        // Bob can still reach terminal completion normally.
        assert_eq!(
            pool.complete(completion(&bob)),
            vec![PoolEffect::Complete(bob)]
        );
        assert_eq!(pool.in_flight_count(), 0);
    }

    #[test]
    fn completion_arriving_while_cancelling_is_a_safe_terminal() {
        let mut pool = WebGptPoolScheduler::new(1);
        let first = dispatch_turn(&pool.enqueue(turn("req-a", None)));
        pool.enqueue(turn("req-c", None));
        pool.cancel(completion(&first));

        // A natural completion while Cancelling is a safe terminal that frees the
        // slot and schedules the next turn.
        let effects = pool.complete(completion(&first));
        assert_eq!(effects[0], PoolEffect::Complete(first.clone()));
        let next = dispatch_turn(&effects[1..]);
        assert_eq!(next.request_id, "req-c");
        assert_eq!(pool.in_flight_count(), 1);
        assert_eq!(pool.queued_count(), 0);
    }

    #[test]
    fn duplicate_cancel_request_is_idempotent() {
        let mut pool = WebGptPoolScheduler::new(1);
        let first = dispatch_turn(&pool.enqueue(turn("req-a", None)));
        pool.enqueue(turn("req-c", None));

        // A second cancel request while already Cancelling is deterministic and
        // does not double-free or redispatch.
        assert_eq!(
            pool.cancel(completion(&first)),
            vec![PoolEffect::CancelRequest(first.clone())]
        );
        assert_eq!(
            pool.cancel(completion(&first)),
            vec![PoolEffect::CancelRequest(first.clone())]
        );
        assert_eq!(pool.in_flight_count(), 1);
        assert_eq!(pool.queued_count(), 1);

        // One acknowledgment is still required to free the slot.
        let effects = pool.cancel_ack(completion(&first));
        assert_eq!(effects[0], PoolEffect::CancelAck(first.clone()));
        let next = dispatch_turn(&effects[1..]);
        assert_eq!(next.request_id, "req-c");
        assert_eq!(pool.in_flight_count(), 1);
    }

    #[test]
    fn late_cancel_ack_after_reuse_is_rejected_by_generation() {
        let mut pool = WebGptPoolScheduler::new(1);
        let first = dispatch_turn(&pool.enqueue(turn("req-a", None)));
        pool.enqueue(turn("req-c", None));
        pool.cancel(completion(&first));
        let effects = pool.cancel_ack(completion(&first));
        let next = dispatch_turn(&effects[1..]);
        assert_eq!(next.slot.index, first.slot.index);
        assert_eq!(next.slot.generation, first.slot.generation + 1);

        // A late acknowledgment for the reused slot carries the old generation and
        // is rejected, so it cannot free the new lease.
        assert_eq!(
            pool.cancel_ack(completion(&first)),
            vec![PoolEffect::RejectStale {
                slot: Some(first.slot),
                request_id: first.request_id,
                reason: StaleReason::GenerationMismatch,
            }]
        );
        assert_eq!(pool.in_flight_count(), 1);
    }

    #[test]
    fn cancel_ack_requires_prior_cancel_request() {
        let mut pool = WebGptPoolScheduler::new(1);
        let first = dispatch_turn(&pool.enqueue(turn("req-a", None)));

        // An acknowledgment for a slot that never requested a cancel is rejected;
        // the turn is still Running and stays in flight.
        assert_eq!(
            pool.cancel_ack(completion(&first)),
            vec![PoolEffect::RejectStale {
                slot: Some(first.slot),
                request_id: first.request_id.clone(),
                reason: StaleReason::NotCancelling,
            }]
        );
        assert_eq!(pool.in_flight_count(), 1);

        // The turn can still finish normally.
        assert_eq!(
            pool.complete(completion(&first)),
            vec![PoolEffect::Complete(first)]
        );
    }

    #[test]
    fn old_generation_is_rejected_when_request_id_is_reused() {
        let mut pool = WebGptPoolScheduler::new(1);
        let first = dispatch_turn(&pool.enqueue(turn("req-a", Some("account-a"))));
        assert_eq!(
            pool.complete(completion(&first)),
            vec![PoolEffect::Complete(first.clone())]
        );

        // A completed request id may be deliberately reused by a higher layer.
        // The new dispatch receives a new generation on the same slot.
        let second = dispatch_turn(&pool.enqueue(turn("req-a", Some("account-a"))));
        assert_eq!(second.slot.index, first.slot.index);
        assert_ne!(second.slot.generation, first.slot.generation);

        // The old callback must not terminate the new lease, even though account,
        // request id, and slot index are otherwise identical.
        assert_eq!(
            pool.complete(completion(&first)),
            vec![PoolEffect::RejectStale {
                slot: Some(first.slot),
                request_id: first.request_id,
                reason: StaleReason::GenerationMismatch,
            }]
        );
        assert_eq!(pool.in_flight_count(), 1);
        assert_eq!(
            pool.complete(completion(&second)),
            vec![PoolEffect::Complete(second)]
        );
    }

    #[test]
    fn event_for_wrong_slot_is_rejected() {
        let mut pool = WebGptPoolScheduler::new(2);
        let alice = dispatch_turn(&pool.enqueue(turn("req-a", None)));
        let bob = dispatch_turn(&pool.enqueue(turn("req-b", None)));

        // "req-a" lives on slot 0, but the event nominates slot 1 (which holds
        // "req-b") -> request/slot mismatch.
        let wrong = SlotEvent {
            slot: bob.slot,
            request_id: alice.request_id,
            account: None,
        };
        assert_eq!(
            pool.complete(wrong.clone()),
            vec![PoolEffect::RejectStale {
                slot: Some(wrong.slot),
                request_id: wrong.request_id,
                reason: StaleReason::WrongRequest,
            }]
        );
        assert_eq!(pool.in_flight_count(), 2);

        // A slot index outside the pool is also rejected.
        let out_of_range = SlotEvent {
            slot: Slot {
                index: 99,
                generation: 1,
            },
            request_id: "req-a".to_owned(),
            account: None,
        };
        assert_eq!(
            pool.complete(out_of_range.clone()),
            vec![PoolEffect::RejectStale {
                slot: Some(out_of_range.slot),
                request_id: out_of_range.request_id,
                reason: StaleReason::UnknownSlot,
            }]
        );
    }

    #[test]
    fn event_for_wrong_account_is_rejected() {
        let mut pool = WebGptPoolScheduler::new(1);
        let alice = dispatch_turn(&pool.enqueue(turn("req-a", Some("alice"))));

        // Same slot + request, but a different account -> correlation mismatch.
        let wrong_account = SlotEvent {
            slot: alice.slot,
            request_id: alice.request_id.clone(),
            account: Some("bob".to_owned()),
        };
        assert_eq!(
            pool.complete(wrong_account.clone()),
            vec![PoolEffect::RejectStale {
                slot: Some(wrong_account.slot),
                request_id: wrong_account.request_id,
                reason: StaleReason::WrongAccount,
            }]
        );
        assert_eq!(pool.in_flight_count(), 1);
    }

    #[test]
    fn one_slot_preserves_strict_fifo_order() {
        let mut pool = WebGptPoolScheduler::new(1);
        let first = dispatch_turn(&pool.enqueue(turn("req-1", None)));
        assert_eq!(pool.enqueue(turn("req-2", None)), Vec::new());
        assert_eq!(pool.enqueue(turn("req-3", None)), Vec::new());
        assert_eq!(pool.in_flight_count(), 1);
        assert_eq!(pool.queued_count(), 2);

        // Completing the single in-flight turn dispatches the next in FIFO order.
        let effects = pool.complete(completion(&first));
        assert_eq!(effects[0], PoolEffect::Complete(first.clone()));
        let second = dispatch_turn(&effects[1..]);
        assert_eq!(second.request_id, "req-2");
        assert_eq!(second.slot.index, first.slot.index);
        assert_eq!(second.slot.generation, 2);

        let effects = pool.complete(completion(&second));
        assert_eq!(effects[0], PoolEffect::Complete(second.clone()));
        let third = dispatch_turn(&effects[1..]);
        assert_eq!(third.request_id, "req-3");
        assert_eq!(pool.in_flight_count(), 1);
        assert_eq!(pool.queued_count(), 0);
    }

    #[test]
    fn queued_cancel_does_not_affect_in_flight_turns() {
        let mut pool = WebGptPoolScheduler::new(1);
        let first = dispatch_turn(&pool.enqueue(turn("req-a", None)));
        pool.enqueue(turn("req-b", None));
        pool.enqueue(turn("req-c", None));
        assert_eq!(pool.in_flight_count(), 1);
        assert_eq!(pool.queued_count(), 2);

        // Removing queued "req-c" only drops that waiter.
        assert_eq!(
            pool.cancel_queued("req-c"),
            vec![PoolEffect::CancelQueued(turn("req-c", None))]
        );
        assert_eq!(pool.in_flight_count(), 1);
        assert_eq!(pool.queued_count(), 1);
        assert_eq!(
            pool.cancel_queued("req-b"),
            vec![PoolEffect::CancelQueued(turn("req-b", None))]
        );
        assert_eq!(pool.queued_count(), 0);

        // "req-a" is still in flight and untouched.
        assert_eq!(
            pool.complete(completion(&first)),
            vec![PoolEffect::Complete(first)]
        );
    }

    #[test]
    fn duplicate_enqueue_is_rejected_to_prevent_replay() {
        let mut pool = WebGptPoolScheduler::new(2);
        dispatch_turn(&pool.enqueue(turn("req-a", None)));
        assert_eq!(
            pool.enqueue(turn("req-a", None)),
            vec![PoolEffect::RejectDuplicate {
                request_id: "req-a".to_owned()
            }]
        );
        assert_eq!(pool.in_flight_count(), 1);
    }

    #[test]
    fn zero_slots_is_an_explicit_no_capacity_state() {
        let mut pool = WebGptPoolScheduler::new(0);
        assert_eq!(pool.slot_count(), 0);
        assert_eq!(pool.in_flight_count(), 0);
        assert_eq!(pool.queued_count(), 0);

        // Enqueue is refused rather than silently promoted to one slot.
        assert_eq!(
            pool.enqueue(turn("req-a", None)),
            vec![PoolEffect::RejectStale {
                slot: None,
                request_id: "req-a".to_owned(),
                reason: StaleReason::NoCapacity,
            }]
        );
        assert_eq!(pool.queued_count(), 0);

        // Any terminal event is rejected as an unknown slot.
        let event = SlotEvent {
            slot: Slot {
                index: 0,
                generation: 1,
            },
            request_id: "req-a".to_owned(),
            account: None,
        };
        assert_eq!(
            pool.complete(event.clone()),
            vec![PoolEffect::RejectStale {
                slot: Some(event.slot),
                request_id: event.request_id,
                reason: StaleReason::UnknownSlot,
            }]
        );
    }
}
