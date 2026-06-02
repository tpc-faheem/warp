//! Executor for `AIAgentActionType::WaitForEvents` (QUALITY-780 §10).
//!
//! The wait action is synthesized from the public
//! `Message::ToolCall::WaitForEvents` tool call by the inbound conversion
//! path (`convert_from.rs`). Once the action lands in the action model it
//! is dispatched here.
//!
//! Responsibilities:
//! - Schedule a watchdog timer using the client-side safety margin
//!   described in `specs/QUALITY-780/TECH.md` §10 ("Client-side safety
//!   margin"). The effective timeout subtracts a defensive margin from
//!   the server-supplied `idle_timeout_seconds` so the client closes the
//!   wait before the worker-side idle-shutdown safety net fires; if the
//!   stamped value is too small, falls back to a hard floor so the
//!   watchdog still fires.
//! - Track a per-conversation generation counter so a freshly enqueued
//!   `WaitForEvents` action supersedes any prior pending fire on the
//!   same conversation.
//! - Transition the conversation to `ConversationStatus::WaitingForEvents`
//!   inside `execute` (before returning `Async`), so downstream
//!   subscribers (driver, task-sync, notifications, pill bar) re-evaluate
//!   immediately rather than seeing the stale `InProgress` state.
//! - Expose `complete_wait_action` so an inbound resume (a generic
//!   `Cancel` or server-echoed `WaitForEventsResult` referencing the
//!   waiting tool-call id) can close out the action without waiting for
//!   the watchdog.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::FutureExt;
use warpui::{Entity, EntityId, ModelContext, SingletonEntity};

use super::{ActionExecution, AnyActionExecution, ExecuteActionInput, PreprocessActionInput};
use crate::ai::agent::conversation::{AIConversationId, ConversationStatus};
use crate::ai::agent::{AIAgentActionResultType, AIAgentActionType, WaitForEventsResult};
use crate::ai::blocklist::{BlocklistAIHistoryEvent, BlocklistAIHistoryModel};

/// Default upper bound for the wait watchdog when the server-supplied
/// `idle_timeout_seconds` is unset (`0` per the prost convention for flat
/// scalars). 30 minutes; chosen to roughly mirror the existing
/// server-side `VMIdleTimeoutMinutes` tenant default so the client
/// watchdog and the worker-side safety net stay in the same ballpark
/// when neither is configured.
pub(crate) const DEFAULT_ORCHESTRATED_IDLE_TIMEOUT_SECONDS: i32 = 30 * 60;

/// Defensive margin the client subtracts from the server-supplied
/// `idle_timeout_seconds` before scheduling the watchdog. This protects
/// against (a) the server-side margin not being deployed yet, (b)
/// modest clock skew between the worker VM and the local client, and
/// (c) the latency budget for the recovery cycle "client watchdog fires
/// → executor synthesizes `WaitForEventsResult` → `FinishedAction`
/// → controller auto-follow-up → outbound request → server
/// `BeginTaskProgress` → next agent turn starts producing activity".
/// See `specs/QUALITY-780/TECH.md` §10 "Client-side safety margin".
pub(crate) const CLIENT_WATCHDOG_SAFETY_MARGIN: Duration = Duration::from_secs(30);

/// Hard floor applied after subtracting [`CLIENT_WATCHDOG_SAFETY_MARGIN`].
/// If the server-supplied timeout is already below the margin the floor
/// keeps the watchdog firing on a sane schedule (e.g. tiny test values
/// like `5s`) instead of immediately on schedule.
pub(crate) const HARD_FLOOR: Duration = Duration::from_secs(5);

/// Compute the actual watchdog timeout for a server-supplied
/// `idle_timeout_seconds`, applying the client-side safety margin and
/// the hard floor described in `specs/QUALITY-780/TECH.md` §10.
///
/// Negative values are clamped to the default for safety; the default
/// is also used when the prost scalar comes in as `0` ("unset"). The
/// returned duration is always at least [`HARD_FLOOR`].
pub(crate) fn watchdog_timeout_for_stamped_seconds(stamped_seconds: i32) -> Duration {
    let seconds = if stamped_seconds <= 0 {
        DEFAULT_ORCHESTRATED_IDLE_TIMEOUT_SECONDS
    } else {
        stamped_seconds
    };
    let stamped = Duration::from_secs(seconds as u64);
    stamped
        .checked_sub(CLIENT_WATCHDOG_SAFETY_MARGIN)
        .filter(|d| *d >= HARD_FLOOR)
        .unwrap_or(HARD_FLOOR)
}

/// In-flight `WaitForEvents` action state tracked by the executor.
///
/// Only one wait is in flight per conversation; a fresh enqueue
/// supersedes the previous wait by bumping the
/// `conversation_generation` counter so the old watchdog closure
/// no-ops when it eventually fires.
struct PendingWait {
    tool_call_id: String,
    /// Sender for the channel awaited by the spawned action future.
    /// Both the watchdog and `complete_wait_action` send on this
    /// channel to drive completion through the action model.
    sender: async_channel::Sender<WaitForEventsResult>,
}

