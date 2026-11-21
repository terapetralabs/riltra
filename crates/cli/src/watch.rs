// SPDX-License-Identifier: Apache-2.0
//! Source signature for `riltra watch`: the polled description of what an
//! incremental index run would read. Two polls with equal signatures mean no
//! build is due. It reads only `scan::plan_walk` (ignore rules, symlinks not
//! followed, `.git` skipped), `index::config_fingerprint` and the read-only
//! `git::head_branch`, and it writes nothing.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::git;
use crate::index::{self, Freshness, IndexSummary};
use crate::scan::{self, PlanAction};
use crate::{json, response};

/// What a poll compares. `files` maps a repository-relative path to
/// (length, mtime in nanoseconds since the epoch, 0 when the platform reports none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub files: BTreeMap<String, (u64, u128)>,
    pub config: String,
    pub head: Option<String>,
    pub branch: Option<String>,
}

/// Riltra's own outputs under a home that sits inside the watched root. A build
/// writes them, so counting them would make every build trigger the next one.
pub struct Excluded {
    index_dir: PathBuf,
    knowledge_dir: PathBuf,
    registry: PathBuf,
}

impl Excluded {
    // Maintainer note: the exclusion list must be updated whenever Riltra adds a new directory
    // under RILTRA_HOME, because a build writes there and would otherwise retrigger itself.
    /// `canon_home` must already be canonical; the caller canonicalizes it once.
    pub fn new(canon_home: &Path) -> Excluded {
        Excluded {
            index_dir: canon_home.join("index"),
            knowledge_dir: canon_home.join("knowledge"),
            registry: canon_home.join("registry.json"),
        }
    }

    fn covers(&self, canon_file: &Path) -> bool {
        canon_file.starts_with(&self.index_dir)
            || canon_file.starts_with(&self.knowledge_dir)
            || canon_file == self.registry
    }
}

/// Refuses a root that is the home or sits inside it. Home inside the root is
/// allowed: the signature leaves out Riltra's own outputs instead.
pub fn check_overlap(canon_root: &Path, canon_home: &Path) -> Result<(), String> {
    // Path::starts_with compares whole components, so `/a/bc` is not inside `/a/b`.
    if canon_root.starts_with(canon_home) {
        return Err(format!(
            "watch-root-overlaps-home: {} is the Riltra home or inside it; watch a repository outside it",
            canon_root.display()
        ));
    }
    Ok(())
}

/// The signature of `root` as the next build would see it. Only files the walk
/// would parse count (C6). Only a failed walk (for example, the root vanished)
/// returns `Err`. A file that cannot be resolved or stat'ed for any reason, such
/// as a vanished, locked or access-denied file, is left out, the same as the
/// normal build's unreadable skip (scan.rs, index.rs ingest_file); the next poll
/// reconciles it.
pub fn signature(root: &Path, excluded: &Excluded) -> Result<Signature, String> {
    let (plan, _skips) = scan::plan_walk(root)?;
    let mut files = BTreeMap::new();
    for planned in plan {
        if !matches!(planned.action, PlanAction::Parse(_)) {
            continue;
        }
        let entry = file_entry(
            &planned.full,
            excluded,
            |p| std::fs::canonicalize(p),
            |p| {
                let meta = std::fs::symlink_metadata(p)?;
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                Ok((meta.len(), mtime))
            },
        );
        if let Some(stamp) = entry {
            files.insert(planned.rel, stamp);
        }
    }
    let (head, branch) = git::head_branch(root);
    Ok(Signature {
        files,
        config: index::config_fingerprint(root),
        head,
        branch,
    })
}

/// The signature entry for one planned file, or `None` when the file is left out.
/// `canonicalize` and `stat` are the filesystem calls, passed in so a test can
/// inject a failure. Every error from them leaves the file out, as a vanished
/// file is left out; the caller never fails on a per-file error.
fn file_entry(
    full: &Path,
    excluded: &Excluded,
    canonicalize: impl Fn(&Path) -> std::io::Result<PathBuf>,
    stat: impl Fn(&Path) -> std::io::Result<(u64, u128)>,
) -> Option<(u64, u128)> {
    let canon = canonicalize(full).ok()?;
    if excluded.covers(&canon) {
        return None;
    }
    stat(full).ok()
}

/// Polling and debounce settings for `run`. Defaults: poll every 1000 ms, build
/// after 500 ms of quiet, and build at the latest after 10000 ms of pending change.
/// `max_builds` counts successful builds only (C10).
#[derive(Debug, Clone)]
pub struct Options {
    pub interval_ms: u64,
    pub debounce_ms: u64,
    pub max_wait_ms: u64,
    pub max_builds: Option<u64>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            interval_ms: 1000,
            debounce_ms: 500,
            max_wait_ms: 10_000,
            max_builds: None,
        }
    }
}

