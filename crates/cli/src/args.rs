// SPDX-License-Identifier: Apache-2.0
//! CLI argument parsing into typed commands. No external CLI library (M1 is
//! std-only); both `--flag value` and `--flag=value` forms are accepted.

#[derive(Debug, PartialEq)]
pub enum Command {
    Help,
    Version,
    Status,
    Init {
        repo: String,
        name: Option<String>,
    },
    RepoList,
    RepoStatus {
        repo: String,
    },
    Doctor {
        repo: Option<String>,
    },
    Scan {
        repo: String,
        paths: Vec<String>,
        symbol: Option<String>,
        progress: bool,
    },
    Index {
        repo: String,
        full: bool,
        progress: bool,
    },
    IndexStatus {
        repo: String,
    },
    Search {
        repo: String,
        symbol: String,
        limit: Option<String>,
        offset: Option<String>,
        /// Also list the name occurrences of the exact symbol name.
        occurrences: bool,
    },
    Semantic {
        op: SemanticOp,
    },
    Edge {
        op: EdgeOp,
    },
    Trace(TraceArgs),
    Impact(TraceArgs),
    Context(ContextArgs),
    Knowledge {
        op: KnowledgeOp,
    },
    Mcp,
}

#[derive(Debug, PartialEq)]
pub struct ContextArgs {
    pub repo: String,
    pub path: Option<String>,
    pub line: Option<String>,
    pub col: Option<String>,
    pub target: Option<String>,
    pub in_path: Option<String>,
    pub kind: Option<String>,
    pub symbol: Option<String>,
    pub query: Option<String>,
    pub semantic: bool,
    pub depth: Option<String>,
    pub limit: Option<String>,
    pub max_bytes: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct TraceArgs {
    pub repo: String,
    pub path: Option<String>,
    pub line: Option<String>,
    pub col: Option<String>,
    pub target: Option<String>,
    pub in_path: Option<String>,
    pub kind: Option<String>,
    pub symbol: Option<String>,
    pub to: Option<String>,
    pub depth: Option<String>,
    pub limit: Option<String>,
    pub offset: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum EdgeOp {
    Callers {
        repo: String,
        path: Option<String>,
        line: Option<String>,
        col: Option<String>,
        target: Option<String>,
        in_path: Option<String>,
        kind: Option<String>,
        symbol: Option<String>,
        limit: Option<String>,
        offset: Option<String>,
    },
    Callees {
        repo: String,
        path: Option<String>,
        line: Option<String>,
        col: Option<String>,
        target: Option<String>,
        in_path: Option<String>,
        kind: Option<String>,
        symbol: Option<String>,
        limit: Option<String>,
        offset: Option<String>,
    },
}

#[derive(Debug, PartialEq)]
pub enum SemanticOp {
    Import {
        repo: String,
        scip: String,
        claim: Option<String>,
    },
    Status {
        repo: String,
    },
    Query {
        repo: String,
        path: String,
        line: String,
        col: String,
        refs: bool,
        lsp_server: Option<String>,
    },
}

/// M7 knowledge subcommands. Value flags are kept as raw strings and parsed in
/// `main.rs` so argument errors are distinguishable from storage errors by
/// exit code.
#[derive(Debug, PartialEq)]
pub enum KnowledgeOp {
    VectorBuild {
        repo: String,
        model_dir: String,
    },
    VectorStatus {
        repo: String,
    },
    VectorClear {
        repo: String,
    },
    Create {
        repo: String,
        input: String,
    },
    Show {
        repo: String,
        id: String,
        revision: Option<String>,
    },
    Update {
        repo: String,
        id: String,
        expected_revision: String,
        input: String,
    },
    History {
        repo: String,
        id: String,
        limit: Option<String>,
        offset: Option<String>,
        generation: Option<String>,
    },
    Search {
        repo: String,
        query: Option<String>,
        path: Option<String>,
        kind: Option<String>,
        semantic: bool,
        include_archived: bool,
        include_superseded: bool,
        include_retracted: bool,
        limit: Option<String>,
        offset: Option<String>,
        generation: Option<String>,
    },
    Link {
        repo: String,
        input: String,
    },
    Unlink {
        repo: String,
        relation: String,
        input: String,
    },
    Bind {
        repo: String,
        path: String,
        line: String,
        col: String,
        symbol: Option<String>,
        input: String,
    },
    Unbind {
        repo: String,
        binding: String,
        input: String,
    },
    At {
        repo: String,
        path: String,
        line: String,
        col: String,
        symbol: Option<String>,
        limit: Option<String>,
        offset: Option<String>,
        generation: Option<String>,
    },
    Related {
        repo: String,
        id: String,
        revision: Option<String>,
        direction: String,
        include_retracted: bool,
        limit: Option<String>,
        offset: Option<String>,
        generation: Option<String>,
    },
    Export {
        repo: String,
        format: Option<String>,
        out: Option<String>,
    },
    Import {
        repo: String,
        file: String,
    },
    Backup {
        repo: String,
        out: String,
    },
    Restore {
        repo: String,
        file: String,
        overwrite: bool,
    },
    Verify {
        repo: String,
    },
}

/// Parse full argv (including argv[0]-style first token). `Err` is the usage
/// text; callers map it to exit code 2.
pub fn parse(args: &[String]) -> Result<Command, String> {
    if args.is_empty() {
        return Ok(Command::Help);
    }
    match args[0].as_str() {
        "-h" | "--help" | "help" if args.len() == 1 => Ok(Command::Help),
        "-V" | "--version" | "version" if args.len() == 1 => Ok(Command::Version),
        "status" if args.len() == 1 => Ok(Command::Status),
        "mcp" if args.len() == 1 => Ok(Command::Mcp),
        "init" => {
            let mut repo = None;
            let mut name = None;
            let mut i = 1;
            while i < args.len() {
                if let Some(v) = take_value(args, &mut i, "--repo") {
                    repo = Some(v);
                } else if let Some(v) = take_value(args, &mut i, "--name") {
                    name = Some(v);
                } else {
                    return Err(crate::usage());
                }
            }
            match repo {
                Some(repo) => Ok(Command::Init { repo, name }),
                None => Err(crate::usage()),
            }
        }
        "repo" => match args.get(1).map(String::as_str) {
            Some("list") if args.len() == 2 => Ok(Command::RepoList),
            Some("status") => {
                let mut repo = None;
                let mut i = 2;
                while i < args.len() {
                    if let Some(v) = take_value(args, &mut i, "--repo") {
                        repo = Some(v);
                    } else {
                        return Err(crate::usage());
                    }
                }
                match repo {
                    Some(repo) => Ok(Command::RepoStatus { repo }),
                    None => Err(crate::usage()),
                }
            }
            _ => Err(crate::usage()),
        },
        "doctor" => {
            let mut repo = None;
            let mut i = 1;
            while i < args.len() {
                if let Some(v) = take_value(args, &mut i, "--repo") {
                    repo = Some(v);
                } else {
                    return Err(crate::usage());
                }
            }
            Ok(Command::Doctor { repo })
        }
        "scan" => {
            let mut repo = None;
            let mut paths = Vec::new();
            let mut symbol = None;
            let mut progress = false;
            let mut i = 1;
            while i < args.len() {
                if args[i] == "--progress" {
                    progress = true;
                    i += 1;
                } else if let Some(v) = take_value(args, &mut i, "--repo") {
                    repo = Some(v);
                } else if let Some(v) = take_value(args, &mut i, "--path") {
                    paths.push(v);
                } else if let Some(v) = take_value(args, &mut i, "--symbol") {
                    symbol = Some(v);
                } else {
                    return Err(crate::usage());
                }
            }
            match repo {
                Some(repo) => Ok(Command::Scan {
                    repo,
                    paths,
                    symbol,
                    progress,
                }),
                None => Err(crate::usage()),
            }
        }
        "index" => {
            let (subcommand, mut i) = match args.get(1).map(String::as_str) {
                Some("status") => (true, 2),
                _ => (false, 1),
            };
            let mut repo = None;
            let mut full = false;
            let mut progress = false;
            while i < args.len() {
                if args[i] == "--full" && !subcommand {
                    full = true;
                    i += 1;
                } else if args[i] == "--progress" && !subcommand {
                    progress = true;
                    i += 1;
                } else if let Some(v) = take_value(args, &mut i, "--repo") {
                    repo = Some(v);
                } else {
                    return Err(crate::usage());
                }
            }
            match repo {
                Some(repo) if subcommand => Ok(Command::IndexStatus { repo }),
                Some(repo) => Ok(Command::Index {
                    repo,
                    full,
                    progress,
                }),
                None => Err(crate::usage()),
            }
        }
        "search" => {
            let mut repo = None;
            let mut symbol = None;
            let mut limit = None;
            let mut offset = None;
            let mut occurrences = false;
            let mut i = 1;
            while i < args.len() {
                if args[i] == "--occurrences" {
                    occurrences = true;
                    i += 1;
                } else if let Some(v) = take_value(args, &mut i, "--repo") {
                    repo = Some(v);
                } else if let Some(v) = take_value(args, &mut i, "--symbol") {
                    symbol = Some(v);
                } else if let Some(v) = take_value(args, &mut i, "--limit") {
                    limit = Some(v);
                } else if let Some(v) = take_value(args, &mut i, "--offset") {
                    offset = Some(v);
                } else {
                    return Err(crate::usage());
                }
            }
            match (repo, symbol) {
                (Some(repo), Some(symbol)) => Ok(Command::Search {
                    repo,
                    symbol,
                    limit,
                    offset,
                    occurrences,
                }),
                _ => Err(crate::usage()),
            }
        }
        "semantic" => match args.get(1).map(String::as_str) {
            Some("import") => {
                let mut repo = None;
                let mut scip = None;
                let mut claim = None;
                let mut i = 2;
                while i < args.len() {
                    if let Some(v) = take_value(args, &mut i, "--repo") {
                        repo = Some(v);
                    } else if let Some(v) = take_value(args, &mut i, "--scip") {
                        scip = Some(v);
                    } else if let Some(v) = take_value(args, &mut i, "--source-digest") {
                        claim = Some(v);
                    } else {
                        return Err(crate::usage());
                    }
                }
                match (repo, scip) {
                    (Some(repo), Some(scip)) => Ok(Command::Semantic {
                        op: SemanticOp::Import { repo, scip, claim },
                    }),
                    _ => Err(crate::usage()),
                }
            }
            Some("status") => {
                let mut repo = None;
                let mut i = 2;
                while i < args.len() {
                    if let Some(v) = take_value(args, &mut i, "--repo") {
                        repo = Some(v);
                    } else {
                        return Err(crate::usage());
                    }
                }
                match repo {
                    Some(repo) => Ok(Command::Semantic {
                        op: SemanticOp::Status { repo },
                    }),
                    None => Err(crate::usage()),
                }
            }
            Some("query") => {
                let mut repo = None;
                let mut path = None;
                let mut line = None;
                let mut col = None;
                let mut refs = false;
                let mut lsp_server = None;
                let mut i = 2;
                while i < args.len() {
                    if let Some(v) = take_value(args, &mut i, "--repo") {
                        repo = Some(v);
                    } else if let Some(v) = take_value(args, &mut i, "--path") {
                        path = Some(v);
                    } else if let Some(v) = take_value(args, &mut i, "--line") {
                        line = Some(v);
                    } else if let Some(v) = take_value(args, &mut i, "--col") {
                        col = Some(v);
                    } else if args[i] == "--refs" {
                        refs = true;
                        i += 1;
                    } else if let Some(v) = take_value(args, &mut i, "--lsp-server") {
                        lsp_server = Some(v);
                    } else {
                        return Err(crate::usage());
                    }
                }
                match (repo, path, line, col) {
                    (Some(repo), Some(path), Some(line), Some(col)) => Ok(Command::Semantic {
                        op: SemanticOp::Query {
                            repo,
                            path,
                            line,
                            col,
                            refs,
                            lsp_server,
                        },
                    }),
                    _ => Err(crate::usage()),
                }
            }
            _ => Err(crate::usage()),
        },
        "callers" => parse_edge_args(args, true).map(|op| Command::Edge { op }),
        "callees" => parse_edge_args(args, false).map(|op| Command::Edge { op }),
        "trace" => parse_trace_args(args).map(Command::Trace),
        "impact" => {
            let parsed = parse_trace_args(args)?;
            if parsed.to.is_some() {
                return Err(crate::usage());
            }
            Ok(Command::Impact(parsed))
        }
        "context" => parse_context_args(args).map(Command::Context),
        "knowledge" => match args.get(1).map(String::as_str) {
            Some(sub) => parse_knowledge_op(sub, args).map(|op| Command::Knowledge { op }),
            None => Err(crate::usage()),
        },
        _ => Err(crate::usage()),
    }
}

fn parse_context_args(args: &[String]) -> Result<ContextArgs, String> {
    let mut repo = None;
    let mut path = None;
    let mut line = None;
    let mut col = None;
    let mut target = None;
    let mut in_path = None;
    let mut kind = None;
    let mut symbol = None;
    let mut query = None;
    let mut semantic = false;
    let mut depth = None;
    let mut limit = None;
    let mut max_bytes = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--semantic" {
            semantic = true;
            i += 1;
        } else if let Some(v) = take_value(args, &mut i, "--repo") {
            repo = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--path") {
            path = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--line") {
            line = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--col") {
            col = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--target") {
            target = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--in") {
            in_path = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--kind") {
            kind = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--symbol") {
            symbol = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--query") {
            query = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--depth") {
            depth = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--limit") {
            limit = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--max-bytes") {
            max_bytes = Some(v);
        } else {
            return Err(crate::usage());
        }
    }
    match repo {
        Some(repo) if anchor_is_valid(&path, &line, &col, &target, &in_path, &kind) => {
            Ok(ContextArgs {
                repo,
                path,
                line,
                col,
                target,
                in_path,
                kind,
                symbol,
                query,
                semantic,
                depth,
                limit,
                max_bytes,
            })
        }
        _ => Err(crate::usage()),
    }
}