pub struct WaitForEventsExecutor {
    /// Terminal view this executor is associated with — needed to
    /// transition the conversation status via
    /// `BlocklistAIHistoryModel::update_conversation_status`.
    terminal_view_id: EntityId,
    /// Per-conversation generation counter. Bumped on every fresh
    /// `execute()` so superseded watchdogs (whose closure captured an
    /// older generation) no-op when they fire.
    conversation_generation: HashMap<AIConversationId, Arc<AtomicUsize>>,
    /// In-flight waits keyed by conversation id (only one wait per
    /// conversation at a time).
    pending: HashMap<AIConversationId, PendingWait>,
}

impl WaitForEventsExecutor {
    pub fn new(terminal_view_id: EntityId, ctx: &mut ModelContext<Self>) -> Self {
        // Subscribe to history events so the executor can react when an
        // inbound `Cancel` or `WaitForEventsResult` arrives and the
        // history model transitions the conversation out of
        // `WaitingForEvents` via
        // `clear_conversation_waiting_for_events_if_matches`. The
        // executor uses the status transition as the signal to send
        // `Completed` on the pending wait's channel, which drives the
        // action through the normal `FinishedAction` path (and in turn
        // triggers the controller's auto-follow-up subscriber).
        let history_model = BlocklistAIHistoryModel::handle(ctx);
        ctx.subscribe_to_model(&history_model, Self::handle_history_event);

        Self {
            terminal_view_id,
            conversation_generation: HashMap::new(),
            pending: HashMap::new(),
        }
    }

    fn handle_history_event(
        &mut self,
        event: &BlocklistAIHistoryEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        let BlocklistAIHistoryEvent::UpdatedConversationStatus {
            terminal_view_id,
            conversation_id,
            ..
        } = event
        else {
            return;
        };
        if *terminal_view_id != self.terminal_view_id {
            return;
        }
        let Some(pending) = self.pending.get(conversation_id) else {
            return;
        };
        let tool_call_id = pending.tool_call_id.clone();
        // Only complete the wait when the conversation has actually
        // transitioned out of `WaitingForEvents`. A stale event for a
        // conversation that is still waiting (e.g. status touched but
        // not changed) must not cancel the in-flight watchdog.
        let history_model = BlocklistAIHistoryModel::as_ref(ctx);
        let Some(conversation) = history_model.conversation(conversation_id) else {
            return;
        };
        if matches!(conversation.status(), ConversationStatus::WaitingForEvents) {
            return;
        }
        let conversation_id = *conversation_id;
        self.complete_wait_action(
            conversation_id,
            &tool_call_id,
            WaitForEventsResult::Completed,
        );
    }

    pub(super) fn should_autoexecute(
        &self,
        _input: ExecuteActionInput,
        _ctx: &mut ModelContext<Self>,
    ) -> bool {
        // The wait action is synthesized from a server-emitted tool
        // call; it never requires user confirmation.
        true
    }