/// One watch event before it is rendered as JSON. `run` never emits `Stopped`;
/// the caller emits it when `run` returns an error.
// `Built` carries the summary verbatim, as the contract states; an event is built once per
// successful build, so the size gap between variants costs nothing that matters here.
#[allow(clippy::large_enum_variant)]
pub enum Event {
    Started { status: String },
    Built(IndexSummary),
    Failed { reason: String },
    Busy,
    Stopped { reason: String },
}

/// One event as a single-line JSON object with a fixed key order (C10). `built`
/// nests the index summary verbatim, as `response::index_summary_json` renders it.
pub fn event_json(repo: &str, root: &str, e: &Event) -> String {
    let name = json::escape(repo);
    match e {
        Event::Started { status } => format!(
            "{{\"event\":\"started\",\"repo\":\"{name}\",\"status\":\"{}\"}}",
            json::escape(status)
        ),
        Event::Built(summary) => format!(
            "{{\"event\":\"built\",\"index\":{}}}",
            response::index_summary_json(repo, root, summary)
        ),
        Event::Failed { reason } => format!(
            "{{\"event\":\"failed\",\"repo\":\"{name}\",\"reason\":\"{}\"}}",
            json::escape(reason)
        ),
        Event::Busy => format!("{{\"event\":\"busy\",\"repo\":\"{name}\"}}"),
        Event::Stopped { reason } => format!(
            "{{\"event\":\"stopped\",\"repo\":\"{name}\",\"reason\":\"{}\"}}",
            json::escape(reason)
        ),
    }
}

/// Writes one event line, then a newline, then flushes. A failed write is
/// returned as an error; it never panics (C10).
pub fn write_event(out: &mut dyn Write, line: &str) -> Result<(), String> {
    let written = out
        .write_all(line.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush());
    written.map_err(|e| format!("cannot write watch event: {e}"))
}

/// Everything the loop reads from outside, so a test can script it.
pub trait WatchDeps {
    /// Monotonic milliseconds.
    fn now_ms(&mut self) -> u64;
    fn sleep(&mut self, ms: u64);
    fn signature(&mut self) -> Result<Signature, String>;
    fn build(&mut self) -> Result<IndexSummary, String>;
    fn freshness(&mut self) -> Freshness;
    fn should_stop(&mut self) -> bool;
}

/// The production deps: the real clock, the real filesystem and the real index.
pub struct RealDeps {
    home: PathBuf,
    name: String,
    root: PathBuf,
    excluded: Excluded,
    started: Instant,
}

impl RealDeps {
    pub fn new(home: &Path, name: &str, root: &Path, excluded: Excluded) -> RealDeps {
        RealDeps {
            home: home.to_path_buf(),
            name: name.to_string(),
            root: root.to_path_buf(),
            excluded,
            started: Instant::now(),
        }
    }
}

impl WatchDeps for RealDeps {
    fn now_ms(&mut self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn sleep(&mut self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }

    fn signature(&mut self) -> Result<Signature, String> {
        signature(&self.root, &self.excluded)
    }

    fn build(&mut self) -> Result<IndexSummary, String> {
        index::build(&self.home, &self.name, &self.root, false)
    }

    fn freshness(&mut self) -> Freshness {
        index::evaluate(&self.home, &self.name, &self.root)
    }

    fn should_stop(&mut self) -> bool {
        false
    }
}

/// The debounced watch loop. Returns `Err` only when a signature cannot be taken.
pub fn run(
    opts: &Options,
    deps: &mut dyn WatchDeps,
    emit: &mut dyn FnMut(&Event),
) -> Result<(), String> {
    let mut last = deps.signature()?;
    let status = deps.freshness().status;
    emit(&Event::Started {
        status: status.clone(),
    });
    let mut pending = status != "fresh";
    let start = deps.now_ms();
    let mut quiet_since = start;
    let mut first_pending = pending.then_some(start);
    let mut failures: u32 = 0;
    let mut builds: u64 = 0;
    loop {
        if deps.should_stop() || opts.max_builds == Some(builds) {
            return Ok(());
        }
        // B7: the backoff doubles per consecutive failure, capped at 60 s, and never overflows.
        let shift = failures.min(6);
        deps.sleep(opts.interval_ms.saturating_mul(1u64 << shift).min(60_000));
        let cur = deps.signature()?;
        if cur != last {
            let now = deps.now_ms();
            let first = *first_pending.get_or_insert(now);
            pending = true;
            quiet_since = now;
            last = cur.clone();
            // C9: keep waiting for quiet until the ceiling is reached.
            if now - first < opts.max_wait_ms {
                continue;
            }
        }
        if !pending {
            continue;
        }
        let now = deps.now_ms();
        let quiet = now - quiet_since >= opts.debounce_ms;
        let ceiling = first_pending.is_some_and(|f| now - f >= opts.max_wait_ms);
        if !(quiet || ceiling) {
            continue;
        }
        let before = cur;
        first_pending = None;
        match deps.build() {
            Ok(summary) => {
                emit(&Event::Built(summary));
                builds += 1;
                failures = 0;
                pending = false;
            }
            // C9: a busy retry waits a full debounce again; it never retries on every poll.
            Err(e) if e.starts_with("index-busy:") => {
                emit(&Event::Busy);
                quiet_since = deps.now_ms();
            }
            // C5: a failed build waits for a new change before the next attempt.
            Err(reason) => {
                emit(&Event::Failed { reason });
                failures += 1;
                pending = false;
            }
        }
        // A change that landed during the build differs from `before`, so the next poll sets pending.
        last = before;
    }
}

/// Stops `inner` once an event could not be written, so a closed stdout ends the watch.
struct Guarded<'a> {
    inner: &'a mut dyn WatchDeps,
    failed: &'a Cell<bool>,
}

