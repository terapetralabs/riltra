// SPDX-License-Identifier: Apache-2.0
//! `changes`: which definitions a git change touches. Symbols come from a live
//! parse of the changed files, so they never depend on index freshness. Caller
//! counts come from the index when it is usable; any index failure degrades only
//! those counts, the structure is answered without it. A map of the change,
//! never an impact verdict.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;

use crate::git::{self, DiffGit, GitRun};
use crate::index;
use crate::json::escape;
use crate::paths;
use crate::scan::MAX_FILE_BYTES;
use crate::semantic;
use crate::store::{self, IndexRead};
use crate::syntax::{self, FileFacts, FileKind, Language, Span};

pub const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 1000;
/// Git's binary probe: a NUL byte in this prefix makes a file binary.
const BINARY_PROBE_BYTES: usize = 8000;
const DISCLAIMER: &str = "structure-of-change-not-impact";
/// Largest file the identity check reads, on both sides. Why: a duplicate-path comparison
/// holds both sides in memory and writes two copies to the temporary directory, so the bound
/// keeps one comparison proportionate. A larger identical file is listed, not proven identical.
const IDENTITY_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// `changes` relies on GIT_CONFIG_COUNT overrides, which git ignores before this release.
const MIN_GIT: (u32, u32) = (2, 31);

#[derive(Debug)]
pub struct Failure {
    pub code: i32,
    pub message: String,
}