/// A command anchors either at `--path/--line/--col` or at a name
/// (`--target`, narrowed by `--in`/`--kind`), never both; a blank target
/// names nothing.
fn anchor_is_valid(
    path: &Option<String>,
    line: &Option<String>,
    col: &Option<String>,
    target: &Option<String>,
    in_path: &Option<String>,
    kind: &Option<String>,
) -> bool {
    match target {
        Some(target) => {
            path.is_none()
                && line.is_none()
                && col.is_none()
                && !target.trim().is_empty()
                && in_path.as_ref().is_none_or(|p| !p.trim().is_empty())
                && kind.as_ref().is_none_or(|k| !k.trim().is_empty())
        }
        None => {
            path.is_some() && line.is_some() && col.is_some() && in_path.is_none() && kind.is_none()
        }
    }
}

fn parse_trace_args(args: &[String]) -> Result<TraceArgs, String> {
    let mut values = std::collections::BTreeMap::new();
    let mut i = 1;
    while i < args.len() {
        let mut matched = false;
        for flag in [
            "--repo", "--path", "--line", "--col", "--target", "--in", "--kind", "--symbol",
            "--to", "--depth", "--limit", "--offset",
        ] {
            if let Some(value) = take_value(args, &mut i, flag) {
                values.insert(flag, value);
                matched = true;
                break;
            }
        }
        if !matched {
            return Err(crate::usage());
        }
    }
    let repo = values.remove("--repo").ok_or_else(crate::usage)?;
    let path = values.remove("--path");
    let line = values.remove("--line");
    let col = values.remove("--col");
    let target = values.remove("--target");
    let in_path = values.remove("--in");
    let kind = values.remove("--kind");
    if !anchor_is_valid(&path, &line, &col, &target, &in_path, &kind) {
        return Err(crate::usage());
    }
    Ok(TraceArgs {
        repo,
        path,
        line,
        col,
        target,
        in_path,
        kind,
        symbol: values.remove("--symbol"),
        to: values.remove("--to"),
        depth: values.remove("--depth"),
        limit: values.remove("--limit"),
        offset: values.remove("--offset"),
    })
}