impl WatchDeps for Guarded<'_> {
    fn now_ms(&mut self) -> u64 {
        self.inner.now_ms()
    }

    fn sleep(&mut self, ms: u64) {
        self.inner.sleep(ms);
    }

    fn signature(&mut self) -> Result<Signature, String> {
        self.inner.signature()
    }

    fn build(&mut self) -> Result<IndexSummary, String> {
        self.inner.build()
    }

    fn freshness(&mut self) -> Freshness {
        self.inner.freshness()
    }

    fn should_stop(&mut self) -> bool {
        self.failed.get() || self.inner.should_stop()
    }
}

/// Runs the loop with `deps`, writes every event to `out`, and returns the exit
/// code (C10): 0 when the loop ends normally, 3 when an event cannot be written
/// or `run` fails. A `run` failure is reported as a final `stopped` event.
pub fn drive_events(
    opts: &Options,
    deps: &mut dyn WatchDeps,
    repo: &str,
    root: &str,
    out: &mut dyn Write,
    err: &mut String,
) -> i32 {
    let failed = Cell::new(false);
    let mut write_error: Option<String> = None;
    let result = {
        let mut guarded = Guarded {
            inner: deps,
            failed: &failed,
        };
        let mut emit = |event: &Event| {
            if write_error.is_some() {
                return;
            }
            if let Err(msg) = write_event(&mut *out, &event_json(repo, root, event)) {
                failed.set(true);
                write_error = Some(msg);
            }
        };
        run(opts, &mut guarded, &mut emit)
    };
    let mut code = 0;
    if let Err(reason) = &result {
        code = 3;
        if write_error.is_none() {
            let stopped = Event::Stopped {
                reason: reason.clone(),
            };
            if let Err(msg) = write_event(out, &event_json(repo, root, &stopped)) {
                err.push_str(&format!("error: {msg}\n"));
            }
        }
        err.push_str(&format!("error: {reason}\n"));
    }
    if let Some(msg) = &write_error {
        code = 3;
        err.push_str(&format!("error: {msg}\n"));
    }
    code
}

#[cfg(test)]
mod tests {
    use super::{check_overlap, file_entry, signature, Excluded, Signature};
    use crate::git;
    use crate::testutil::{git_repo, tmpdir};
    use std::io::Write;
    use std::path::{Path, PathBuf};

    /// A watched root with `a.rs`, and a home outside it.
    fn rooted(tag: &str) -> (PathBuf, Excluded) {
        let root = tmpdir(tag);
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let home = tmpdir(&format!("{tag}-home"));
        std::fs::write(home.join("registry.json"), "{}\n").unwrap();
        std::fs::create_dir_all(home.join("index")).unwrap();
        let excluded = Excluded::new(&home.canonicalize().unwrap());
        (root, excluded)
    }

    /// A home outside the root, for tests that need no home of their own.
    fn outside_home(tag: &str) -> Excluded {
        let home = tmpdir(&format!("{tag}-home"));
        Excluded::new(&home.canonicalize().unwrap())
    }

    fn has(sig: &Signature, name: &str) -> bool {
        sig.files.keys().any(|k| k.ends_with(name))
    }