impl Failure {
    fn new(code: i32, message: impl Into<String>) -> Failure {
        Failure {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scope {
    Unstaged,
    Staged,
    All,
    Compare,
}

impl Scope {
    fn parse(text: &str) -> Option<Scope> {
        Some(match text {
            "unstaged" => Scope::Unstaged,
            "staged" => Scope::Staged,
            "all" => Scope::All,
            "compare" => Scope::Compare,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Scope::Unstaged => "unstaged",
            Scope::Staged => "staged",
            Scope::All => "all",
            Scope::Compare => "compare",
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Request {
    pub scope: Scope,
    pub base: Option<String>,
    pub limit: usize,
}

/// Checks the arguments that need no repository. `Err` is a usage error (exit 2).
pub fn request(scope: &str, base: Option<&str>, limit: usize) -> Result<Request, String> {
    let scope_value = Scope::parse(scope).ok_or_else(|| format!("invalid --scope: '{scope}'"))?;
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(format!("--limit must be between 1 and {MAX_LIMIT}"));
    }
    match (scope_value, base) {
        (Scope::Compare, None) => return Err("--scope compare needs --base <ref>".to_string()),
        (other, Some(_)) if other != Scope::Compare => {
            return Err("--base is only valid with --scope compare".to_string())
        }
        (_, Some(text)) if text.starts_with('-') => {
            return Err(format!("refusing a ref that starts with '-': '{text}'"))
        }
        _ => {}
    }
    Ok(Request {
        scope: scope_value,
        base: base.map(str::to_string),
        limit,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Range {
    pub start: u32,
    pub count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hunk {
    pub old: Range,
    pub new: Range,
}

/// Parses one `@@ -a[,b] +c[,d] @@` header; an omitted count means 1.
pub fn parse_hunk(line: &str) -> Option<Hunk> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let new = rest.split(" @@").next()?;
    Some(Hunk {
        old: parse_range(old)?,
        new: parse_range(new)?,
    })
}

fn parse_range(text: &str) -> Option<Range> {
    match text.split_once(',') {
        Some((start, count)) => Some(Range {
            start: start.parse().ok()?,
            count: count.parse().ok()?,
        }),
        None => Some(Range {
            start: text.parse().ok()?,
            count: 1,
        }),
    }
}

/// Whether a range of one or more lines covers `line`.
fn covers(range: Range, line: u32) -> bool {
    range.count > 0 && range.start <= line && line < range.start + range.count
}

/// Whether a range of one or more lines meets the closed line interval `first..=last`.
fn intersects(range: Range, first: u32, last: u32) -> bool {
    range.start <= last && first < range.start + range.count
}

/// Whether one hunk touches a definition. New-side lines meet the new span; a
/// pure deletion (count 0, start = the line before the deleted text) touches when
/// `start_line <= start < end_line`. Old-side lines meet the old span.
pub fn touches(hunk: &Hunk, new: Option<&Span>, old: Option<&Span>) -> bool {
    let new_hit = new.is_some_and(|s| {
        if hunk.new.count > 0 {
            intersects(hunk.new, s.start_line, s.end_line)
        } else {
            s.start_line <= hunk.new.start && hunk.new.start < s.end_line
        }
    });
    let old_hit =
        old.is_some_and(|s| hunk.old.count > 0 && intersects(hunk.old, s.start_line, s.end_line));
    new_hit || old_hit
}

#[derive(Debug, Clone)]
pub struct Def {
    pub kind: &'static str,
    pub name: String,
    pub span: Span,
}

#[derive(Debug)]
pub enum Pair<'a> {
    Both(&'a Def, &'a Def),
    NewOnly(&'a Def),
    OldOnly(&'a Def),
}

/// Matches old and new definitions by (kind, name).
// shortcut: duplicate names pair in source order, upgrade when a language needs overload or scope resolution
pub fn pair_defs<'a>(old: &'a [Def], new: &'a [Def]) -> Vec<Pair<'a>> {
    let mut used = BTreeSet::new();
    let mut pairs = Vec::new();
    for def in new {
        let found = old
            .iter()
            .enumerate()
            .find(|(o, d)| !used.contains(o) && d.kind == def.kind && d.name == def.name);
        match found {
            Some((o, d)) => {
                used.insert(o);
                pairs.push(Pair::Both(d, def));
            }
            None => pairs.push(Pair::NewOnly(def)),
        }
    }
    for (o, d) in old.iter().enumerate() {
        if !used.contains(&o) {
            pairs.push(Pair::OldOnly(d));
        }
    }
    pairs
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Modified,
    Added,
    Deleted,
    Renamed,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Modified => "modified",
            Kind::Added => "added",
            Kind::Deleted => "deleted",
            Kind::Renamed => "renamed",
        }
    }
}

/// A git path as display text, and whether it was valid UTF-8. Only valid names go to git.
type Named = (String, bool);

fn name_of(bytes: &[u8]) -> Named {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_string(), true),
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), false),
    }
}

/// What a git record says about a path's modes or state. Each one is a refusal
/// reason, decided before any content is read.
#[derive(Debug, Clone, Copy)]
struct Flags {
    /// Mode 160000 on either side: a submodule pointer.
    gitlink: bool,
    /// Mode 120000 on either side: a symlink blob, whose target text is never parsed.
    symlink: bool,
    /// Status U: a merge conflict with no single version to read.
    unmerged: bool,
}

#[derive(Debug, Clone)]
struct Change {
    /// Display name (lossy when the name is not UTF-8).
    path: String,
    old_path: Option<String>,
    kind: Kind,
    untracked: bool,
    /// The name is not UTF-8: reported, never handed to git.
    non_utf8: bool,
    gitlink: bool,
    symlink: bool,
    unmerged: bool,
    merged: bool,
    filter_managed: bool,
}

fn new_change(
    kind: Kind,
    path: Named,
    old_path: Option<Named>,
    untracked: bool,
    flags: Flags,
) -> Change {
    Change {
        path: path.0,
        old_path: old_path.as_ref().map(|old| old.0.clone()),
        kind,
        untracked,
        non_utf8: !path.1 || old_path.as_ref().is_some_and(|old| !old.1),
        gitlink: flags.gitlink,
        symlink: flags.symlink,
        unmerged: flags.unmerged,
        merged: false,
        filter_managed: false,
    }
}

const NO_FLAGS: Flags = Flags {
    gitlink: false,
    symlink: false,
    unmerged: false,
};

fn next_name<'a>(fields: &mut impl Iterator<Item = &'a [u8]>) -> Result<Named, String> {
    fields
        .next()
        .map(name_of)
        .ok_or_else(|| "truncated git output".to_string())
}

/// Parses `git diff --raw -z`: a record ":old-mode new-mode old-id new-id STATUS",
/// then the path (two paths for a rename).
fn parse_raw(out: &[u8]) -> Result<Vec<Change>, String> {
    let mut fields = out.split(|b| *b == 0).filter(|f| !f.is_empty());
    let mut changes = Vec::new();
    while let Some(record) = fields.next() {
        let record = String::from_utf8_lossy(record).into_owned();
        let parts: Vec<&str> = record.split_whitespace().collect();
        let (old_mode, new_mode, code) = match parts.as_slice() {
            [old_mode, new_mode, _, _, code] => {
                (old_mode.trim_start_matches(':'), *new_mode, *code)
            }
            _ => return Err(format!("unreadable git record: '{record}'")),
        };
        let flags = Flags {
            gitlink: old_mode == "160000" || new_mode == "160000",
            symlink: old_mode == "120000" || new_mode == "120000",
            unmerged: false,
        };
        let change = match code.chars().next().unwrap_or('?') {
            'M' | 'T' => new_change(Kind::Modified, next_name(&mut fields)?, None, false, flags),
            'U' => new_change(
                Kind::Modified,
                next_name(&mut fields)?,
                None,
                false,
                Flags {
                    unmerged: true,
                    ..flags
                },
            ),
            'A' => new_change(Kind::Added, next_name(&mut fields)?, None, false, flags),
            'D' => new_change(Kind::Deleted, next_name(&mut fields)?, None, false, flags),
            'R' => {
                let old = next_name(&mut fields)?;
                let new = next_name(&mut fields)?;
                new_change(Kind::Renamed, new, Some(old), false, flags)
            }
            other => return Err(format!("unsupported change kind '{other}' in git output")),
        };
        changes.push(change);
    }
    Ok(changes)
}

#[derive(Debug, Clone, PartialEq)]
enum Answer {
    Count(usize),
    Reason(&'static str),
}

/// Path, definition kind and name: the key of one index caller lookup.
type Key = (String, String, &'static str);

struct DefOut {
    name: String,
    kind: &'static str,
    change: &'static str,
    header_touched: bool,
    old_span: Option<Span>,
    new_span: Option<Span>,
    lookup: Option<Key>,
    sort_line: u32,
}

struct FileOut {
    path: String,
    old_path: Option<String>,
    status: &'static str,
    change: &'static str,
    reason: Option<&'static str>,
    language: Option<&'static str>,
    established: bool,
    defs: Vec<DefOut>,
    outside: Vec<Hunk>,
    hunks: usize,
}

enum Read {
    Bytes(Vec<u8>),
    TooLarge,
    Unreadable,
}

impl Read {
    fn bytes(&self) -> Option<&[u8]> {
        match self {
            Read::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_PROBE_BYTES).any(|b| *b == 0)
}

fn words(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

struct Ctx<'a> {
    git: &'a DiffGit,
    root: &'a Path,
    scope: Scope,
    base_id: Option<String>,
    autocrlf: String,
}

impl Ctx<'_> {
    fn run(&self, args: &[String]) -> Result<GitRun, Failure> {
        self.git
            .run(self.root, args)
            .map_err(|e| Failure::new(3, e))
    }

    fn run_ok(&self, args: &[String]) -> Result<Vec<u8>, Failure> {
        let run = self.run(args)?;
        if run.code != Some(0) {
            return Err(Failure::new(3, format!("git failed: {}", run.stderr)));
        }
        Ok(run.stdout)
    }

    /// Revision arguments of `git diff` for the scope (none for unstaged).
    fn rev_args(&self) -> Vec<String> {
        match self.scope {
            Scope::Unstaged => Vec::new(),
            Scope::Staged => words(&["--cached"]),
            Scope::All => words(&["HEAD"]),
            Scope::Compare => vec![self.base_id.clone().unwrap_or_default()],
        }
    }

    /// The old side's object spec: index for unstaged, HEAD for staged and all, the base for compare.
    fn old_spec(&self, path: &str) -> String {
        match self.scope {
            Scope::Unstaged => format!(":./{path}"),
            Scope::Compare => format!("{}:./{path}", self.base_id.as_deref().unwrap_or_default()),
            Scope::Staged | Scope::All => format!("HEAD:./{path}"),
        }
    }
}

/// A blob's raw content, read only when `cat-file -s` says it is within the cap.
fn blob(ctx: &Ctx, spec: &str) -> Result<Read, Failure> {
    let size = ctx.run_ok(&words(&["cat-file", "-s", spec]))?;
    let size = String::from_utf8_lossy(&size)
        .trim()
        .parse::<u64>()
        .map_err(|_| Failure::new(3, format!("git gave no size for '{spec}'")))?;
    if size > MAX_FILE_BYTES {
        return Ok(Read::TooLarge);
    }
    Ok(Read::Bytes(
        ctx.run_ok(&words(&["cat-file", "blob", spec]))?,
    ))
}

/// The worktree file through the confined reader: canonical containment, so a link or
/// junction out of the root is refused, and the size cap applies.
fn worktree(root: &Path, rel: &str) -> Read {
    worktree_capped(root, rel, MAX_FILE_BYTES)
}

/// `worktree` with its own size cap.
fn worktree_capped(root: &Path, rel: &str, cap: u64) -> Read {
    // Containment first: a link out of the root is refused before any metadata of the file is read.
    if semantic::confined_path(root, rel).is_err() {
        return Read::Unreadable;
    }
    match std::fs::symlink_metadata(root.join(rel)) {
        Ok(meta) if !meta.file_type().is_file() => Read::Unreadable,
        Ok(meta) if meta.len() > cap => Read::TooLarge,
        Ok(_) => semantic::confined_read(root, rel, cap).map_or(Read::Unreadable, Read::Bytes),
        Err(_) => Read::Unreadable,
    }
}

fn read_sides(ctx: &Ctx, change: &Change) -> Result<(Option<Read>, Option<Read>), Failure> {
    let old_path = match change.kind {
        Kind::Added => None,
        Kind::Renamed => change.old_path.as_deref(),
        _ => Some(change.path.as_str()),
    };
    let old = match old_path {
        Some(path) => Some(blob(ctx, &ctx.old_spec(path))?),
        None => None,
    };
    let new = match change.kind {
        Kind::Deleted => None,
        _ if ctx.scope == Scope::Staged && !change.untracked => {
            Some(blob(ctx, &format!(":./{}", change.path))?)
        }
        _ => Some(worktree(ctx.root, &change.path)),
    };
    Ok((old, new))
}

/// Changed paths, one entry per path. A conflicted path can come out as two raw
/// records (one per stage in the unstaged scope); the conflict record wins.
fn changed_paths(ctx: &Ctx) -> Result<Vec<Change>, Failure> {
    let mut args = words(&[
        "diff",
        "--relative",
        "--raw",
        "-z",
        "--find-renames",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
    ]);
    args.extend(ctx.rev_args());
    let out = ctx.run_ok(&args)?;
    let mut by_path: BTreeMap<String, Change> = BTreeMap::new();
    for change in parse_raw(&out).map_err(|e| Failure::new(3, e))? {
        let keep_previous = match by_path.get(&change.path) {
            Some(previous) => !(change.unmerged && !previous.unmerged),
            None => false,
        };
        if !keep_previous {
            by_path.insert(change.path.clone(), change);
        }
    }
    Ok(by_path.into_values().collect())
}

/// Conflicted paths from `ls-files -u -z`: one record per stage, "<mode> <id> <stage>\t<path>".
/// The map gives each path (lossy display) and whether its name was valid UTF-8.
fn parse_unmerged(out: &[u8]) -> BTreeMap<String, bool> {
    let mut paths = BTreeMap::new();
    for record in out.split(|b| *b == 0).filter(|f| !f.is_empty()) {
        if let Some(tab) = record.iter().position(|b| *b == b'\t') {
            if let Some(name) = record.get(tab + 1..) {
                let (text, utf8) = name_of(name);
                paths.insert(text, utf8);
            }
        }
    }
    paths
}

fn unmerged_paths(ctx: &Ctx) -> Result<BTreeMap<String, bool>, Failure> {
    Ok(parse_unmerged(
        &ctx.run_ok(&words(&["ls-files", "-u", "-z"]))?,
    ))
}

fn untracked(ctx: &Ctx) -> Result<Vec<Change>, Failure> {
    let out = ctx.run_ok(&words(&[
        "ls-files",
        "--others",
        "--exclude-standard",
        "-z",
    ]))?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| new_change(Kind::Added, name_of(f), None, true, NO_FLAGS))
        .collect())
}

/// Reconciles the tracked and untracked listings by path, before the count and the
/// limit. A path the index lacks is `added` through the untracked listing and is
/// never duplicated. In the all and compare scopes a path the tracked diff calls
/// deleted, but that exists in the worktree, is one entry: nothing when the two sides
/// are identical, otherwise `modified`. A submodule pointer is refused before any
/// content is read or compared.
fn reconcile(
    ctx: &Ctx,
    tracked: Vec<Change>,
    untracked: Vec<Change>,
) -> Result<Vec<Change>, Failure> {
    let mut extra = untracked;
    let mut out = Vec::new();
    for change in tracked {
        let reappears = change.kind == Kind::Deleted
            && matches!(ctx.scope, Scope::All | Scope::Compare)
            && extra.iter().any(|u| u.path == change.path);
        if !reappears {
            out.push(change);
            continue;
        }
        extra.retain(|u| u.path != change.path);
        if change.gitlink {
            out.push(Change {
                kind: Kind::Modified,
                ..change
            });
            continue;
        }
        if change.filter_managed || !same_content(ctx, &change.path)? {
            out.push(Change {
                kind: Kind::Modified,
                merged: true,
                ..change
            });
        }
    }
    out.extend(extra);
    Ok(out)
}

/// Whether the old side (HEAD for all, the base for compare) and the worktree file are
/// identical as `git diff --no-index` compares them. The worktree side is read only
/// through the confined reader, up to `IDENTITY_MAX_BYTES`. A file over that bound is
/// not proven identical, so its entry is listed; the analysis then refuses it as too
/// large or unreadable.
fn same_content(ctx: &Ctx, path: &str) -> Result<bool, Failure> {
    let spec = ctx.old_spec(path);
    let size = ctx.run_ok(&words(&["cat-file", "-s", spec.as_str()]))?;
    let size = String::from_utf8_lossy(&size)
        .trim()
        .parse::<u64>()
        .map_err(|_| Failure::new(3, format!("git gave no size for '{spec}'")))?;
    if size > IDENTITY_MAX_BYTES {
        return Ok(false);
    }
    let before = ctx.run_ok(&words(&["cat-file", "blob", spec.as_str()]))?;
    match worktree_capped(ctx.root, path, IDENTITY_MAX_BYTES) {
        Read::Bytes(after) => Ok(no_index(ctx, &before, &after, true)?.0),
        Read::TooLarge | Read::Unreadable => Ok(false),
    }
}

/// `git diff --no-index` on two byte strings, run in a fresh temporary directory outside
/// the repository. With `quiet` git only reports whether the files differ (exit 0 when
/// identical, 1 when not) and no diff text is produced, so the identity decision holds
/// nothing large in memory. Without `quiet` the text of the diff comes back as well.
/// Returns whether the two are identical and the text. The child gets the repository's
/// effective `core.autocrlf` and `core.safecrlf=false` as command-scope overrides, so the
/// machine's system and global settings do not decide the comparison.
// shortcut: the duplicate-path identity and hunks use only the repository's core.autocrlf; .gitattributes text and eol rules are not applied here, upgrade when a repository depends on them
fn no_index(
    ctx: &Ctx,
    before: &[u8],
    after: &[u8],
    quiet: bool,
) -> Result<(bool, String), Failure> {
    let dir = TempDir::create()?;
    std::fs::write(dir.path().join("old"), before)
        .map_err(|e| Failure::new(3, format!("cannot write a temp file: {e}")))?;
    std::fs::write(dir.path().join("new"), after)
        .map_err(|e| Failure::new(3, format!("cannot write a temp file: {e}")))?;
    let mut args = words(&["diff", "--no-index"]);
    if quiet {
        args.push("--quiet".to_string());
    } else {
        args.push("-U0".to_string());
    }
    args.extend(words(&[
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--",
        "old",
        "new",
    ]));
    let run = ctx
        .git
        .run_in_with(
            dir.path(),
            &args,
            &[
                ("core.autocrlf", ctx.autocrlf.as_str()),
                ("core.safecrlf", "false"),
            ],
        )
        .map_err(|e| Failure::new(3, e))?;
    // git diff --no-index exits 1 when the two files differ.
    if !matches!(run.code, Some(0) | Some(1)) {
        return Err(Failure::new(
            3,
            format!("git diff --no-index failed: {}", run.stderr),
        ));
    }
    let text = if quiet {
        String::new()
    } else {
        String::from_utf8_lossy(&run.stdout).into_owned()
    };
    Ok((run.code == Some(0) && text.is_empty(), text))
}

/// Hunks between two byte strings, from `no_index` (the full diff text).
fn no_index_hunks(ctx: &Ctx, before: &[u8], after: &[u8]) -> Result<Vec<Hunk>, Failure> {
    let (_, text) = no_index(ctx, before, after, false)?;
    Ok(text.lines().filter_map(parse_hunk).collect())
}

/// Index paths with a directory component that leads out of the root (a link or junction on
/// the way). Git follows such a link only in part, so whether git lists such a path depends on
/// the outside content. Those paths are therefore listed as modified (and refused as
/// unreadable) whether or not git lists them.
fn escaped_index_paths(ctx: &Ctx) -> Result<BTreeSet<String>, Failure> {
    let out = ctx.run_ok(&words(&["ls-files", "-z"]))?;
    let mut inside: BTreeMap<String, bool> = BTreeMap::new();
    let mut escaped = BTreeSet::new();
    for name in out.split(|b| *b == 0).filter(|f| !f.is_empty()) {
        let path = String::from_utf8_lossy(name).into_owned();
        let parts: Vec<&str> = path.split('/').collect();
        let mut prefix = String::new();
        let mut leaves = false;
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            let ok = *inside
                .entry(prefix.clone())
                .or_insert_with(|| semantic::confined_path(ctx.root, &prefix).is_ok());
            if !ok {
                leaves = true;
                break;
            }
        }
        if leaves {
            escaped.insert(path);
        }
    }
    Ok(escaped)
}

/// The paths whose `filter` attribute names a driver (not `unspecified`, not `unset`).
/// Reads attributes only: `git check-attr` runs no filter.
fn filter_managed_paths(ctx: &Ctx, paths: &[String]) -> Result<BTreeSet<String>, Failure> {
    if paths.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
    }
    let args = words(&["check-attr", "-z", "--stdin", "filter"]);
    let run = ctx
        .git
        .run_stdin(ctx.root, &args, &input)
        .map_err(|e| Failure::new(3, e))?;
    if run.code != Some(0) {
        return Err(Failure::new(
            3,
            format!("git check-attr failed: {}", run.stderr),
        ));
    }
    let fields: Vec<&[u8]> = run
        .stdout
        .split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .collect();
    let mut managed = BTreeSet::new();
    for record in fields.chunks(3) {
        if let [path, _attribute, value] = record {
            if !matches!(*value, b"unspecified" | b"unset") {
                managed.insert(String::from_utf8_lossy(path).into_owned());
            }
        }
    }
    Ok(managed)
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A uniquely named directory under the system temp directory, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    /// A new directory whose name earlier runs did not use: process id, time and a
    /// counter in the name, retried a few times before the run fails.
    fn create() -> Result<TempDir, Failure> {
        let mut last = String::new();
        for _ in 0..3 {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let name = format!(
                "riltra-changes-{}-{}-{}",
                std::process::id(),
                nanos,
                TEMP_SEQ.fetch_add(1, Ordering::SeqCst)
            );
            let path = std::env::temp_dir().join(name);
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(TempDir(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = e.to_string(),
                Err(e) => {
                    return Err(Failure::new(
                        3,
                        format!("cannot create a temp directory: {e}"),
                    ))
                }
            }
        }
        Err(Failure::new(
            3,
            format!("cannot create a temp directory: {last}"),
        ))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best effort: Drop cannot report an error, so a directory that cannot be
        // removed is left behind.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The repository's effective `core.autocrlf` (unset means `false`), read through the hardened posture.
fn repo_autocrlf(git: &DiffGit, root: &Path) -> Result<String, Failure> {
    let value = git
        .config_value(root, "core.autocrlf")
        .map_err(|e| Failure::new(3, e))?;
    Ok(value.unwrap_or_else(|| "false".to_string()))
}

fn tracked_hunks(ctx: &Ctx, change: &Change) -> Result<Vec<Hunk>, Failure> {
    let rename = if change.kind == Kind::Renamed {
        "--find-renames"
    } else {
        "--no-renames"
    };
    let mut args = words(&[
        "diff",
        "--relative",
        "-U0",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        rename,
    ]);
    args.extend(ctx.rev_args());
    args.push("--".to_string());
    args.extend(change.old_path.iter().cloned());
    args.push(change.path.clone());
    let text = ctx.run_ok(&args)?;
    Ok(String::from_utf8_lossy(&text)
        .lines()
        .filter_map(parse_hunk)
        .collect())
}

/// An untracked file is one added hunk over all its lines; an empty file has none.
fn whole_file_hunk(bytes: &[u8]) -> Option<Hunk> {
    if bytes.is_empty() {
        return None;
    }
    let mut lines = bytes.iter().filter(|b| **b == b'\n').count() as u32;
    if bytes.last() != Some(&b'\n') {
        lines += 1;
    }
    Some(Hunk {
        old: Range { start: 0, count: 0 },
        new: Range {
            start: 1,
            count: lines,
        },
    })
}

fn defs_of(facts: &FileFacts) -> Vec<Def> {
    facts
        .definitions
        .iter()
        .filter_map(|f| {
            f.name.clone().map(|name| Def {
                kind: f.kind,
                name,
                span: f.span.clone(),
            })
        })
        .collect()
}

fn parse_side(lang: Language, side: &Option<Read>) -> Option<FileFacts> {
    side.as_ref()
        .and_then(Read::bytes)
        .map(|bytes| syntax::parse_source(lang, bytes))
}

fn out_def(
    lookup: &str,
    change: &'static str,
    kind: &'static str,
    name: &str,
    old: Option<&Span>,
    new: Option<&Span>,
    hunks: &[Hunk],
) -> DefOut {
    // shortcut: header is the first line, upgrade when multi-line signatures matter
    let header_touched = if change == "removed" {
        old.is_some_and(|s| hunks.iter().any(|h| covers(h.old, s.start_line)))
    } else {
        new.is_some_and(|s| hunks.iter().any(|h| covers(h.new, s.start_line)))
    };
    let sort_line = if change == "removed" {
        old.map_or(0, |s| s.start_line)
    } else {
        new.map_or(0, |s| s.start_line)
    };
    DefOut {
        name: name.to_string(),
        kind,
        change,
        header_touched,
        old_span: old.cloned(),
        new_span: new.cloned(),
        // Callers are looked up under the path the index holds (the old path for a rename).
        lookup: (change != "added").then(|| (lookup.to_string(), name.to_string(), kind)),
        sort_line,
    }
}

fn match_defs(lookup: &str, old: &[Def], new: &[Def], hunks: &[Hunk]) -> (Vec<DefOut>, Vec<Hunk>) {
    let mut defs = Vec::new();
    for pair in pair_defs(old, new) {
        match pair {
            Pair::Both(od, nd) => {
                if hunks
                    .iter()
                    .any(|h| touches(h, Some(&nd.span), Some(&od.span)))
                {
                    defs.push(out_def(
                        lookup,
                        "modified",
                        nd.kind,
                        &nd.name,
                        Some(&od.span),
                        Some(&nd.span),
                        hunks,
                    ));
                }
            }
            Pair::NewOnly(nd) => {
                if hunks.iter().any(|h| touches(h, Some(&nd.span), None)) {
                    defs.push(out_def(
                        lookup,
                        "added",
                        nd.kind,
                        &nd.name,
                        None,
                        Some(&nd.span),
                        hunks,
                    ));
                }
            }
            Pair::OldOnly(od) => {
                defs.push(out_def(
                    lookup,
                    "removed",
                    od.kind,
                    &od.name,
                    Some(&od.span),
                    None,
                    hunks,
                ));
            }
        }
    }
    defs.sort_by(|a, b| {
        a.sort_line
            .cmp(&b.sort_line)
            .then_with(|| a.name.cmp(&b.name))
    });
    let outside = hunks
        .iter()
        .filter(|h| {
            !old.iter().any(|d| touches(h, None, Some(&d.span)))
                && !new.iter().any(|d| touches(h, Some(&d.span), None))
        })
        .copied()
        .collect();
    (defs, outside)
}

/// Status precedence (fixed): a refusal that needs no content first (non-UTF-8
/// name, submodule, merge conflict), then content checks (too-large, unreadable,
/// binary), then language-unsupported, then parse-error.
fn analyse(ctx: &Ctx, change: &Change) -> Result<FileOut, Failure> {
    let kind = syntax::classify(Path::new(&change.path));
    let language = match &kind {
        FileKind::Supported(l) => Some(syntax::language_name(*l)),
        _ => None,
    };
    let lang = match kind {
        FileKind::Supported(l)
            if matches!(
                l,
                Language::Python | Language::Rust | Language::TypeScript | Language::Tsx
            ) =>
        {
            Some(l)
        }
        _ => None,
    };
    let refusal = if change.non_utf8 {
        Some("non-utf8-path")
    } else if change.gitlink {
        Some("submodule")
    } else if change.symlink {
        Some("symlink")
    } else if change.filter_managed {
        Some("filter-managed")
    } else if change.unmerged {
        Some("unmerged")
    } else if semantic::confined_path(ctx.root, &change.path).is_err() {
        // Git follows a link in a directory of the path only in part: nothing under it is read or diffed.
        Some("unreadable")
    } else {
        None
    };
    if let Some(reason) = refusal {
        return Ok(FileOut {
            path: change.path.clone(),
            old_path: change.old_path.clone(),
            status: "unsupported",
            change: change.kind.name(),
            reason: Some(reason),
            language,
            established: false,
            defs: Vec::new(),
            outside: Vec::new(),
            hunks: 0,
        });
    }
    let (old, new) = read_sides(ctx, change)?;
    let reads: Vec<&Read> = old.iter().chain(new.iter()).collect();
    let too_large = reads.iter().any(|r| matches!(r, Read::TooLarge));
    let unreadable = reads.iter().any(|r| matches!(r, Read::Unreadable));
    let binary = reads.iter().any(|r| r.bytes().is_some_and(is_binary));
    let (old_facts, new_facts) = match lang {
        Some(l) if !too_large && !unreadable && !binary => {
            (parse_side(l, &old), parse_side(l, &new))
        }
        _ => (None, None),
    };
    let errored = if new.is_some() {
        new_facts.as_ref().is_some_and(|f| f.errors > 0)
    } else {
        old_facts.as_ref().is_some_and(|f| f.errors > 0)
    };
    let (status, reason) = if too_large {
        ("unsupported", Some("too-large"))
    } else if unreadable {
        ("unsupported", Some("unreadable"))
    } else if binary {
        ("binary", Some("binary"))
    } else if lang.is_none() {
        ("unsupported", Some("language-unsupported"))
    } else if errored {
        ("parse-error", Some("syntax-error"))
    } else {
        (change.kind.name(), None)
    };
    let hunks: Vec<Hunk> = if change.untracked {
        match new.as_ref().and_then(Read::bytes) {
            Some(bytes) if !is_binary(bytes) => whole_file_hunk(bytes).into_iter().collect(),
            _ => Vec::new(),
        }
    } else {
        if change.merged {
            match (
                old.as_ref().and_then(Read::bytes),
                new.as_ref().and_then(Read::bytes),
            ) {
                (Some(before), Some(after)) => no_index_hunks(ctx, before, after)?,
                _ => Vec::new(),
            }
        } else {
            tracked_hunks(ctx, change)?
        }
    };
    let ordinary = reason.is_none();
    let (defs, outside) = if ordinary {
        let lookup = change.old_path.as_deref().unwrap_or(&change.path);
        let old_defs = old_facts.as_ref().map(defs_of).unwrap_or_default();
        let new_defs = new_facts.as_ref().map(defs_of).unwrap_or_default();
        match_defs(lookup, &old_defs, &new_defs, &hunks)
    } else if status == "binary" {
        (Vec::new(), Vec::new())
    } else {
        (Vec::new(), hunks.clone())
    };
    Ok(FileOut {
        path: change.path.clone(),
        old_path: change.old_path.clone(),
        status,
        change: change.kind.name(),
        reason,
        language,
        established: ordinary,
        defs,
        outside,
        hunks: hunks.len(),
    })
}

/// Index-side answers for every lookup key. Any failure here is `unavailable`,
/// which degrades only the caller counts (the structure never depends on it).
struct Freshness {
    status: String,
    reasons: Vec<String>,
}

fn index_answers(
    home: &Path,
    repo: &str,
    canon: &Path,
    keys: &BTreeSet<Key>,
    git: &DiffGit,
) -> (Freshness, BTreeMap<Key, Answer>) {
    let filled = |reason: &'static str| -> BTreeMap<Key, Answer> {
        keys.iter()
            .map(|k| (k.clone(), Answer::Reason(reason)))
            .collect()
    };
    let unavailable = |reason: &'static str| {
        (
            Freshness {
                status: "unavailable".to_string(),
                reasons: vec![reason.to_string()],
            },
            filled("index-unavailable"),
        )
    };
    match store::open_read(&store::db_path(home, repo)) {
        IndexRead::Missing => (
            Freshness {
                status: "no-index".to_string(),
                reasons: Vec::new(),
            },
            filled("no-index"),
        ),
        IndexRead::Outdated(_) => unavailable("schema-outdated"),
        IndexRead::Ahead(_) => unavailable("schema-ahead"),
        IndexRead::Error(_) => unavailable("store-error"),
        IndexRead::Ready(mut conn) => match count_callers(&mut conn, repo, canon, keys, git) {
            Ok(done) => done,
            Err(reason) => unavailable(reason),
        },
    }
}

fn count_callers(
    conn: &mut rusqlite::Connection,
    repo: &str,
    canon: &Path,
    keys: &BTreeSet<Key>,
    git: &DiffGit,
) -> Result<(Freshness, BTreeMap<Key, Answer>), &'static str> {
    let tx = conn.transaction().map_err(|_| "store-error")?;
    let snapshot = store::latest_snapshot(&tx)
        .map_err(|_| "store-error")?
        .ok_or("no-snapshot")?;
    store::require_graph_proof(&tx, snapshot.id).map_err(|e| index_reason(&e))?;
    let (status, reasons) =
        index::evaluate_snapshot_with(&tx, repo, canon, &snapshot, &|root| git.fingerprint(root));
    let mut answers = BTreeMap::new();
    for key in keys {
        let symbols = symbol_rows(&tx, snapshot.id, key).map_err(|_| "store-error")?;
        let answer = match symbols.as_slice() {
            [] => Answer::Reason("not-in-index"),
            [(byte_start, byte_end)] => Answer::Count(
                index::indexed_inbound_total(
                    &tx,
                    snapshot.id,
                    &key.0,
                    &key.1,
                    *byte_start,
                    *byte_end,
                )
                .map_err(|e| index_reason(&e))?,
            ),
            _ => Answer::Reason("ambiguous-in-index"),
        };
        answers.insert(key.clone(), answer);
    }
    Ok((Freshness { status, reasons }, answers))
}

/// Byte ranges of the index symbols with this path, name and kind in one snapshot.
fn symbol_rows(
    tx: &rusqlite::Connection,
    snapshot_id: i64,
    key: &Key,
) -> rusqlite::Result<Vec<(usize, usize)>> {
    let mut statement = tx.prepare(
        "SELECT s.byte_start, s.byte_end FROM symbols s JOIN files f ON f.id = s.file_id \
         WHERE s.snapshot_id = ?1 AND f.snapshot_id = ?1 AND f.path = ?2 AND s.name = ?3 AND s.kind = ?4",
    )?;
    let rows = statement.query_map(params![snapshot_id, key.0, key.1, key.2], |row| {
        Ok((
            row.get::<_, i64>(0)? as usize,
            row.get::<_, i64>(1)? as usize,
        ))
    })?;
    rows.collect()
}

/// Only a graph-proof refusal has its own reason; any other index error is a store error.
fn index_reason(message: &str) -> &'static str {
    if message.starts_with("graph-proof-incompatible") {
        "graph-proof-incompatible"
    } else {
        "store-error"
    }
}