/// Shared `callers`/`callees` flag parsing: anchored position plus optional
/// identity check and bounded pagination. `Err` is the usage text (exit 2).
fn parse_edge_args(args: &[String], callers: bool) -> Result<EdgeOp, String> {
    let mut repo = None;
    let mut path = None;
    let mut line = None;
    let mut col = None;
    let mut target = None;
    let mut in_path = None;
    let mut kind = None;
    let mut symbol = None;
    let mut limit = None;
    let mut offset = None;
    let mut i = 1;
    while i < args.len() {
        if let Some(v) = take_value(args, &mut i, "--repo") {
            repo = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--path") {
            path = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--line") {
            line = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--col") {
            col = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--target") {
            target = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--in") {
            in_path = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--kind") {
            kind = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--symbol") {
            symbol = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--limit") {
            limit = Some(v);
        } else if let Some(v) = take_value(args, &mut i, "--offset") {
            offset = Some(v);
        } else {
            return Err(crate::usage());
        }
    }
    match repo {
        Some(repo) if anchor_is_valid(&path, &line, &col, &target, &in_path, &kind) => {
            if callers {
                Ok(EdgeOp::Callers {
                    repo,
                    path,
                    line,
                    col,
                    target,
                    in_path,
                    kind,
                    symbol,
                    limit,
                    offset,
                })
            } else {
                Ok(EdgeOp::Callees {
                    repo,
                    path,
                    line,
                    col,
                    target,
                    in_path,
                    kind,
                    symbol,
                    limit,
                    offset,
                })
            }
        }
        _ => Err(crate::usage()),
    }
}