    fn git(root: &Path, args: &[&str]) {
        let st = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_TERMINAL_PROMPT", "0")
            .status()
            .expect("run git");
        assert!(st.success(), "git {args:?} failed");
    }

    #[test]
    fn signature_detects_add() {
        let (root, ex) = rooted("watch-add");
        let s0 = signature(&root, &ex).unwrap();
        std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();
        let s1 = signature(&root, &ex).unwrap();
        assert_ne!(s0, s1);
        assert!(has(&s1, "b.rs"));
    }

    #[test]
    fn signature_detects_delete() {
        let (root, ex) = rooted("watch-delete");
        let s0 = signature(&root, &ex).unwrap();
        std::fs::remove_file(root.join("a.rs")).unwrap();
        let s1 = signature(&root, &ex).unwrap();
        assert_ne!(s0, s1);
        assert!(!has(&s1, "a.rs"));
    }

    #[test]
    fn signature_detects_rename() {
        let (root, ex) = rooted("watch-rename");
        let s0 = signature(&root, &ex).unwrap();
        std::fs::rename(root.join("a.rs"), root.join("b.rs")).unwrap();
        let s1 = signature(&root, &ex).unwrap();
        assert_ne!(s0, s1);
        assert!(has(&s1, "b.rs") && !has(&s1, "a.rs"));
    }

    #[test]
    fn signature_detects_edit_with_new_length() {
        let (root, ex) = rooted("watch-edit");
        let s0 = signature(&root, &ex).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(root.join("a.rs"))
            .unwrap();
        f.write_all(b"fn more() {}\n").unwrap();
        drop(f);
        let s1 = signature(&root, &ex).unwrap();
        assert_ne!(s0, s1);
    }

    #[test]
    fn signature_detects_ignore_rule_change() {
        let (root, ex) = rooted("watch-rules");
        let s0 = signature(&root, &ex).unwrap();
        std::fs::write(root.join(".riltraignore"), "gen/\n").unwrap();
        let s1 = signature(&root, &ex).unwrap();
        assert_ne!(s0.config, s1.config);
    }

    #[test]
    fn signature_ignores_ignored_paths() {
        let (root, ex) = rooted("watch-ignored");
        std::fs::write(root.join(".riltraignore"), "gen/\n").unwrap();
        let s0 = signature(&root, &ex).unwrap();
        std::fs::create_dir_all(root.join("gen")).unwrap();
        std::fs::write(root.join("gen").join("x.rs"), "fn x() {}\n").unwrap();
        let s1 = signature(&root, &ex).unwrap();
        assert_eq!(s0, s1);
    }

    #[test]
    fn signature_excludes_only_riltra_outputs() {
        let root = tmpdir("watch-home-inside");
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let home = root.join(".rhome");
        std::fs::create_dir_all(home.join("index")).unwrap();
        std::fs::write(home.join("registry.json"), "{}\n").unwrap();
        let ex = Excluded::new(&home.canonicalize().unwrap());
        let s0 = signature(&root, &ex).unwrap();
        // Riltra's own writes: a database, its lock, the knowledge store, the registry.
        // The `.rs` files would be parsed if they were not excluded, so they test the
        // exclusion itself rather than the unsupported-file rule.
        std::fs::write(home.join("index").join("r.db"), b"x").unwrap();
        std::fs::write(home.join("index").join("r.db.lock"), b"").unwrap();
        std::fs::write(home.join("index").join("extra.rs"), "fn e() {}\n").unwrap();
        std::fs::create_dir_all(home.join("knowledge")).unwrap();
        std::fs::write(home.join("knowledge").join("r.db"), b"x").unwrap();
        std::fs::write(home.join("knowledge").join("extra.rs"), "fn k() {}\n").unwrap();
        std::fs::write(home.join("registry.json"), "{\"v\":2}\n").unwrap();
        let s1 = signature(&root, &ex).unwrap();
        assert_eq!(s0, s1);
        // Any other file under home is still part of the build's input.
        std::fs::write(home.join("notes.rs"), "fn n() {}\n").unwrap();
        let s2 = signature(&root, &ex).unwrap();
        assert_ne!(s1, s2);
        assert!(has(&s2, "notes.rs"));
    }

    #[test]
    fn signature_detects_head_change() {
        let Some(root) = git_repo("watch-head", "main") else {
            return;
        };
        let ex = outside_home("watch-head");
        let s0 = signature(&root, &ex).unwrap();
        git(&root, &["commit", "--allow-empty", "-m", "empty"]);
        let s1 = signature(&root, &ex).unwrap();
        assert_eq!(s0.files, s1.files);
        assert_ne!(s0.head, s1.head);
        assert!(s1.head.is_some());
    }