/// `(major, minor)` of a `git version` line such as `git version 2.53.0.windows.2`.
pub fn parse_git_version(text: &str) -> Option<(u32, u32)> {
    let rest = text.trim().strip_prefix("git version ")?;
    let mut parts = rest.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

fn require_git_version() -> Result<(), Failure> {
    let out = std::process::Command::new("git")
        .arg("--version")
        .output()
        .map_err(|e| Failure::new(3, format!("git unavailable: {e}")))?;
    match parse_git_version(&String::from_utf8_lossy(&out.stdout)) {
        Some(found) if found >= MIN_GIT => Ok(()),
        _ => Err(Failure::new(3, "git 2.31 or newer is required for changes")),
    }
}

/// Full commit id of `rev`, or `None` when git does not know it. `--end-of-options`
/// keeps a `-`-prefixed ref from reading as an option.
fn resolve_commit(git: &DiffGit, root: &Path, rev: &str) -> Result<Option<String>, Failure> {
    let spec = format!("{rev}^{{commit}}");
    let args = words(&["rev-parse", "--verify", "--end-of-options", spec.as_str()]);
    let run = git.run(root, &args).map_err(|e| Failure::new(3, e))?;
    if run.code != Some(0) {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&run.stdout).trim().to_string(),
    ))
}

