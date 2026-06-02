//! Unit tests for the pure helpers in
//! `crate::ai::blocklist::action_model::execute::wait_for_events`.
//!
//! Async scheduling and channel-driven completion are exercised via the
//! integration suite — see `specs/QUALITY-780/TECH.md` §10 and the
//! Wave-3 integration test plan.

use std::time::Duration;

use super::{
    watchdog_timeout_for_stamped_seconds, CLIENT_WATCHDOG_SAFETY_MARGIN,
    DEFAULT_ORCHESTRATED_IDLE_TIMEOUT_SECONDS, HARD_FLOOR,
};

#[test]
fn default_idle_timeout_constant_matches_thirty_minutes() {
    // The wait watchdog falls back to this value when the server does not
    // supply an `idle_timeout_seconds` on the `wait_for_events` tool call.
    // 30 minutes is intentionally chosen to roughly mirror the existing
    // server-side `VMIdleTimeoutMinutes` tenant default; document the
    // expectation so a future change to the constant trips this test.
    assert_eq!(DEFAULT_ORCHESTRATED_IDLE_TIMEOUT_SECONDS, 30 * 60);
}

#[test]
fn client_watchdog_safety_margin_is_thirty_seconds() {
    // Margin keeps the recovery cycle (executor → controller follow-up →
    // server BeginTaskProgress → next agent turn) shorter than the
    // worker-side idle ceiling. 30s gives enough headroom for the
    // outbound request to land before the worker shuts down. Trip if the
    // constant moves to verify the relationship to the recovery budget
    // is still documented in `specs/QUALITY-780/TECH.md` §10.
    assert_eq!(CLIENT_WATCHDOG_SAFETY_MARGIN, Duration::from_secs(30));
}

#[test]
fn hard_floor_is_five_seconds() {
    // Tiny stamped values (e.g. integration test cases that pass 5s)
    // should still fire after a finite delay rather than firing
    // immediately. The hard floor keeps the watchdog ticking on a sane
    // schedule even when the safety margin would otherwise consume the
    // entire stamped budget.
    assert_eq!(HARD_FLOOR, Duration::from_secs(5));
}

#[test]
fn watchdog_timeout_subtracts_margin_for_stamped_minute() {
    // A 60s stamped timeout has 30s of headroom after subtracting the
    // safety margin — that's the canonical "happy path" the safety
    // margin is designed for.
    assert_eq!(
        watchdog_timeout_for_stamped_seconds(60),
        Duration::from_secs(30)
    );
}

#[test]
fn watchdog_timeout_clamps_to_hard_floor_when_stamped_value_is_too_small() {
    // A 10s stamped timeout would become negative after subtracting the
    // 30s safety margin — the hard floor kicks in so the watchdog still
    // fires after a finite delay.
    assert_eq!(
        watchdog_timeout_for_stamped_seconds(10),
        HARD_FLOOR,
        "stamped 10s should clamp to HARD_FLOOR after subtracting the safety margin"
    );
}

#[test]
fn watchdog_timeout_falls_back_to_default_minus_margin_when_unset() {
    // Prost flattens scalars, so the proto's "unset" looks like `0` on
    // the Rust side. Per §10, treat that as "use the built-in fallback
    // (default minus margin)".
    let expected = Duration::from_secs(DEFAULT_ORCHESTRATED_IDLE_TIMEOUT_SECONDS as u64)
        - CLIENT_WATCHDOG_SAFETY_MARGIN;
    assert_eq!(watchdog_timeout_for_stamped_seconds(0), expected);
}

#[test]
fn watchdog_timeout_clamps_negative_value_to_default_minus_margin() {
    // Defense against a buggy or malicious payload. `Duration::from_secs`
    // takes a `u64`; a negative value would underflow without the clamp.
    let expected = Duration::from_secs(DEFAULT_ORCHESTRATED_IDLE_TIMEOUT_SECONDS as u64)
        - CLIENT_WATCHDOG_SAFETY_MARGIN;
    assert_eq!(watchdog_timeout_for_stamped_seconds(-42), expected);
}

#[test]
fn watchdog_timeout_preserves_large_stamped_value() {
    // Server-supplied values well above the margin pass through as
    // (stamped - margin). 15 minutes stays at 14m30s after the
    // subtraction.
    assert_eq!(
        watchdog_timeout_for_stamped_seconds(900),
        Duration::from_secs(900) - CLIENT_WATCHDOG_SAFETY_MARGIN
    );
}