    #[test]
    fn overlap_is_refused() {
        let root = tmpdir("watch-overlap");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let home = root.join(".rhome");
        std::fs::create_dir_all(&home).unwrap();
        let canon_root = root.canonicalize().unwrap();
        let canon_sub = root.join("sub").canonicalize().unwrap();
        let canon_home = home.canonicalize().unwrap();
        let refused = "watch-root-overlaps-home:";
        assert!(check_overlap(&canon_root, &canon_root)
            .unwrap_err()
            .starts_with(refused));
        assert!(check_overlap(&canon_sub, &canon_root)
            .unwrap_err()
            .starts_with(refused));
        assert!(check_overlap(&canon_root, &canon_home).is_ok());
    }

    #[test]
    fn per_file_errors_leave_the_file_out() {
        let ex = outside_home("watch-entry");
        let denied = || std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let gone = || std::io::Error::from(std::io::ErrorKind::NotFound);
        let full = Path::new("a.rs");
        // Locked, denied or otherwise unresolvable: left out, not fatal (the next poll reconciles).
        assert_eq!(
            file_entry(full, &ex, |_| Err(denied()), |_| Ok((1, 2))),
            None
        );
        // Resolves, but its metadata cannot be read: left out too.
        assert_eq!(
            file_entry(full, &ex, |p| Ok(p.to_path_buf()), |_| Err(denied())),
            None
        );
        // Vanished between the walk and the stat: left out, as before.
        assert_eq!(file_entry(full, &ex, |_| Err(gone()), |_| Ok((1, 2))), None);
        assert_eq!(
            file_entry(full, &ex, |p| Ok(p.to_path_buf()), |_| Err(gone())),
            None
        );
        // A readable file keeps its stamp.
        assert_eq!(
            file_entry(full, &ex, |p| Ok(p.to_path_buf()), |_| Ok((1, 2))),
            Some((1, 2))
        );
    }

    #[test]
    fn signature_is_deterministic() {
        let (root, ex) = rooted("watch-deterministic");
        let s0 = signature(&root, &ex).unwrap();
        let s1 = signature(&root, &ex).unwrap();
        assert_eq!(s0, s1);
    }

    #[test]
    fn signature_ignores_unsupported_files() {
        let (root, ex) = rooted("watch-unsupported");
        let s0 = signature(&root, &ex).unwrap();
        std::fs::write(root.join("notes.xyz"), "plain\n").unwrap();
        let s1 = signature(&root, &ex).unwrap();
        assert_eq!(s0, s1);
    }

    #[test]
    fn head_branch_matches_fingerprint() {
        let Some(root) = git_repo("watch-refs", "main") else {
            return;
        };
        let fp = git::fingerprint(&root);
        assert_eq!(git::head_branch(&root), (fp.head, fp.branch));
        git(&root, &["checkout", "-b", "other"]);
        let fp = git::fingerprint(&root);
        assert_eq!(
            git::head_branch(&root),
            (fp.head.clone(), fp.branch.clone())
        );
        assert_eq!(fp.branch.as_deref(), Some("other"));
    }

    #[test]
    fn head_branch_none_outside_own_repo() {
        let Some(repo) = git_repo("watch-nested", "main") else {
            return;
        };
        let sub = repo.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(git::head_branch(&sub), (None, None));
        let fp = git::fingerprint(&sub);
        assert_eq!((fp.head, fp.branch), (None, None));
        let plain = tmpdir("watch-plain");
        assert_eq!(git::head_branch(&plain), (None, None));
    }

    #[test]
    fn head_branch_filters_short_head() {
        let root = tmpdir("watch-short-head");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let canon = root.canonicalize().unwrap().to_string_lossy().to_string();
        let run = |_dir: &Path, args: &[&str]| -> Option<String> {
            match args {
                ["rev-parse", "--show-toplevel"] => Some(canon.clone()),
                ["rev-parse", "HEAD"] => Some("abc".to_string()),
                ["branch", "--show-current"] => Some("main".to_string()),
                _ => None,
            }
        };
        assert_eq!(
            git::head_branch_using(&root, &run),
            (None, Some("main".to_string()))
        );
    }
}

#[cfg(test)]
mod loop_tests {
    use super::{drive_events, event_json, run, write_event, Event, Options, Signature, WatchDeps};
    use crate::index::{Freshness, IndexSummary, PhaseMs};
    use std::collections::{BTreeMap, VecDeque};
    use std::io::{self, Write};

