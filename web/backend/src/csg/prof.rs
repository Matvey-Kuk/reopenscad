//! Opt-in phase timers for the exact kernel.
//!
//! The kernel's cost is spread over a handful of named phases (BSP build,
//! polygon clipping, face merging, 2D triangulation, hull construction) and a
//! wall-clock total tells you nothing about which of them dominates. These
//! counters accumulate per-phase time and call counts into thread-local
//! storage so a release-mode harness can print a breakdown.
//!
//! The timers are inert unless `REOPENSCAD_PROFILE` is set in the environment,
//! so the server pays only one relaxed atomic load per instrumented scope.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

static ENABLED: AtomicU8 = AtomicU8::new(0);

fn enabled() -> bool {
    match ENABLED.load(Ordering::Relaxed) {
        0 => {
            let on = std::env::var_os("REOPENSCAD_PROFILE").is_some();
            ENABLED.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
        2 => true,
        _ => false,
    }
}

thread_local! {
    static COUNTERS: RefCell<Vec<(&'static str, u128, u64)>> = const { RefCell::new(Vec::new()) };
}

fn record(name: &'static str, nanos: u128) {
    COUNTERS.with(|counters| {
        let mut counters = counters.borrow_mut();
        if let Some(entry) = counters.iter_mut().find(|entry| entry.0 == name) {
            entry.1 += nanos;
            entry.2 += 1;
        } else {
            counters.push((name, nanos, 1));
        }
    });
}

/// Adds `count` to a named tally without timing it.
pub fn count(name: &'static str, amount: u64) {
    if !enabled() {
        return;
    }
    COUNTERS.with(|counters| {
        let mut counters = counters.borrow_mut();
        if let Some(entry) = counters.iter_mut().find(|entry| entry.0 == name) {
            entry.2 += amount;
        } else {
            counters.push((name, 0, amount));
        }
    });
}

/// Times `body` under `name`. Nested scopes each get their own total, so the
/// numbers overlap; read them as inclusive times.
pub fn scope<T>(name: &'static str, body: impl FnOnce() -> T) -> T {
    if !enabled() {
        return body();
    }
    let start = Instant::now();
    let value = body();
    record(name, start.elapsed().as_nanos());
    value
}

/// Prints the accumulated breakdown for the calling thread, then clears it.
pub fn report() {
    if !enabled() {
        return;
    }
    COUNTERS.with(|counters| {
        let mut counters = counters.borrow_mut();
        counters.sort_by(|a, b| b.1.cmp(&a.1));
        println!("--- kernel phase profile (inclusive) ---");
        for (name, nanos, calls) in counters.iter() {
            println!(
                "{name:<34} {:>10.3} ms  calls={calls}",
                *nanos as f64 / 1e6
            );
        }
        counters.clear();
    });
}