/// The `changes` document for one registered repository (CLI and MCP share it).
/// Every git call that reads repository content or state runs through the hardened
/// `DiffGit` posture, including the index freshness check. `git --version` and the
/// read of filter driver names run no repository code and are not hardened.
pub fn document(home: &Path, repo: &str, root: &Path, req: &Request) -> Result<String, Failure> {
    require_git_version()?;
    let canon = paths::canonical_root(root)
        .map_err(|e| Failure::new(3, format!("repository root unavailable: {e}")))?;
    if std::fs::symlink_metadata(canon.join(".git")).is_err() || !git::is_worktree_top(&canon) {
        return Err(Failure::new(
            3,
            "registered root is not the top level of a git work tree: no .git at the root",
        ));
    }
    let diff_git = DiffGit::new(&canon).map_err(|e| Failure::new(3, e))?;
    let autocrlf = repo_autocrlf(&diff_git, &canon)?;
    let head = resolve_commit(&diff_git, &canon, "HEAD")?
        .ok_or_else(|| Failure::new(3, "repository has no HEAD commit"))?;
    let base_id = match (&req.base, req.scope) {
        (Some(text), Scope::Compare) => Some(
            resolve_commit(&diff_git, &canon, text)?
                .ok_or_else(|| Failure::new(2, format!("unknown ref: '{text}'")))?,
        ),
        _ => None,
    };
    let ctx = Ctx {
        git: &diff_git,
        root: &canon,
        scope: req.scope,
        base_id,
        autocrlf,
    };
    let mut changes = changed_paths(&ctx)?;
    let conflicted = unmerged_paths(&ctx)?;
    for change in changes.iter_mut() {
        if let Some(&utf8) = conflicted.get(&change.path) {
            change.unmerged = true;
            change.non_utf8 |= !utf8;
        }
    }
    let listed: BTreeSet<String> = changes.iter().map(|c| c.path.clone()).collect();
    for (path, &utf8) in conflicted
        .iter()
        .filter(|(path, _)| !listed.contains(*path))
    {
        changes.push(new_change(
            Kind::Modified,
            (path.clone(), utf8),
            None,
            false,
            Flags {
                gitlink: false,
                symlink: false,
                unmerged: true,
            },
        ));
    }
    let mut untracked_list = if req.scope != Scope::Staged {
        untracked(&ctx)?
    } else {
        Vec::new()
    };
    let names: Vec<String> = changes
        .iter()
        .chain(untracked_list.iter())
        .filter(|c| !c.non_utf8)
        .map(|c| c.path.clone())
        .collect();
    let filtered = filter_managed_paths(&ctx, &names)?;
    for change in changes.iter_mut().chain(untracked_list.iter_mut()) {
        if filtered.contains(&change.path) {
            change.filter_managed = true;
        }
    }
    let mut changes = reconcile(&ctx, changes, untracked_list)?;
    let escaped = if req.scope == Scope::Staged {
        BTreeSet::new()
    } else {
        escaped_index_paths(&ctx)?
    };
    for change in changes.iter_mut() {
        if escaped.contains(&change.path) {
            change.kind = Kind::Modified;
        }
    }
    let listed: BTreeSet<String> = changes.iter().map(|c| c.path.clone()).collect();
    for path in escaped.difference(&listed) {
        changes.push(new_change(
            Kind::Modified,
            (path.clone(), true),
            None,
            false,
            NO_FLAGS,
        ));
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    let total = changes.len();
    let truncated = total > req.limit;
    changes.truncate(req.limit);
    let files = changes
        .iter()
        .map(|change| analyse(&ctx, change))
        .collect::<Result<Vec<FileOut>, Failure>>()?;
    let keys: BTreeSet<Key> = files
        .iter()
        .flat_map(|f| f.defs.iter().filter_map(|d| d.lookup.clone()))
        .collect();
    let (freshness, answers) = index_answers(home, repo, &canon, &keys, &diff_git);
    let files_json: Vec<String> = files.iter().map(|f| file_json(f, &answers)).collect();
    Ok(format!(
        "{{\"schema_version\":1,\"repo\":\"{}\",\"scope\":\"{}\",\"base\":{},\"head\":\"{}\",\"index_freshness\":{},\"total_files\":{},\"files\":[{}],\"truncated\":{},\"limits\":{{\"limit\":{}}},\"disclaimer\":\"{DISCLAIMER}\"}}",
        escape(repo),
        req.scope.name(),
        opt_str(req.base.as_deref()),
        escape(&head),
        freshness_json(&freshness),
        total,
        files_json.join(","),
        truncated,
        req.limit
    ))
}

fn opt_str(value: Option<&str>) -> String {
    match value {
        Some(text) => format!("\"{}\"", escape(text)),
        None => "null".to_string(),
    }
}

fn freshness_json(freshness: &Freshness) -> String {
    let reasons: Vec<String> = freshness
        .reasons
        .iter()
        .map(|r| format!("\"{}\"", escape(r)))
        .collect();
    format!(
        "{{\"status\":\"{}\",\"reasons\":[{}]}}",
        escape(&freshness.status),
        reasons.join(",")
    )
}

fn span_json(span: Option<&Span>) -> String {
    match span {
        Some(s) => format!(
            "{{\"start_line\":{},\"end_line\":{}}}",
            s.start_line, s.end_line
        ),
        None => "null".to_string(),
    }
}

fn hunk_json(hunk: &Hunk) -> String {
    format!(
        "{{\"old\":{{\"start\":{},\"count\":{}}},\"new\":{{\"start\":{},\"count\":{}}}}}",
        hunk.old.start, hunk.old.count, hunk.new.start, hunk.new.count
    )
}

/// The `callers` fields of one definition. Every lookup key has an answer from
/// `index_answers`; a key without one is reported as unavailable, never invented.
fn callers_of(def: &DefOut, answers: &BTreeMap<Key, Answer>) -> (String, Option<&'static str>) {
    match &def.lookup {
        None => ("null".to_string(), Some("added")),
        Some(key) => match answers.get(key) {
            Some(Answer::Count(n)) => (format!("{{\"inbound_edges\":{n}}}"), None),
            Some(Answer::Reason(reason)) => ("null".to_string(), Some(*reason)),
            None => ("null".to_string(), Some("index-unavailable")),
        },
    }
}

fn def_json(def: &DefOut, answers: &BTreeMap<Key, Answer>) -> String {
    let (callers, reason) = callers_of(def, answers);
    format!(
        "{{\"name\":\"{}\",\"kind\":\"{}\",\"change\":\"{}\",\"header_touched\":{},\"old_span\":{},\"new_span\":{},\"callers\":{},\"callers_reason\":{}}}",
        escape(&def.name),
        def.kind,
        def.change,
        def.header_touched,
        span_json(def.old_span.as_ref()),
        span_json(def.new_span.as_ref()),
        callers,
        opt_str(reason)
    )
}

fn file_json(file: &FileOut, answers: &BTreeMap<Key, Answer>) -> String {
    let defs: Vec<String> = file.defs.iter().map(|d| def_json(d, answers)).collect();
    let outside: Vec<String> = file.outside.iter().map(hunk_json).collect();
    format!(
        "{{\"path\":\"{}\",\"old_path\":{},\"status\":\"{}\",\"change\":\"{}\",\"reason\":{},\"language\":{},\"symbols\":\"{}\",\"definitions\":[{}],\"hunks_outside_definitions\":[{}],\"hunks\":{}}}",
        escape(&file.path),
        opt_str(file.old_path.as_deref()),
        file.status,
        file.change,
        opt_str(file.reason),
        opt_str(file.language),
        if file.established { "established" } else { "not-established" },
        defs.join(","),
        outside.join(","),
        file.hunks
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: u32, end: u32) -> Span {
        Span {
            start_line: start,
            start_col: 1,
            end_line: end,
            end_col: 1,
        }
    }

    #[test]
    fn parses_hunk_headers_including_omitted_counts() {
        assert_eq!(
            parse_hunk("@@ -3,4 +2,0 @@"),
            Some(Hunk {
                old: Range { start: 3, count: 4 },
                new: Range { start: 2, count: 0 },
            })
        );
        assert_eq!(
            parse_hunk("@@ -5 +7 @@ def alpha"),
            Some(Hunk {
                old: Range { start: 5, count: 1 },
                new: Range { start: 7, count: 1 },
            })
        );
        assert_eq!(
            parse_hunk("@@ -0,0 +1,3 @@"),
            Some(Hunk {
                old: Range { start: 0, count: 0 },
                new: Range { start: 1, count: 3 },
            })
        );
        assert_eq!(parse_hunk("Binary files a/x and b/x differ"), None);
    }

    #[test]
    fn pure_deletion_touches_only_strictly_inside_its_span() {
        // Definition spans lines 5..=9; a deletion after line p touches when 5 <= p < 9.
        let deletion = |p: u32| Hunk {
            old: Range { start: 1, count: 1 },
            new: Range { start: p, count: 0 },
        };
        let def = span(5, 9);
        assert!(!touches(&deletion(4), Some(&def), None));
        assert!(touches(&deletion(5), Some(&def), None));
        assert!(touches(&deletion(8), Some(&def), None));
        assert!(!touches(&deletion(9), Some(&def), None));
    }

    #[test]
    fn duplicate_names_pair_in_source_order() {
        let def = |line: u32| Def {
            kind: "function",
            name: "f".to_string(),
            span: span(line, line),
        };
        let old = [def(1), def(10)];
        let new = [def(3), def(12), def(20)];
        let shown: Vec<String> = pair_defs(&old, &new)
            .iter()
            .map(|p| match p {
                Pair::Both(o, n) => format!("both {} {}", o.span.start_line, n.span.start_line),
                Pair::NewOnly(n) => format!("new {}", n.span.start_line),
                Pair::OldOnly(o) => format!("old {}", o.span.start_line),
            })
            .collect();
        assert_eq!(shown, ["both 1 3", "both 10 12", "new 20"]);
    }

    #[test]
    fn refuses_option_like_refs_and_scope_base_mismatch() {
        assert!(request("compare", Some("-x"), DEFAULT_LIMIT).is_err());
        assert!(request("compare", Some("--output=x"), DEFAULT_LIMIT).is_err());
        assert!(request("compare", None, DEFAULT_LIMIT).is_err());
        assert!(request("all", Some("HEAD"), DEFAULT_LIMIT).is_err());
        assert!(request("compare", Some("HEAD"), DEFAULT_LIMIT).is_ok());
        assert!(request("all", None, 0).is_err());
    }

    #[test]
    fn parses_git_versions_against_the_minimum() {
        assert_eq!(
            parse_git_version("git version 2.53.0.windows.2\n"),
            Some((2, 53))
        );
        assert!(parse_git_version("git version 2.30.9").unwrap() < MIN_GIT);
        assert!(parse_git_version("git version 2.31.0").unwrap() >= MIN_GIT);
        assert_eq!(parse_git_version("not git"), None);
    }

    #[test]
    fn a_name_that_is_not_utf8_is_flagged_and_never_a_git_argument() {
        let changes = parse_raw(b":100644 100644 aaa bbb M\0bad\xff.py\0").unwrap();
        assert_eq!(changes.len(), 1);
        assert!(changes[0].non_utf8);
        assert!(changes[0].path.contains('\u{fffd}'));
    }

    #[test]
    fn raw_records_mark_submodules_and_merge_conflicts() {
        let changes =
            parse_raw(b":160000 160000 aaa bbb M\0sub\0:000000 100644 0000 ccc U\0m.py\0").unwrap();
        assert_eq!(changes.len(), 2);
        assert!(changes[0].gitlink && !changes[0].unmerged);
        assert!(changes[1].unmerged && !changes[1].gitlink);
        assert_eq!(changes[1].kind, Kind::Modified);
    }

    #[test]
    fn an_empty_untracked_file_has_no_hunk() {
        assert_eq!(whole_file_hunk(b""), None);
        assert_eq!(whole_file_hunk(b"x\n").map(|h| h.new.count), Some(1));
        assert_eq!(whole_file_hunk(b"x").map(|h| h.new.count), Some(1));
    }

    #[test]
    fn a_conflicted_name_that_is_not_utf8_keeps_its_flag() {
        let paths = parse_unmerged(b"100644 aaa 1\tbad\xff.py\x00100644 aaa 2\tbad\xff.py\x00");
        assert_eq!(paths.len(), 1);
        assert_eq!(paths.values().copied().collect::<Vec<bool>>(), vec![false]);
        let utf8 = parse_unmerged(b"100644 aaa 1\tgood.py\x00");
        assert_eq!(utf8.get("good.py"), Some(&true));
    }

    #[test]
    fn a_symlink_mode_marks_the_side_without_a_submodule_flag() {
        let changes = parse_raw(b":100644 120000 aaa bbb T\0m.py\0").unwrap();
        assert_eq!(changes.len(), 1);
        assert!(changes[0].symlink && !changes[0].gitlink);
        assert_eq!(changes[0].kind, Kind::Modified);
    }
}