    pub(super) fn preprocess_action(
        &mut self,
        _action: PreprocessActionInput,
        _ctx: &mut ModelContext<Self>,
    ) -> BoxFuture<'static, ()> {
        futures::future::ready(()).boxed()
    }

    pub(super) fn execute(
        &mut self,
        input: ExecuteActionInput,
        ctx: &mut ModelContext<Self>,
    ) -> impl Into<AnyActionExecution> {
        let AIAgentActionType::WaitForEvents {
            tool_call_id,
            idle_timeout_seconds,
        } = &input.action.action
        else {
            return ActionExecution::InvalidAction;
        };

        let tool_call_id = tool_call_id.clone();
        let conversation_id = input.conversation_id;
        let timeout = watchdog_timeout_for_stamped_seconds(*idle_timeout_seconds);

        // Bump the conversation's generation counter. Capture both the
        // shared counter (so the watchdog closure can re-check on fire)
        // and the new stamp for comparison.
        let generation_counter = self
            .conversation_generation
            .entry(conversation_id)
            .or_default()
            .clone();
        let expected_generation = generation_counter.fetch_add(1, Ordering::SeqCst) + 1;

        // Drop any previous pending entry; the previous waiter's future
        // (if still pending) sees the closed sender and resolves with
        // `Err`, which `on_complete` maps to `Completed` (a harmless
        // no-op since the action_model has long since moved on for
        // that prior wait).
        let (sender, receiver) = async_channel::bounded(1);
        if let Some(prev) = self.pending.insert(
            conversation_id,
            PendingWait {
                tool_call_id: tool_call_id.clone(),
                sender,
            },
        ) {
            // Explicitly drop the prior sender so the previous wait's
            // receiver wakes up. The previous action has already been
            // FinishedAction-ed via the action_model's running_actions
            // bookkeeping; this is just a cleanup detail.
            drop(prev.sender);
        }

        // Transition the conversation into `WaitingForEvents` before
        // returning `Async`. We do this directly via the history model
        // (rather than letting the action_model's
        // `update_conversation_in_progress_status` run) so the status
        // reflects the yield as soon as the action is dispatched.
        let terminal_view_id = self.terminal_view_id;
        let stored_tool_call_id = tool_call_id.clone();
        BlocklistAIHistoryModel::handle(ctx).update(ctx, move |history_model, ctx| {
            history_model.mark_conversation_waiting_for_events(
                conversation_id,
                stored_tool_call_id,
                terminal_view_id,
                ctx,
            );
        });

        // Spawn the watchdog. On fire — if our generation still matches —
        // synthesize a `WaitForEventsResult::Completed` on the channel,
        // which resolves the awaited future and drives the action's
        // completion through the action_model's normal `FinishedAction`
        // path (which in turn triggers the controller's auto-follow-up
        // subscriber).
        let watchdog_tool_call_id = tool_call_id.clone();
        ctx.spawn(
            async move {
                warpui::r#async::Timer::after(timeout).await;
            },
            move |me, (), _ctx| {
                me.fire_watchdog_if_current(
                    conversation_id,
                    &watchdog_tool_call_id,
                    expected_generation,
                    &generation_counter,
                );
            },
        );

        ActionExecution::new_async(async move { receiver.recv().await }, move |result, _ctx| {
            let wait_result = match result {
                Ok(result) => result,
                // The sender was dropped without sending. This
                // happens when a fresh `WaitForEvents` action
                // superseded this one (the prior pending entry was
                // replaced); treat it as a normal completion so the
                // action_model removes it from running_actions and
                // does not block the follow-up request.
                Err(_) => WaitForEventsResult::Completed,
            };
            AIAgentActionResultType::WaitForEvents(wait_result)
        })
    }

    /// Called by `BlocklistAIActionModel::complete_wait_for_events_action_if_running`
    /// when an inbound `Cancel` or server-echoed `WaitForEventsResult`
    /// references the in-flight wait's `tool_call_id`. Sends `result`
    /// on the awaited channel so the action future resolves through
    /// the normal `FinishedAction` path.
    ///
    /// No-op if the supplied `tool_call_id` doesn't match the in-flight
    /// wait (e.g. unrelated `Cancel` for a different tool call) or if
    /// there is no in-flight wait for `conversation_id`.
    pub(crate) fn complete_wait_action(
        &mut self,
        conversation_id: AIConversationId,
        tool_call_id: &str,
        result: WaitForEventsResult,
    ) {
        let Some(pending) = self.pending.get(&conversation_id) else {
            return;
        };
        if pending.tool_call_id != tool_call_id {
            return;
        }
        let Some(pending) = self.pending.remove(&conversation_id) else {
            return;
        };
        let _ = pending.sender.try_send(result);
    }

    /// Called from `BlocklistAIActionExecutor::cancel_running_async_action`
    /// when the running wait action is cancelled by user action. Drops
    /// the in-flight entry so a later watchdog fire is a no-op. The
    /// `FinishedAction` event itself is emitted by the parent
    /// executor's cancel path, which calls
    /// `AIAgentActionType::cancelled_result` to produce
    /// `WaitForEventsResult::Cancelled`.
    pub(crate) fn cancel_execution(&mut self, tool_call_id: &str) {
        let Some((conversation_id, pending)) = self
            .pending
            .iter()
            .find(|(_, pending)| pending.tool_call_id == tool_call_id)
            .map(|(id, _)| *id)
            .and_then(|id| self.pending.remove(&id).map(|p| (id, p)))
        else {
            return;
        };
        // Bump the generation so any in-flight watchdog timer for this
        // conversation observes a stale generation and no-ops on fire.
        if let Some(gen) = self.conversation_generation.get(&conversation_id) {
            gen.fetch_add(1, Ordering::SeqCst);
        }
        // Drop the sender — the awaited receiver wakes with `Err`,
        // mapped to `Completed` by `on_complete`. The parent
        // executor's cancel path has already emitted a `FinishedAction`
        // with `WaitForEventsResult::Cancelled` (via
        // `AIAgentActionType::cancelled_result`), so the duplicate
        // synthesized `Completed` from this branch is suppressed by the
        // `async_executing_actions.remove` check in the spawn callback.
        drop(pending.sender);
    }

    fn fire_watchdog_if_current(
        &mut self,
        conversation_id: AIConversationId,
        tool_call_id: &str,
        expected_generation: usize,
        generation_counter: &Arc<AtomicUsize>,
    ) {
        if generation_counter.load(Ordering::SeqCst) != expected_generation {
            log::debug!(
                "WaitForEventsExecutor: watchdog superseded conversation_id={conversation_id:?} \
                 expected_generation={expected_generation}"
            );
            return;
        }
        let Some(pending) = self.pending.get(&conversation_id) else {
            return;
        };
        if pending.tool_call_id != tool_call_id {
            return;
        }
        let Some(pending) = self.pending.remove(&conversation_id) else {
            return;
        };
        log::info!(
            "WaitForEventsExecutor: watchdog fired conversation_id={conversation_id:?} \
             tool_call_id={tool_call_id}"
        );
        let _ = pending.sender.try_send(WaitForEventsResult::Completed);
    }
}

impl Entity for WaitForEventsExecutor {
    type Event = ();
}

#[cfg(test)]
#[path = "wait_for_events_tests.rs"]
mod tests;