/// Consume `--flag value` / `--flag=value` for `flags` plus bare boolean
/// tokens for `booleans`, returning value flags in a map and set booleans.
fn knowledge_flags(
    args: &[String],
    start: usize,
    flags: &[&str],
    booleans: &[&str],
) -> Result<
    (
        std::collections::BTreeMap<String, String>,
        std::collections::BTreeSet<String>,
    ),
    String,
> {
    let mut values = std::collections::BTreeMap::new();
    let mut set = std::collections::BTreeSet::new();
    let mut i = start;
    while i < args.len() {
        if booleans.contains(&args[i].as_str()) {
            set.insert(args[i].clone());
            i += 1;
            continue;
        }
        let mut matched = false;
        for flag in flags {
            if let Some(value) = take_value(args, &mut i, flag) {
                values.insert((*flag).to_string(), value);
                matched = true;
                break;
            }
        }
        if !matched {
            return Err(crate::usage());
        }
    }
    Ok((values, set))
}

fn knowledge_value(
    values: &std::collections::BTreeMap<String, String>,
    flag: &str,
) -> Result<String, String> {
    values.get(flag).cloned().ok_or_else(crate::usage)
}

fn parse_knowledge_op(sub: &str, args: &[String]) -> Result<KnowledgeOp, String> {
    match sub {
        "vector" => match args.get(2).map(String::as_str) {
            Some("build") => {
                let (v, _) = knowledge_flags(args, 3, &["--repo", "--model-dir"], &[])?;
                Ok(KnowledgeOp::VectorBuild {
                    repo: knowledge_value(&v, "--repo")?,
                    model_dir: knowledge_value(&v, "--model-dir")?,
                })
            }
            Some("status") => {
                let (v, _) = knowledge_flags(args, 3, &["--repo"], &[])?;
                Ok(KnowledgeOp::VectorStatus {
                    repo: knowledge_value(&v, "--repo")?,
                })
            }
            Some("clear") => {
                let (v, _) = knowledge_flags(args, 3, &["--repo"], &[])?;
                Ok(KnowledgeOp::VectorClear {
                    repo: knowledge_value(&v, "--repo")?,
                })
            }
            _ => Err(crate::usage()),
        },
        "create" => {
            let (v, _) = knowledge_flags(args, 2, &["--repo", "--input"], &[])?;
            Ok(KnowledgeOp::Create {
                repo: knowledge_value(&v, "--repo")?,
                input: knowledge_value(&v, "--input")?,
            })
        }
        "show" => {
            let (v, _) = knowledge_flags(args, 2, &["--repo", "--id", "--revision"], &[])?;
            Ok(KnowledgeOp::Show {
                repo: knowledge_value(&v, "--repo")?,
                id: knowledge_value(&v, "--id")?,
                revision: v.get("--revision").cloned(),
            })
        }
        "update" => {
            let (v, _) = knowledge_flags(
                args,
                2,
                &["--repo", "--id", "--expected-revision", "--input"],
                &[],
            )?;
            Ok(KnowledgeOp::Update {
                repo: knowledge_value(&v, "--repo")?,
                id: knowledge_value(&v, "--id")?,
                expected_revision: knowledge_value(&v, "--expected-revision")?,
                input: knowledge_value(&v, "--input")?,
            })
        }
        "history" => {
            let (v, _) = knowledge_flags(
                args,
                2,
                &["--repo", "--id", "--limit", "--offset", "--generation"],
                &[],
            )?;
            Ok(KnowledgeOp::History {
                repo: knowledge_value(&v, "--repo")?,
                id: knowledge_value(&v, "--id")?,
                limit: v.get("--limit").cloned(),
                offset: v.get("--offset").cloned(),
                generation: v.get("--generation").cloned(),
            })
        }
        "search" => {
            let (v, flags) = knowledge_flags(
                args,
                2,
                &[
                    "--repo",
                    "--query",
                    "--path",
                    "--kind",
                    "--limit",
                    "--offset",
                    "--generation",
                ],
                &[
                    "--semantic",
                    "--include-archived",
                    "--include-superseded",
                    "--include-retracted",
                ],
            )?;
            Ok(KnowledgeOp::Search {
                repo: knowledge_value(&v, "--repo")?,
                query: v.get("--query").cloned(),
                path: v.get("--path").cloned(),
                kind: v.get("--kind").cloned(),
                semantic: flags.contains("--semantic"),
                include_archived: flags.contains("--include-archived"),
                include_superseded: flags.contains("--include-superseded"),
                include_retracted: flags.contains("--include-retracted"),
                limit: v.get("--limit").cloned(),
                offset: v.get("--offset").cloned(),
                generation: v.get("--generation").cloned(),
            })
        }
        "link" => {
            let (v, _) = knowledge_flags(args, 2, &["--repo", "--input"], &[])?;
            Ok(KnowledgeOp::Link {
                repo: knowledge_value(&v, "--repo")?,
                input: knowledge_value(&v, "--input")?,
            })
        }
        "unlink" => {
            let (v, _) = knowledge_flags(args, 2, &["--repo", "--relation", "--input"], &[])?;
            Ok(KnowledgeOp::Unlink {
                repo: knowledge_value(&v, "--repo")?,
                relation: knowledge_value(&v, "--relation")?,
                input: knowledge_value(&v, "--input")?,
            })
        }
        "bind" => {
            let (v, _) = knowledge_flags(
                args,
                2,
                &["--repo", "--path", "--line", "--col", "--symbol", "--input"],
                &[],
            )?;
            Ok(KnowledgeOp::Bind {
                repo: knowledge_value(&v, "--repo")?,
                path: knowledge_value(&v, "--path")?,
                line: knowledge_value(&v, "--line")?,
                col: knowledge_value(&v, "--col")?,
                symbol: v.get("--symbol").cloned(),
                input: knowledge_value(&v, "--input")?,
            })
        }
        "unbind" => {
            let (v, _) = knowledge_flags(args, 2, &["--repo", "--binding", "--input"], &[])?;
            Ok(KnowledgeOp::Unbind {
                repo: knowledge_value(&v, "--repo")?,
                binding: knowledge_value(&v, "--binding")?,
                input: knowledge_value(&v, "--input")?,
            })
        }
        "at" => {
            let (v, _) = knowledge_flags(
                args,
                2,
                &[
                    "--repo",
                    "--path",
                    "--line",
                    "--col",
                    "--symbol",
                    "--limit",
                    "--offset",
                    "--generation",
                ],
                &[],
            )?;
            Ok(KnowledgeOp::At {
                repo: knowledge_value(&v, "--repo")?,
                path: knowledge_value(&v, "--path")?,
                line: knowledge_value(&v, "--line")?,
                col: knowledge_value(&v, "--col")?,
                symbol: v.get("--symbol").cloned(),
                limit: v.get("--limit").cloned(),
                offset: v.get("--offset").cloned(),
                generation: v.get("--generation").cloned(),
            })
        }
        "related" => {
            let (v, flags) = knowledge_flags(
                args,
                2,
                &[
                    "--repo",
                    "--id",
                    "--revision",
                    "--direction",
                    "--limit",
                    "--offset",
                    "--generation",
                ],
                &["--include-retracted"],
            )?;
            Ok(KnowledgeOp::Related {
                repo: knowledge_value(&v, "--repo")?,
                id: knowledge_value(&v, "--id")?,
                revision: v.get("--revision").cloned(),
                direction: v
                    .get("--direction")
                    .cloned()
                    .unwrap_or_else(|| "outbound".to_string()),
                include_retracted: flags.contains("--include-retracted"),
                limit: v.get("--limit").cloned(),
                offset: v.get("--offset").cloned(),
                generation: v.get("--generation").cloned(),
            })
        }
        "export" => {
            let (v, _) = knowledge_flags(args, 2, &["--repo", "--format", "--out"], &[])?;
            Ok(KnowledgeOp::Export {
                repo: knowledge_value(&v, "--repo")?,
                format: v.get("--format").cloned(),
                out: v.get("--out").cloned(),
            })
        }
        "import" => {
            let (v, _) = knowledge_flags(args, 2, &["--repo", "--file"], &[])?;
            Ok(KnowledgeOp::Import {
                repo: knowledge_value(&v, "--repo")?,
                file: knowledge_value(&v, "--file")?,
            })
        }
        "backup" => {
            let (v, _) = knowledge_flags(args, 2, &["--repo", "--out"], &[])?;
            Ok(KnowledgeOp::Backup {
                repo: knowledge_value(&v, "--repo")?,
                out: knowledge_value(&v, "--out")?,
            })
        }
        "restore" => {
            let (v, flags) = knowledge_flags(args, 2, &["--repo", "--file"], &["--overwrite"])?;
            Ok(KnowledgeOp::Restore {
                repo: knowledge_value(&v, "--repo")?,
                file: knowledge_value(&v, "--file")?,
                overwrite: flags.contains("--overwrite"),
            })
        }
        "verify" => {
            let (v, _) = knowledge_flags(args, 2, &["--repo"], &[])?;
            Ok(KnowledgeOp::Verify {
                repo: knowledge_value(&v, "--repo")?,
            })
        }
        _ => Err(crate::usage()),
    }
}

