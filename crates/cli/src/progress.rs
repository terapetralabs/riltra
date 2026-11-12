// SPDX-License-Identifier: Apache-2.0
//! Opt-in progress lines for long `scan` and `index` runs.
//!
//! Progress is written to stderr only, and only after `enable()` (the
//! `--progress` flag). When disabled every function here is a no-op, so
//! stdout, stderr and exit codes stay unchanged without the flag. Lines are
//! written live at most once every two seconds, so a fast run prints only its
//! final `done` line.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// shortcut: process-global state avoids threading a reporter through every
// scan and index helper. Upgrade to an explicit reporter argument if scan or
// index ever run concurrently in one process.
static ENABLED: AtomicBool = AtomicBool::new(false);
static STATE: OnceLock<Mutex<State>> = OnceLock::new();

const PRINT_INTERVAL: Duration = Duration::from_secs(2);

struct State {
    label: String,
    started: Instant,
    last_print: Instant,
    count: u64,
}

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| {
        let now = Instant::now();
        Mutex::new(State {
            label: String::new(),
            started: now,
            last_print: now,
            count: 0,
        })
    })
}

/// Turns progress reporting on for the rest of this process.
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Resets the counter and timers for a new run labelled `label`.
pub fn start(label: &str) {
    if !is_enabled() {
        return;
    }
    let mut s = state().lock().expect("progress state lock");
    let now = Instant::now();
    s.label = label.to_string();
    s.started = now;
    s.last_print = now;
    s.count = 0;
}

/// Counts one parsed file and prints a line if two seconds passed since the
/// last print.
pub fn tick() {
    if !is_enabled() {
        return;
    }
    let mut s = state().lock().expect("progress state lock");
    s.count += 1;
    let now = Instant::now();
    if now.duration_since(s.last_print) >= PRINT_INTERVAL {
        s.last_print = now;
        let secs = now.duration_since(s.started).as_secs();
        let line = format!(
            "progress: {}: {} files parsed, {}s elapsed",
            s.label, s.count, secs
        );
        write_stderr(&line);
    }
}

/// Prints the final line for the current run.
pub fn finish() {
    if !is_enabled() {
        return;
    }
    let s = state().lock().expect("progress state lock");
    let secs = Instant::now().duration_since(s.started).as_secs();
    let line = format!(
        "progress: {}: done, {} files parsed in {}s",
        s.label, s.count, secs
    );
    write_stderr(&line);
}

fn write_stderr(line: &str) {
    // Progress is diagnostic only: a failed stderr write must not change the
    // command's result, so the error is ignored on purpose.
    let _ = writeln!(std::io::stderr(), "{line}");
}