    /// Scripted clock and signatures. The clock moves only in `sleep`.
    struct Fake {
        now: u64,
        sleeps: Vec<u64>,
        sigs: VecDeque<Result<String, String>>,
        sig_default: String,
        freshness_status: String,
        builds: Vec<u64>,
        build_results: VecDeque<Result<IndexSummary, String>>,
        sig_on_build: Option<String>,
        bump_on_build: bool,
        stop_after_sleeps: usize,
        stop_after_builds: usize,
    }

    impl Fake {
        fn new(freshness: &str, start: &str) -> Fake {
            Fake {
                now: 0,
                sleeps: Vec::new(),
                sigs: VecDeque::new(),
                sig_default: start.to_string(),
                freshness_status: freshness.to_string(),
                builds: Vec::new(),
                build_results: VecDeque::new(),
                sig_on_build: None,
                bump_on_build: false,
                stop_after_sleeps: 20,
                stop_after_builds: usize::MAX,
            }
        }

        /// Signature answers in order; once they run out, `sig_default`.
        fn script(mut self, tags: &[&str]) -> Fake {
            self.sigs = tags.iter().map(|t| Ok(t.to_string())).collect();
            self
        }
    }

    impl WatchDeps for Fake {
        fn now_ms(&mut self) -> u64 {
            self.now
        }

        fn sleep(&mut self, ms: u64) {
            self.sleeps.push(ms);
            self.now += ms;
        }

        fn signature(&mut self) -> Result<Signature, String> {
            let next = self
                .sigs
                .pop_front()
                .unwrap_or_else(|| Ok(self.sig_default.clone()));
            next.map(|tag| Signature {
                files: BTreeMap::new(),
                config: tag,
                head: None,
                branch: None,
            })
        }

        fn build(&mut self) -> Result<IndexSummary, String> {
            self.builds.push(self.now);
            if let Some(next) = self.sig_on_build.take() {
                self.sig_default = next;
            }
            if self.bump_on_build {
                self.sig_default = format!("v{}", self.builds.len());
            }
            self.build_results
                .pop_front()
                .unwrap_or_else(|| Ok(summary()))
        }

        fn freshness(&mut self) -> Freshness {
            Freshness {
                status: self.freshness_status.clone(),
                reasons: Vec::new(),
                snapshot: None,
            }
        }

        fn should_stop(&mut self) -> bool {
            self.sleeps.len() >= self.stop_after_sleeps
                || self.builds.len() >= self.stop_after_builds
        }
    }

    fn summary() -> IndexSummary {
        IndexSummary {
            snapshot_id: 1,
            status_at_write: "fresh".to_string(),
            pre_fingerprint: String::new(),
            post_fingerprint: String::new(),
            files_discovered: 0,
            processed: 0,
            definitions: 0,
            imports: 0,
            calls: 0,
            errors: 0,
            skipped_ignored: 0,
            skipped_large: 0,
            skipped_binary: 0,
            skipped_symlink: 0,
            skipped_escape: 0,
            skipped_unreadable: 0,
            unsupported_total: 0,
            mode: "incremental".to_string(),
            reused: 0,
            reparsed: 0,
            added: 0,
            removed: 0,
            fallback_reason: String::new(),
            timings_ms: PhaseMs {
                diff: 0,
                ingest: 0,
                verify: 0,
                promote: 0,
            },
        }
    }

    fn busy() -> String {
        "index-busy: lock held".to_string()
    }