/// Consume `--flag value` or `--flag=value` at `args[*i]`; `None` when the
/// token is not that flag (index untouched) or the value is missing.
fn take_value(args: &[String], i: &mut usize, flag: &str) -> Option<String> {
    let token = args.get(*i)?;
    if let Some(rest) = token.strip_prefix(flag) {
        if let Some(value) = rest.strip_prefix('=') {
            *i += 1;
            return Some(value.to_string());
        }
        if rest.is_empty() {
            *i += 1;
            let value = args.get(*i)?.clone();
            *i += 1;
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{parse, Command, KnowledgeOp, SemanticOp};

    fn p(args: &[&str]) -> Result<Command, String> {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_init_with_name_forms() {
        assert!(matches!(
            p(&["init", "--repo", "D:/r", "--name", "r"]),
            Ok(Command::Init { .. })
        ));
        assert!(matches!(
            p(&["init", "--repo=D:/r"]),
            Ok(Command::Init { repo, name: None }) if repo == "D:/r"
        ));
    }

    #[test]
    fn rejects_init_without_repo() {
        assert!(p(&["init"]).is_err());
        assert!(p(&["init", "--repo"]).is_err());
        assert!(p(&["init", "--name", "x"]).is_err());
    }

    #[test]
    fn parses_repo_subcommands() {
        assert!(matches!(p(&["repo", "list"]), Ok(Command::RepoList)));
        assert!(matches!(
            p(&["repo", "status", "--repo", "r"]),
            Ok(Command::RepoStatus { .. })
        ));
        assert!(p(&["repo"]).is_err());
        assert!(p(&["repo", "status"]).is_err());
        assert!(p(&["repo", "drop", "--repo", "r"]).is_err());
    }

    #[test]
    fn parses_doctor_with_optional_repo() {
        assert!(matches!(p(&["doctor"]), Ok(Command::Doctor { repo: None })));
        assert!(matches!(
            p(&["doctor", "--repo", "r"]),
            Ok(Command::Doctor { repo: Some(_) })
        ));
        assert!(p(&["doctor", "--repo"]).is_err());
    }

    #[test]
    fn parses_scan_with_paths_and_symbol() {
        assert!(matches!(
            p(&["scan", "--repo", "r"]),
            Ok(Command::Scan { paths, symbol: None, .. }) if paths.is_empty()
        ));
        assert!(matches!(
            p(&[
                "scan",
                "--repo",
                "r",
                "--path",
                "a.py",
                "--path=b.py",
                "--symbol",
                "f"
            ]),
            Ok(Command::Scan { .. })
        ));
        assert!(p(&["scan"]).is_err());
        assert!(p(&["scan", "--path", "a.py"]).is_err());
    }

    #[test]
    fn parses_semantic_commands() {
        assert!(matches!(
            p(&["semantic", "import", "--repo", "r", "--scip", "i.scip"]),
            Ok(Command::Semantic {
                op: SemanticOp::Import { claim: None, .. }
            })
        ));
        assert!(matches!(
            p(&[
                "semantic",
                "import",
                "--repo",
                "r",
                "--scip",
                "i.scip",
                "--source-digest",
                "ab"
            ]),
            Ok(Command::Semantic {
                op: SemanticOp::Import { claim: Some(_), .. }
            })
        ));
        assert!(matches!(
            p(&["semantic", "status", "--repo", "r"]),
            Ok(Command::Semantic {
                op: SemanticOp::Status { .. }
            })
        ));
        assert!(matches!(
            p(
                &[
                    "semantic", "query", "--repo", "r", "--path", "a.py", "--line", "3", "--col",
                    "5"
                ]
            ),
            Ok(Command::Semantic {
                op: SemanticOp::Query {
                    refs: false,
                    lsp_server: None,
                    ..
                }
            })
        ));
        assert!(matches!(
            p(&[
                "semantic", "query", "--repo", "r", "--path", "a.py", "--line", "3", "--col", "5",
                "--refs"
            ]),
            Ok(Command::Semantic {
                op: SemanticOp::Query { refs: true, .. }
            })
        ));
        // M6 boundary: no traversal flags exist; unknown flags are usage errors.
        assert!(p(&[
            "semantic",
            "query",
            "--repo",
            "r",
            "--path",
            "a.py",
            "--line",
            "3",
            "--col",
            "5",
            "--refs",
            "--refs-depth",
            "2"
        ])
        .is_err());
        assert!(p(&["semantic", "import", "--repo", "r"]).is_err());
        assert!(p(&["semantic", "query", "--repo", "r", "--path", "a.py"]).is_err());
        assert!(p(&["semantic", "frobnicate", "--repo", "r"]).is_err());
        assert!(p(&["semantic"]).is_err());
    }

    #[test]
    fn parses_knowledge_vector_lifecycle_commands() {
        assert!(matches!(
            p(&[
                "knowledge", "vector", "build", "--repo", "r", "--model-dir", "model"
            ]),
            Ok(Command::Knowledge {
                op: KnowledgeOp::VectorBuild { repo, model_dir }
            }) if repo == "r" && model_dir == "model"
        ));
        assert!(matches!(
            p(&["knowledge", "vector", "status", "--repo", "r"]),
            Ok(Command::Knowledge { op: KnowledgeOp::VectorStatus { repo } }) if repo == "r"
        ));
        assert!(matches!(
            p(&["knowledge", "vector", "clear", "--repo", "r"]),
            Ok(Command::Knowledge { op: KnowledgeOp::VectorClear { repo } }) if repo == "r"
        ));
        assert!(p(&["knowledge", "vector", "build", "--repo", "r"]).is_err());
        assert!(p(&["knowledge", "vector", "status"]).is_err());
        assert!(p(&["knowledge", "vector", "unknown", "--repo", "r"]).is_err());
    }

    #[test]
    fn parses_knowledge_search_semantic_option() {
        assert!(matches!(
            p(&[
                "knowledge",
                "search",
                "--repo",
                "r",
                "--query",
                "q",
                "--semantic"
            ]),
            Ok(Command::Knowledge {
                op: KnowledgeOp::Search { semantic: true, .. }
            })
        ));
        assert!(matches!(
            p(&["knowledge", "search", "--repo", "r", "--query", "q"]),
            Ok(Command::Knowledge {
                op: KnowledgeOp::Search {
                    semantic: false,
                    ..
                }
            })
        ));
    }

    #[test]
    fn parses_index_full_flag() {
        assert!(matches!(
            p(&["index", "--repo", "r", "--full"]),
            Ok(Command::Index { repo, full: true, .. }) if repo == "r"
        ));
        assert!(matches!(
            p(&["index", "--repo", "r"]),
            Ok(Command::Index { repo, full: false, .. }) if repo == "r"
        ));
        assert!(p(&["index", "--repo", "r", "--full=x"]).is_err());
        assert!(p(&["index", "--repo", "r", "bogus"]).is_err());
    }

    #[test]
    fn parses_progress_flag_for_scan_and_index() {
        assert!(matches!(
            p(&["scan", "--repo", "r", "--progress"]),
            Ok(Command::Scan { progress: true, .. })
        ));
        assert!(matches!(
            p(&["scan", "--repo", "r"]),
            Ok(Command::Scan {
                progress: false,
                ..
            })
        ));
        assert!(matches!(
            p(&["index", "--repo", "r", "--progress"]),
            Ok(Command::Index { progress: true, .. })
        ));
        assert!(matches!(
            p(&["index", "--repo", "r"]),
            Ok(Command::Index {
                progress: false,
                ..
            })
        ));
        assert!(p(&["index", "status", "--repo", "r", "--progress"]).is_err());
    }

    #[test]
    fn parses_index_and_search() {
        assert!(matches!(
            p(&["index", "--repo", "r"]),
            Ok(Command::Index { .. })
        ));
        assert!(matches!(
            p(&["index", "status", "--repo", "r"]),
            Ok(Command::IndexStatus { .. })
        ));
        assert!(p(&["index"]).is_err());
        assert!(p(&["index", "status"]).is_err());
        assert!(p(&["index", "bogus", "--repo", "r"]).is_err());
        assert!(matches!(
            p(&["search", "--repo", "r", "--symbol", "f"]),
            Ok(Command::Search {
                limit: None,
                offset: None,
                ..
            })
        ));
        assert!(matches!(
            p(&[
                "search",
                "--repo",
                "r",
                "--symbol",
                "f",
                "--limit",
                "10",
                "--offset=5"
            ]),
            Ok(Command::Search { .. })
        ));
        assert!(p(&["search", "--repo", "r"]).is_err());
        assert!(p(&["search", "--symbol", "f"]).is_err());
    }

    #[test]
    fn rejects_unknown_commands() {
        assert!(p(&["index"]).is_err());
        assert!(p(&["repo", "list", "extra"]).is_err());
        assert!(p(&["--bogus"]).is_err());
    }

    #[test]
    fn parses_knowledge_subcommands_and_flag_forms() {
        assert!(matches!(
            p(&["knowledge", "create", "--repo", "r", "--input", "-"]),
            Ok(Command::Knowledge { .. })
        ));
        assert!(matches!(
            p(&["knowledge", "create", "--repo=r", "--input=-"]),
            Ok(Command::Knowledge { .. })
        ));
        assert!(matches!(
            p(&[
                "knowledge",
                "show",
                "--repo",
                "r",
                "--id",
                "d1",
                "--revision",
                "2"
            ]),
            Ok(Command::Knowledge { .. })
        ));
        assert!(matches!(
            p(&[
                "knowledge",
                "search",
                "--repo",
                "r",
                "--query",
                "x",
                "--include-archived",
                "--include-superseded",
                "--include-retracted"
            ]),
            Ok(Command::Knowledge { .. })
        ));
        assert!(matches!(
            p(&[
                "knowledge",
                "related",
                "--repo",
                "r",
                "--id",
                "d1",
                "--direction",
                "inbound",
                "--include-retracted"
            ]),
            Ok(Command::Knowledge { .. })
        ));
        // Missing required flags and unknown subcommands are usage errors.
        assert!(p(&["knowledge", "create", "--repo", "r"]).is_err());
        assert!(p(&["knowledge", "show", "--repo", "r"]).is_err());
        assert!(p(&[
            "knowledge",
            "update",
            "--repo",
            "r",
            "--id",
            "d1",
            "--input",
            "-"
        ])
        .is_err());
        // Search parses with no query: text-or-path is enforced at runtime so
        // the refusal carries a specific message, not generic usage.
        assert!(matches!(
            p(&["knowledge", "search", "--repo", "r"]),
            Ok(Command::Knowledge { .. })
        ));
        assert!(matches!(
            p(&[
                "knowledge",
                "search",
                "--repo",
                "r",
                "--path",
                "src/main.rs"
            ]),
            Ok(Command::Knowledge { .. })
        ));
        assert!(p(&["knowledge", "frobnicate", "--repo", "r"]).is_err());
        assert!(p(&["knowledge"]).is_err());
    }

    #[test]
    fn parses_knowledge_anchor_pagination_flags() {
        assert!(matches!(
            p(&[
                "knowledge",
                "at",
                "--repo",
                "r",
                "--path",
                "src/lib.rs",
                "--line",
                "1",
                "--col",
                "1",
                "--limit",
                "1",
                "--offset",
                "1",
                "--generation",
                "2"
            ]),
            Ok(Command::Knowledge { .. })
        ));
    }

    #[test]
    fn parses_bind_symbol_as_value() {
        for symbol in ["--symbol", "--symbol=scip-sym"] {
            let mut args = vec![
                "knowledge",
                "bind",
                "--repo",
                "r",
                "--path",
                "a.rs",
                "--line",
                "1",
                "--col",
                "1",
                symbol,
            ];
            if symbol == "--symbol" {
                args.push("scip-sym");
            }
            args.extend(["--input", "-"]);
            assert!(
                matches!(p(&args), Ok(Command::Knowledge { op: KnowledgeOp::Bind { symbol: Some(s), .. } }) if s == "scip-sym")
            );
        }
    }

    #[test]
    fn parses_at_symbol_as_value() {
        for symbol in ["--symbol", "--symbol=scip-sym"] {
            let mut args = vec![
                "knowledge",
                "at",
                "--repo",
                "r",
                "--path",
                "a.rs",
                "--line",
                "1",
                "--col",
                "1",
                symbol,
            ];
            if symbol == "--symbol" {
                args.push("scip-sym");
            }
            assert!(
                matches!(p(&args), Ok(Command::Knowledge { op: KnowledgeOp::At { symbol: Some(s), .. } }) if s == "scip-sym")
            );
        }
    }
}