    fn name(e: &Event) -> &'static str {
        match e {
            Event::Started { .. } => "started",
            Event::Built(_) => "built",
            Event::Failed { .. } => "failed",
            Event::Busy => "busy",
            Event::Stopped { .. } => "stopped",
        }
    }

    fn drive(opts: &Options, fake: &mut Fake) -> (Result<(), String>, Vec<&'static str>) {
        let mut names = Vec::new();
        let result = run(opts, fake, &mut |e: &Event| names.push(name(e)));
        (result, names)
    }

    #[test]
    fn defaults_match_the_brief() {
        let o = Options::default();
        assert_eq!(
            (o.interval_ms, o.debounce_ms, o.max_wait_ms, o.max_builds),
            (1000, 500, 10_000, None)
        );
    }

    #[test]
    fn stale_at_start_builds_once() {
        let mut fake = Fake::new("stale", "A");
        let (result, names) = drive(&Options::default(), &mut fake);
        assert!(result.is_ok());
        assert_eq!(names, vec!["started", "built"]);
        assert_eq!(fake.builds, vec![1000]);
    }

    #[test]
    fn fresh_at_start_does_not_build() {
        let mut fake = Fake::new("fresh", "A");
        let (result, names) = drive(&Options::default(), &mut fake);
        assert!(result.is_ok());
        assert_eq!(names, vec!["started"]);
        assert!(fake.builds.is_empty());
    }

    #[test]
    fn change_during_startup_is_not_lost() {
        // A is taken before the freshness check, B after it.
        let mut fake = Fake::new("fresh", "B").script(&["A"]);
        let (result, names) = drive(&Options::default(), &mut fake);
        assert!(result.is_ok());
        assert_eq!(names, vec!["started", "built"]);
        assert_eq!(fake.builds, vec![2000]);
    }

    #[test]
    fn burst_is_coalesced() {
        // Changes on four polls (t=100..400), then quiet. The build waits 500 ms after the last change.
        let mut fake = Fake::new("fresh", "E").script(&["A", "B", "C", "D"]);
        let opts = Options {
            interval_ms: 100,
            ..Options::default()
        };
        let (result, names) = drive(&opts, &mut fake);
        assert!(result.is_ok());
        assert_eq!(names, vec!["started", "built"]);
        assert_eq!(fake.builds, vec![900]);
    }

    #[test]
    fn change_during_build_triggers_second_build() {
        let mut fake = Fake::new("stale", "A");
        fake.sig_on_build = Some("B".to_string());
        let (result, names) = drive(&Options::default(), &mut fake);
        assert!(result.is_ok());
        assert_eq!(names, vec!["started", "built", "built"]);
        assert_eq!(fake.builds, vec![1000, 3000]);
    }

    #[test]
    fn no_rebuild_without_change_after_success() {
        let mut fake = Fake::new("stale", "A");
        fake.stop_after_sleeps = 20;
        let (result, names) = drive(&Options::default(), &mut fake);
        assert!(result.is_ok());
        assert_eq!(names, vec!["started", "built"]);
        assert_eq!(fake.builds.len(), 1);
    }

    #[test]
    fn failed_build_waits_for_change_and_backs_off() {
        let mut fake = Fake::new("stale", "C");
        fake.sigs = ["A", "A", "A", "A", "A", "A", "A", "B", "B", "B", "C", "C"]
            .iter()
            .map(|t| Ok(t.to_string()))
            .collect();
        fake.build_results = VecDeque::from(vec![
            Err("boom".to_string()),
            Err("boom".to_string()),
            Ok(summary()),
        ]);
        fake.stop_after_sleeps = 12;
        let (result, names) = drive(&Options::default(), &mut fake);
        assert!(result.is_ok());
        assert_eq!(names, vec!["started", "failed", "failed", "built"]);
        assert_eq!(
            fake.sleeps,
            vec![1000, 2000, 2000, 2000, 2000, 2000, 2000, 2000, 4000, 4000, 4000, 1000]
        );
        assert_eq!(fake.builds, vec![1000, 15000, 27000]);
    }

    #[test]
    fn busy_stays_pending() {
        let mut fake = Fake::new("stale", "A");
        fake.build_results = VecDeque::from(vec![Err(busy()), Ok(summary())]);
        let (result, names) = drive(&Options::default(), &mut fake);
        assert!(result.is_ok());
        assert_eq!(names, vec!["started", "busy", "built"]);
        assert_eq!(fake.builds, vec![1000, 2000]);
    }

    #[test]
    fn consecutive_busy_waits_debounce() {
        let mut fake = Fake::new("stale", "A");
        fake.build_results =
            VecDeque::from(vec![Err(busy()), Err(busy()), Err(busy()), Ok(summary())]);
        fake.stop_after_sleeps = 100;
        let opts = Options {
            interval_ms: 100,
            debounce_ms: 500,
            max_wait_ms: 2000,
            max_builds: None,
        };
        let (result, names) = drive(&opts, &mut fake);
        assert!(result.is_ok());
        assert_eq!(names, vec!["started", "busy", "busy", "busy", "built"]);
        assert_eq!(fake.builds, vec![500, 1000, 1500, 2000]);
    }

    #[test]
    fn continuous_changes_build_after_max_wait() {
        // The signature changes on every poll. The first change is at t=100.
        let mut fake = Fake::new("fresh", "A");
        fake.stop_after_sleeps = 100;
        fake.sigs = std::iter::once(Ok("A".to_string()))
            .chain((1..=60).map(|i| Ok(format!("c{i}"))))
            .collect();
        let opts = Options {
            interval_ms: 100,
            debounce_ms: 500,
            max_wait_ms: 2000,
            max_builds: Some(2),
        };
        let (result, names) = drive(&opts, &mut fake);
        assert!(result.is_ok());
        assert_eq!(names, vec!["started", "built", "built"]);
        assert_eq!(fake.builds, vec![2100, 4200]);
    }

    #[test]
    fn backoff_never_overflows() {
        let mut fake = Fake::new("stale", "A");
        fake.bump_on_build = true;
        fake.build_results = (0..100).map(|_| Err("boom".to_string())).collect();
        fake.stop_after_builds = 100;
        fake.stop_after_sleeps = usize::MAX;
        let (result, names) = drive(&Options::default(), &mut fake);
        assert!(result.is_ok());
        assert_eq!(fake.builds.len(), 100);
        assert_eq!(
            names.iter().copied().filter(|n| *n == "failed").count(),
            100
        );
        assert!(fake.sleeps.iter().all(|s| *s <= 60_000));
        assert!(fake.sleeps.contains(&60_000));
    }

    #[test]
    fn signature_error_stops_run() {
        let mut fake = Fake::new("fresh", "A");
        fake.sigs = VecDeque::from(vec![Ok("A".to_string()), Err("gone".to_string())]);
        let (result, names) = drive(&Options::default(), &mut fake);
        assert_eq!(result, Err("gone".to_string()));
        assert_eq!(names, vec!["started"]);
    }

    #[test]
    fn event_json_shapes() {
        let started = Event::Started {
            status: "stale".to_string(),
        };
        assert_eq!(
            event_json("r", "/root", &started),
            r#"{"event":"started","repo":"r","status":"stale"}"#
        );
        assert_eq!(
            event_json("r", "/root", &Event::Busy),
            r#"{"event":"busy","repo":"r"}"#
        );
        let failed = Event::Failed {
            reason: "a\"b".to_string(),
        };
        assert_eq!(
            event_json("r", "/root", &failed),
            r#"{"event":"failed","repo":"r","reason":"a\"b"}"#
        );
        let stopped = Event::Stopped {
            reason: "gone".to_string(),
        };
        assert_eq!(
            event_json("r", "/root", &stopped),
            r#"{"event":"stopped","repo":"r","reason":"gone"}"#
        );
        let index = crate::response::index_summary_json("r", "/root", &summary());
        assert_eq!(
            event_json("r", "/root", &Event::Built(summary())),
            format!("{{\"event\":\"built\",\"index\":{index}}}")
        );
    }

    /// A writer whose every write fails the way a closed pipe does.
    struct BrokenPipe;

    impl Write for BrokenPipe {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_event_maps_broken_pipe() {
        assert!(write_event(&mut BrokenPipe, r#"{"event":"busy","repo":"r"}"#).is_err());
        let mut sink: Vec<u8> = Vec::new();
        write_event(&mut sink, "x").unwrap();
        assert_eq!(sink, b"x\n");
    }

    #[test]
    fn drive_exits_3_when_an_event_cannot_be_written() {
        let mut fake = Fake::new("stale", "A");
        let mut err = String::new();
        let code = drive_events(
            &Options::default(),
            &mut fake,
            "r",
            "/root",
            &mut BrokenPipe,
            &mut err,
        );
        assert_eq!(code, 3);
        assert!(err.contains("cannot write watch event"), "{err}");
        assert!(fake.builds.is_empty(), "no build once the output is gone");
    }

    #[test]
    fn drive_writes_each_event_and_exits_0() {
        let mut fake = Fake::new("stale", "A");
        let mut sink: Vec<u8> = Vec::new();
        let mut err = String::new();
        let code = drive_events(
            &Options::default(),
            &mut fake,
            "r",
            "/root",
            &mut sink,
            &mut err,
        );
        assert_eq!(code, 0);
        assert!(err.is_empty(), "{err}");
        let text = String::from_utf8(sink).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert_eq!(
            lines[0],
            r#"{"event":"started","repo":"r","status":"stale"}"#
        );
        assert!(
            lines[1].starts_with(r#"{"event":"built","index":{"#),
            "{}",
            lines[1]
        );
    }

    #[test]
    fn drive_run_error_writes_stopped_then_exits_3() {
        let mut fake = Fake::new("fresh", "A");
        fake.sigs = VecDeque::from(vec![Ok("A".to_string()), Err("gone".to_string())]);
        let mut sink: Vec<u8> = Vec::new();
        let mut err = String::new();
        let code = drive_events(
            &Options::default(),
            &mut fake,
            "r",
            "/root",
            &mut sink,
            &mut err,
        );
        assert_eq!(code, 3);
        let text = String::from_utf8(sink).unwrap();
        assert_eq!(
            text.lines().last(),
            Some(r#"{"event":"stopped","repo":"r","reason":"gone"}"#)
        );
        assert!(err.contains("gone"), "{err}");
    }
}
