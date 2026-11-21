// SPDX-License-Identifier: Apache-2.0
//! Guided, approval-gated project start.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::{agent_setup, index, overview, packet, paths, registry, response, store};

struct Options {
    root: PathBuf,
    name: Option<String>,
    client: Option<agent_setup::Client>,
    json: bool,
}

pub(crate) fn interactive(stdin_terminal: bool, stdout_terminal: bool, json: bool) -> bool {
    stdin_terminal && stdout_terminal && !json
}

fn parse(args: &[String], cwd: &Path) -> Result<Options, String> {
    let mut root = cwd.to_path_buf();
    let mut name = None;
    let mut client = None;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        let (flag, value, inline) = args[i]
            .split_once('=')
            .map_or((args[i].as_str(), None, false), |(flag, value)| {
                (flag, Some(value), true)
            });
        let value = match (flag, value) {
            ("--path", Some(value)) => value,
            ("--path", None) if i + 1 < args.len() => {
                i += 1;
                &args[i]
            }
            ("--name", Some(value)) => {
                name = Some(value.to_owned());
                i += 1;
                continue;
            }
            ("--name", None) if i + 1 < args.len() => {
                i += 1;
                name = Some(args[i].clone());
                i += 1;
                continue;
            }
            ("--client", Some(value)) => {
                client = Some(parse_client(value)?);
                i += 1;
                continue;
            }
            ("--client", None) if i + 1 < args.len() => {
                i += 1;
                client = Some(parse_client(&args[i])?);
                i += 1;
                continue;
            }
            ("--json", None) => {
                json = true;
                i += 1;
                continue;
            }
            _ => return Err("unknown or incomplete start option".into()),
        };
        if value.is_empty() {
            return Err(format!("{flag} requires a nonempty value"));
        }
        root = PathBuf::from(value);
        i += 1;
        if inline {
            continue;
        }
    }
    Ok(Options {
        root,
        name,
        client,
        json,
    })
}

fn parse_client(value: &str) -> Result<agent_setup::Client, String> {
    match value {
        "codex" => Ok(agent_setup::Client::Codex),
        "claude" => Ok(agent_setup::Client::Claude),
        "opencode" => Ok(agent_setup::Client::OpenCode),
        _ => Err("--client must be codex, claude, or opencode".into()),
    }
}

fn confirm(
    input: &mut impl BufRead,
    output: &mut impl Write,
    prompt: &str,
) -> std::io::Result<bool> {
    write!(output, "{prompt} [y/N] ")?;
    output.flush()?;
    let mut answer = String::new();
    if input.read_line(&mut answer)? == 0 {
        return Ok(false);
    }
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

fn fail(code: i32, message: impl std::fmt::Display, error: &mut String) -> i32 {
    error.push_str(&format!("error: {message}\n"));
    code
}

/// The injected terminal bits, paths, and streams are an internal test seam;
/// production derives terminal state directly from stdin/stdout.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_with(
    args: &[String],
    stdin_terminal: bool,
    stdout_terminal: bool,
    home: &Path,
    cwd: &Path,
    input: &mut impl BufRead,
    output: &mut impl Write,
    error: &mut String,
) -> i32 {
    run_with_hook(
        args,
        stdin_terminal,
        stdout_terminal,
        home,
        cwd,
        input,
        output,
        error,
        || {},
    )
}

#[allow(clippy::too_many_arguments)]
fn run_with_hook(
    args: &[String],
    stdin_terminal: bool,
    stdout_terminal: bool,
    home: &Path,
    cwd: &Path,
    input: &mut impl BufRead,
    output: &mut impl Write,
    error: &mut String,
    after_selection: impl FnMut(),
) -> i32 {
    run_with_hooks(
        args,
        stdin_terminal,
        stdout_terminal,
        home,
        cwd,
        input,
        output,
        error,
        after_selection,
        || {},
        crate::mcp::in_process_self_test,
    )
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn run_with_packet_hook(
    args: &[String],
    stdin_terminal: bool,
    stdout_terminal: bool,
    home: &Path,
    cwd: &Path,
    input: &mut impl BufRead,
    output: &mut impl Write,
    error: &mut String,
    after_selection: impl FnMut(),
    before_packet_query: impl FnMut(),
) -> i32 {
    run_with_hooks(
        args,
        stdin_terminal,
        stdout_terminal,
        home,
        cwd,
        input,
        output,
        error,
        after_selection,
        before_packet_query,
        crate::mcp::in_process_self_test,
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn run_with_probe_hook(
    args: &[String],
    home: &Path,
    cwd: &Path,
    input: &mut impl BufRead,
    output: &mut impl Write,
    error: &mut String,
    probe: impl FnMut() -> Result<(), String>,
) -> i32 {
    run_with_hooks(
        args,
        true,
        true,
        home,
        cwd,
        input,
        output,
        error,
        || {},
        || {},
        probe,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_with_hooks(
    args: &[String],
    stdin_terminal: bool,
    stdout_terminal: bool,
    home: &Path,
    cwd: &Path,
    input: &mut impl BufRead,
    output: &mut impl Write,
    error: &mut String,
    mut after_selection: impl FnMut(),
    mut before_packet_query: impl FnMut(),
    run_probe: impl FnMut() -> Result<(), String>,
) -> i32 {
    let options = match parse(args, cwd) {
        Ok(options) => options,
        Err(e) => return fail(2, e, error),
    };
    let interactive = interactive(stdin_terminal, stdout_terminal, options.json);
    let home = if home.is_absolute() {
        home.to_path_buf()
    } else {
        cwd.join(home)
    };
    let root = match paths::canonical_root(&options.root) {
        Ok(root) => root,
        Err(e) => return fail(2, format!("invalid project directory: {e}"), error),
    };
    let name = match options.name {
        Some(name) => name,
        None => match root.file_name().and_then(|part| part.to_str()) {
            Some(name) => name.to_owned(),
            None => return fail(2, "cannot derive project name; pass --name", error),
        },
    };
    if !paths::validate_name(&name) {
        return fail(2, "invalid project name", error);
    }
    let registry_path = home.join("registry.json");
    let mut reg = match registry::load(&registry_path) {
        Ok(reg) => reg,
        Err(e) => return fail(3, e, error),
    };
    let root_text = root.to_string_lossy().into_owned();
    let registered = match registry::find(&reg, &name) {
        Some(entry) if entry.root == root_text => true,
        Some(_) => {
            return fail(
                2,
                format!("project name '{name}' is registered for another root"),
                error,
            )
        }
        None => false,
    };

    if !interactive {
        let report = match overview::collect(&home, &name, &root) {
            Ok(report) => report,
            Err(e) => return fail(3, e, error),
        };
        return match overview::render(&report) {
            Ok(json) => {
                // JSON/nonterminal mode is deliberately a read-only overview; a
                // numbered choice cannot be made without an interactive stream.
                let _ = writeln!(output, "{json}");
                0
            }
            Err(e) => fail(3, e, error),
        };
    }

    let git_info = crate::git::fingerprint(&root);
    let (_, ignore_info) = crate::ignore::load_rules(&root);
    let checks = crate::doctor::run_checks(&name, &root, &root_text, &git_info, &ignore_info);
    let readiness = crate::doctor::doctor_json(&name, &root_text, &checks);
    let _ = writeln!(output, "Readiness: {readiness}");
    if checks.iter().any(|check| !check.pass) {
        return fail(3, "project readiness checks failed", error);
    }

    if !registered {
        let _ = writeln!(
            output,
            "RILTRA_HOME: {} (registry target: {})",
            home.display(),
            registry_path.display()
        );
        let approved = match confirm(input, output, "Register this project?") {
            Ok(approved) => approved,
            Err(e) => return fail(3, e, error),
        };
        if !approved {
            let _ = writeln!(
                output,
                "{{\"status\":\"cancelled\",\"stage\":\"registration\"}}"
            );
            return 0;
        }
        reg = match registry::load(&registry_path) {
            Ok(reg) => reg,
            Err(e) => return fail(3, e, error),
        };
        match registry::find(&reg, &name) {
            Some(entry) if entry.root == root_text => {}
            Some(_) => {
                return fail(
                    2,
                    format!("project name '{name}' was registered for another root"),
                    error,
                )
            }
            None => {
                let registered_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|time| time.as_secs())
                    .unwrap_or(0);
                if let Err(e) = registry::add(&mut reg, &name, &root_text, registered_at) {
                    return fail(3, e, error);
                }
                if let Err(e) = registry::save(&registry_path, &reg) {
                    return fail(3, e, error);
                }
            }
        }
    }

    let mut report = match overview::collect(&home, &name, &root) {
        Ok(report) => report,
        Err(e) => return fail(3, e, error),
    };
    if !matches!(report.snapshot.status, overview::SnapshotState::Fresh) {
        let index_path = store::db_path(&home, &name);
        let _ = writeln!(
            output,
            "RILTRA_HOME: {} (index target: {})",
            home.display(),
            index_path.display()
        );
        let approved = match confirm(input, output, "Build or update the project index?") {
            Ok(approved) => approved,
            Err(e) => return fail(3, e, error),
        };
        if approved {
            if let Err(e) = index::build(&home, &name, &root, false) {
                return fail(3, e, error);
            }
            report = match overview::collect(&home, &name, &root) {
                Ok(report) => report,
                Err(e) => return fail(3, e, error),
            };
        }
    }

    if let Some(client) = options.client {
        let mut setup = if client == agent_setup::Client::Codex {
            let _ = writeln!(
                output,
                "Codex inspection target: codex mcp get riltra --json (read-only targeted entry)"
            );
            let approved = match confirm_inspection(
                input,
                output,
                "Run the targeted Codex inspection?",
                || {
                    find_executable("codex").map(|executable| {
                        agent_setup::inspect_codex(
                            &executable,
                            &root,
                            &std::env::current_exe().unwrap_or_default(),
                            &home,
                        )
                    })
                },
            ) {
                Ok(result) => result,
                Err(e) => return fail(3, e, error),
            };
            approved.unwrap_or_else(|| {
                agent_setup::manual_preview(
                    client,
                    "codex",
                    &std::env::current_exe().unwrap_or_default(),
                    &home,
                )
            })
        } else {
            inspect_setup(client, &home, &root)
        };
        let _ = writeln!(
            output,
            "Setup preview: {}",
            serde_json::to_string(&setup).unwrap_or_else(|_| "{}".into())
        );
        if setup.status == agent_setup::SetupStatus::Planned {
            let _ = writeln!(output, "RILTRA_HOME setup target: {}", home.display());
            let approved = match confirm(input, output, "Apply the displayed setup plan?") {
                Ok(approved) => approved,
                Err(e) => return fail(3, e, error),
            };
            let (_, sources) = claude_sources(&root);
            let plan = agent_setup::SetupPlan::new(
                setup,
                &sources,
                &std::env::current_exe().unwrap_or_default(),
                &home,
            );
            setup = agent_setup::apply_setup(plan, approved, || {
                let (complete, sources) = claude_sources(&root);
                (complete, sources)
            });
            let _ = writeln!(
                output,
                "Setup result: {}",
                serde_json::to_string(&setup).unwrap_or_else(|_| "{}".into())
            );
        }
        let probe = match confirm_probe(
            input,
            output,
            "Run Riltra's in-process MCP handler probe?",
            run_probe,
        ) {
            Ok(probe) => probe,
            Err(e) => return fail(3, e, error),
        };
        if let Some(result) = probe {
            let succeeded = result.is_ok();
            setup.probe = if succeeded {
                agent_setup::ProbeStatus::Succeeded
            } else {
                agent_setup::ProbeStatus::Failed
            };
            setup.probe_scope = Some(agent_setup::ProbeScope::InProcessHandler);
            setup.in_process_self_test = if succeeded {
                agent_setup::InProcessSelfTestStatus::Succeeded
            } else {
                agent_setup::InProcessSelfTestStatus::Failed
            };
            let failure = result.as_ref().err().cloned();
            let detail = failure.as_ref().map_or_else(
                || "succeeded".to_string(),
                |error| format!("failed: {error}"),
            );
            let _ = writeln!(
                output,
                "Direct probe scope: in_process_handler; probe {detail}; it exercises the handler over a bounded in-memory duplex transport and does not verify the installed executable, OS stdio, or client connection."
            );
            let _ = writeln!(
                output,
                "Setup report after probe: {}",
                serde_json::to_string(&setup).unwrap_or_else(|_| "{}".into())
            );
            if let Some(diagnostic) = failure {
                return fail(
                    3,
                    format!("approved in-process MCP handler probe failed: {diagnostic}"),
                    error,
                );
            }
        }
    }

    loop {
        let _ = writeln!(output, "{}", overview::render(&report).unwrap_or_default());
        if report.definitions.is_empty() {
            return 0;
        }
        for (i, definition) in report.definitions.iter().enumerate() {
            let label = definition
                .symbol_name
                .as_deref()
                .unwrap_or("Unnamed definition");
            let ordinal = report.definitions[..=i]
                .iter()
                .filter(|item| {
                    item.path == definition.path && item.symbol_kind == definition.symbol_kind
                })
                .count();
            let _ = writeln!(
                output,
                "{}. {} ({ordinal}) — {}",
                i + 1,
                label,
                definition.path
            );
        }
        let mut choice = String::new();
        if let Err(e) = write!(output, "Choose a definition number (or q to cancel): ")
            .and_then(|_| output.flush())
        {
            return fail(3, e, error);
        }
        match input.read_line(&mut choice) {
            Ok(0) => return 0,
            Err(e) => return fail(3, e, error),
            _ => {}
        }
        if choice.trim() == "q" || choice.trim() == "Q" {
            return 0;
        }
        let selected_index = match choice.trim().parse::<usize>() {
            Ok(n) if (1..=report.definitions.len()).contains(&n) => n - 1,
            _ => return fail(2, "invalid definition selection", error),
        };
        let selected = &report.definitions[selected_index];
        let ordinal = report.definitions[..=selected_index]
            .iter()
            .filter(|item| item.path == selected.path && item.symbol_kind == selected.symbol_kind)
            .count();
        let identity = (
            report.snapshot.nonce.clone(),
            selected.path.clone(),
            selected.byte_start,
            selected.byte_end,
            selected.symbol_kind.clone(),
            selected.symbol_name.clone(),
            ordinal,
        );
        after_selection();
        let fresh = match overview::collect(&home, &name, &root) {
            Ok(report) => report,
            Err(e) => return fail(3, e, error),
        };
        let same_snapshot = identity.0 == fresh.snapshot.nonce;
        let fresh_group = fresh
            .definitions
            .iter()
            .filter(|item| item.path == identity.1 && item.symbol_kind == identity.4)
            .nth(identity.6 - 1);
        let fresh_selected = fresh_group.filter(|item| {
            item.byte_start == identity.2
                && item.byte_end == identity.3
                && item.symbol_name == identity.5
        });
        if !same_snapshot || fresh_selected.is_none() {
            report = fresh;
            let _ = writeln!(
                output,
                "Index snapshot changed; choose again from the refreshed definition menu."
            );
            continue;
        }
        let selected = fresh_selected.expect("checked above");
        let anchor = match selected_anchor(&root, selected) {
            Ok(anchor) => anchor,
            Err(e) => return fail(3, e, error),
        };
        let request = packet::PacketRequest {
            anchors: vec![anchor],
            depth: crate::context::DEFAULT_DEPTH,
            limit: crate::context::DEFAULT_LIMIT,
            max_bytes: crate::context::DEFAULT_MAX_BYTES,
        };
        let entry = match registry::find(&reg, &name) {
            Some(entry) => entry.clone(),
            None => return fail(3, "project is not registered", error),
        };
        before_packet_query();
        let result = match store::open_read(&store::db_path(&home, &name)) {
            store::IndexRead::Ready(mut conn) => {
                packet::query_conn(&mut conn, &home, &entry, &name, &root, &request)
            }
            store::IndexRead::Missing => {
                return fail(3, "index disappeared before packet generation", error)
            }
            store::IndexRead::Outdated(version) | store::IndexRead::Ahead(version) => {
                return fail(3, format!("index schema v{version} is unsupported"), error)
            }
            store::IndexRead::Error(e) => return fail(3, e, error),
        };
        let result = match result {
            Ok(result) => result,
            Err(e) => return fail(3, e, error),
        };
        if !matches!(
            (result.snapshot.as_ref(), identity.0.as_deref()),
            (Some(snapshot), Some(nonce)) if snapshot.nonce == nonce
        ) {
            report = match overview::collect(&home, &name, &root) {
                Ok(report) => report,
                Err(e) => return fail(3, e, error),
            };
            let _ = writeln!(
                output,
                "Index snapshot changed before packet generation; choose again from the refreshed definition menu."
            );
            continue;
        }
        match response::packet_query_json(&result) {
            Ok(json) => {
                let _ = writeln!(output, "{json}");
                return 0;
            }
            Err(e) => return fail(3, e, error),
        }
    }
}

fn confirm_inspection(
    input: &mut impl BufRead,
    output: &mut impl Write,
    prompt: &str,
    inspect: impl FnOnce() -> Option<agent_setup::SetupReport>,
) -> std::io::Result<Option<agent_setup::SetupReport>> {
    if confirm(input, output, prompt)? {
        Ok(inspect())
    } else {
        Ok(None)
    }
}

fn confirm_probe(
    input: &mut impl BufRead,
    output: &mut impl Write,
    prompt: &str,
    probe: impl FnOnce() -> Result<(), String>,
) -> std::io::Result<Option<Result<(), String>>> {
    if confirm(input, output, prompt)? {
        Ok(Some(probe()))
    } else {
        Ok(None)
    }
}

fn selected_anchor(
    _root: &Path,
    selected: &overview::SourceEvidence,
) -> Result<packet::AnchorInput, String> {
    Ok(packet::AnchorInput {
        path: Some(selected.path.clone()),
        line: Some(selected.line_start.ok_or("definition lacks indexed line")?),
        col: Some(
            selected
                .start_col
                .ok_or("definition lacks indexed column")?,
        ),
        symbol: None,
        target: None,
        in_path: None,
        kind: None,
    })
}

fn inspect_setup(
    client: agent_setup::Client,
    home: &Path,
    root: &Path,
) -> agent_setup::SetupReport {
    let binary = std::env::current_exe().unwrap_or_default();
    match client {
        agent_setup::Client::OpenCode => {
            let config = std::env::var_os("OPENCODE_CONFIG")
                .map(PathBuf::from)
                .or_else(|| {
                    let base = std::env::var_os("XDG_CONFIG_HOME")
                        .map(PathBuf::from)
                        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
                        .or_else(|| {
                            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
                        });
                    base.map(|base| base.join("opencode").join("opencode.json"))
                });
            let sources = config
                .into_iter()
                .map(|path| agent_setup::ConfigSource {
                    path,
                    scope: agent_setup::SetupScope::UserGlobal,
                })
                .collect::<Vec<_>>();
            let mut sources = sources;
            sources.push(agent_setup::ConfigSource {
                path: root.join(".opencode").join("opencode.json"),
                scope: agent_setup::SetupScope::ProjectLocal,
            });
            // JSONC variants and managed layers remain unverified by the existing parser.
            agent_setup::inspect_opencode(&sources, false, &binary, home)
        }
        agent_setup::Client::Claude => {
            let (complete, sources) = claude_sources(root);
            agent_setup::inspect_claude(&sources, complete, &binary, home)
        }
        agent_setup::Client::Codex => agent_setup::manual_preview(client, "codex", &binary, home),
    }
}

fn find_executable(name: &str) -> Option<PathBuf> {
    crate::paths::find_executable_in(name, &crate::paths::path_dirs(), &["", ".exe"])
}

fn claude_sources(root: &Path) -> (bool, Vec<agent_setup::ClaudeConfigSource>) {
    let user_home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    let mut sources = Vec::new();
    if let Some(user_home) = user_home {
        for path in [
            user_home.join(".claude.json"),
            user_home.join(".claude").join("settings.json"),
        ] {
            sources.push(agent_setup::ClaudeConfigSource {
                config: agent_setup::ConfigSource {
                    path,
                    scope: agent_setup::SetupScope::UserGlobal,
                },
                project_root: None,
            });
        }
    }
    for path in [
        root.join(".mcp.json"),
        root.join(".claude").join("settings.json"),
        root.join(".claude").join("settings.local.json"),
    ] {
        sources.push(agent_setup::ClaudeConfigSource {
            config: agent_setup::ConfigSource {
                path,
                scope: agent_setup::SetupScope::ProjectLocal,
            },
            project_root: None,
        });
    }
    // Managed policy can be server-side or OS-deployed and is not observable here.
    (false, sources)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn terminal_policy_requires_both_streams() {
        assert!(!interactive(true, false, false));
        assert!(!interactive(false, true, false));
        assert!(!interactive(true, true, true));
        assert!(interactive(true, true, false));
    }

    #[test]
    fn client_selection_does_not_authorize_writes() {
        let root = crate::testutil::tmpdir("start-readonly");
        let home = crate::testutil::tmpdir("start-readonly-home");
        let args = vec![
            "--path".into(),
            root.display().to_string(),
            "--name".into(),
            "start-readonly".into(),
            "--client".into(),
            "opencode".into(),
            "--json".into(),
        ];
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let mut error = String::new();
        assert_eq!(
            run_with(
                &args,
                true,
                true,
                &home,
                &root,
                &mut input,
                &mut output,
                &mut error
            ),
            0
        );
        assert!(!home.join("registry.json").exists());
        assert!(!store::db_path(&home, "start-readonly").exists());
    }

    #[test]
    fn registration_and_index_require_confirmation() {
        let root = crate::testutil::tmpdir("start-confirm");
        let home = crate::testutil::tmpdir("start-confirm-home");
        let args = vec![
            "--path".into(),
            root.display().to_string(),
            "--name".into(),
            "start-confirm".into(),
        ];
        let mut input = Cursor::new(b"y\nn\n".to_vec());
        let mut output = Vec::new();
        let mut error = String::new();
        assert_eq!(
            run_with(
                &args,
                true,
                true,
                &home,
                &root,
                &mut input,
                &mut output,
                &mut error
            ),
            0
        );
        assert!(home.join("registry.json").is_file());
        assert!(!store::db_path(&home, "start-confirm").exists());
    }

    #[test]
    fn cancellation_stops_before_registration() {
        let root = crate::testutil::tmpdir("start-cancel");
        let home = crate::testutil::tmpdir("start-cancel-home");
        let args = vec![
            "--path".into(),
            root.display().to_string(),
            "--name".into(),
            "start-cancel".into(),
        ];
        let mut input = Cursor::new(b"n\ny\n".to_vec());
        let mut output = Vec::new();
        let mut error = String::new();
        assert_eq!(
            run_with(
                &args,
                true,
                true,
                &home,
                &root,
                &mut input,
                &mut output,
                &mut error
            ),
            0
        );
        assert!(!home.join("registry.json").exists());
        assert!(!store::db_path(&home, "start-cancel").exists());
        assert!(String::from_utf8(output).unwrap().contains("cancelled"));
    }

    #[test]
    fn approved_index_build_runs_only_after_second_confirmation() {
        let root = paths::canonical_root(&crate::testutil::tmpdir("start-index-confirm")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn ready() {}\n").unwrap();
        let home = crate::testutil::tmpdir("start-index-confirm-home");
        let args = vec![
            "--path".into(),
            root.display().to_string(),
            "--name".into(),
            "start-index-confirm".into(),
        ];
        let mut input = Cursor::new(b"y\ny\n".to_vec());
        let mut output = Vec::new();
        let mut error = String::new();
        assert_eq!(
            run_with(
                &args,
                true,
                true,
                &home,
                &root,
                &mut input,
                &mut output,
                &mut error
            ),
            0,
            "{error}"
        );
        let overview = overview::collect(&home, "start-index-confirm", &root).unwrap();
        assert_eq!(
            overview.snapshot.status,
            overview::SnapshotState::Fresh,
            "{:?}",
            overview.snapshot.reasons
        );
        assert!(overview.snapshot.nonce.is_some());
    }

    #[test]
    fn project_name_conflict_is_invalid_without_overwrite() {
        let first = crate::testutil::tmpdir("start-name-first");
        let second = crate::testutil::tmpdir("start-name-second");
        let home = crate::testutil::tmpdir("start-name-home");
        let first = paths::canonical_root(&first).unwrap();
        let second = paths::canonical_root(&second).unwrap();
        let mut reg = registry::load(&home.join("registry.json")).unwrap();
        registry::add(&mut reg, "same", &first.to_string_lossy(), 1).unwrap();
        registry::save(&home.join("registry.json"), &reg).unwrap();
        let before = std::fs::read(home.join("registry.json")).unwrap();
        let args = vec![
            "--path".into(),
            second.display().to_string(),
            "--name".into(),
            "same".into(),
        ];
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let mut error = String::new();
        assert_eq!(
            run_with(
                &args,
                true,
                true,
                &home,
                &second,
                &mut input,
                &mut output,
                &mut error
            ),
            2
        );
        assert_eq!(std::fs::read(home.join("registry.json")).unwrap(), before);
    }

    #[test]
    fn codex_targeted_inspection_requires_confirmation() {
        let home = crate::testutil::tmpdir("start-inspection-home");
        let binary = std::env::current_exe().unwrap();
        let mut called = false;
        let mut output = Vec::new();
        let mut deny = Cursor::new(b"n\n".to_vec());
        let result = confirm_inspection(&mut deny, &mut output, "inspect", || {
            called = true;
            Some(agent_setup::manual_preview(
                agent_setup::Client::Codex,
                "codex",
                &binary,
                &home,
            ))
        })
        .unwrap();
        assert!(result.is_none());
        assert!(!called);

        let mut allow = Cursor::new(b"y\n".to_vec());
        let result = confirm_inspection(&mut allow, &mut output, "inspect", || {
            called = true;
            Some(agent_setup::manual_preview(
                agent_setup::Client::Codex,
                "codex",
                &binary,
                &home,
            ))
        })
        .unwrap();
        assert!(called);
        assert_eq!(result.unwrap().client, Some(agent_setup::Client::Codex));
    }

    #[test]
    fn in_process_probe_requires_its_own_consent_and_reports_failures_honestly() {
        let mut output = Vec::new();
        let mut denied = Cursor::new(b"n\n".to_vec());
        let mut invoked = false;
        assert!(confirm_probe(&mut denied, &mut output, "probe", || {
            invoked = true;
            Ok(())
        })
        .unwrap()
        .is_none());
        assert!(!invoked);

        let mut allowed = Cursor::new(b"y\n".to_vec());
        let mut report = agent_setup::manual_preview(
            agent_setup::Client::Codex,
            "codex",
            Path::new("riltra"),
            Path::new("home"),
        );
        let result = confirm_probe(&mut allowed, &mut output, "probe", || Ok(())).unwrap();
        report.probe = if result.as_ref().is_some_and(Result::is_ok) {
            agent_setup::ProbeStatus::Succeeded
        } else {
            agent_setup::ProbeStatus::Failed
        };
        report.probe_scope = Some(agent_setup::ProbeScope::InProcessHandler);
        report.in_process_self_test = if result.as_ref().is_some_and(Result::is_ok) {
            agent_setup::InProcessSelfTestStatus::Succeeded
        } else {
            agent_setup::InProcessSelfTestStatus::Failed
        };
        assert_eq!(
            report.in_process_self_test,
            agent_setup::InProcessSelfTestStatus::Succeeded
        );
        assert_eq!(report.probe, agent_setup::ProbeStatus::Succeeded);
        assert_eq!(
            report.connection,
            agent_setup::ConnectionStatus::NotObserved
        );

        let mut allowed = Cursor::new(b"y\n".to_vec());
        let result = confirm_probe(&mut allowed, &mut output, "probe", || {
            Err("unexpected invalid-protocol response".into())
        })
        .unwrap();
        report.probe = if result.as_ref().is_some_and(Result::is_ok) {
            agent_setup::ProbeStatus::Succeeded
        } else {
            agent_setup::ProbeStatus::Failed
        };
        report.probe_scope = Some(agent_setup::ProbeScope::InProcessHandler);
        report.in_process_self_test = if result.as_ref().is_some_and(Result::is_ok) {
            agent_setup::InProcessSelfTestStatus::Succeeded
        } else {
            agent_setup::InProcessSelfTestStatus::Failed
        };
        assert_eq!(
            report.in_process_self_test,
            agent_setup::InProcessSelfTestStatus::Failed
        );
        assert_eq!(report.probe, agent_setup::ProbeStatus::Failed);
        assert_eq!(
            report.connection,
            agent_setup::ConnectionStatus::NotObserved
        );
        let json = serde_json::to_value(report).unwrap();
        assert_eq!(json["connection"], "not_observed");
        assert_eq!(json["probe"], "failed");
        assert_eq!(json["probe_scope"], "in_process_handler");
        assert_eq!(json["in_process_self_test"], "failed");
    }

    #[test]
    fn approved_probe_failure_fails_driver_and_denial_does_not_run_probe() {
        let (home, root, name) = indexed_project("probe-driver");
        let args = vec![
            "--path".into(),
            root.display().to_string(),
            "--name".into(),
            name,
            "--client".into(),
            "opencode".into(),
        ];
        let mut output = Vec::new();
        let mut error = String::new();
        let mut denied = Cursor::new(b"n\nq\n".to_vec());
        let mut invoked = false;
        let code = run_with_probe_hook(
            &args,
            &home,
            &root,
            &mut denied,
            &mut output,
            &mut error,
            || {
                invoked = true;
                Ok(())
            },
        );
        assert_eq!(code, 0, "{error}");
        assert!(!invoked);
        assert!(!String::from_utf8(output)
            .unwrap()
            .contains("\"probe_scope\""));

        let mut output = Vec::new();
        let mut error = String::new();
        let mut approved = Cursor::new(b"y\nq\n".to_vec());
        let code = run_with_probe_hook(
            &args,
            &home,
            &root,
            &mut approved,
            &mut output,
            &mut error,
            || Ok(()),
        );
        assert_eq!(code, 0, "{error}");
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\"probe\":\"succeeded\""));
        assert!(output.contains("\"probe_scope\":\"in_process_handler\""));
        assert!(output.contains("Direct probe scope: in_process_handler"));
        assert!(output
            .contains("does not verify the installed executable, OS stdio, or client connection"));
        assert!(output.contains("\"probe_scope\":\"in_process_handler\""));
        assert!(output.contains("\"connection\":\"not_observed\""));
        assert!(output.contains("\"configuration\":\"manual_required\""));
        assert!(!output.contains("\"connection\":\"reported_connected\""));

        let mut output = Vec::new();
        let mut error = String::new();
        let mut approved = Cursor::new(b"y\n".to_vec());
        let code = run_with_probe_hook(
            &args,
            &home,
            &root,
            &mut approved,
            &mut output,
            &mut error,
            || Err("timed out".into()),
        );
        assert_eq!(code, 3);
        assert!(error.contains("approved in-process MCP handler probe failed: timed out"));
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Direct probe scope: in_process_handler"));
        assert!(output.contains("probe failed: timed out"));
        assert!(output.contains("\"in_process_self_test\":\"failed\""));
        assert!(output.contains("\"probe\":\"failed\""));
        assert!(output.contains("\"probe_scope\":\"in_process_handler\""));
        assert!(output.contains("\"connection\":\"not_observed\""));
        assert!(output.contains("\"configuration\":\"manual_required\""));
        assert!(!output.contains("Choose a definition number"));
    }

    #[test]
    fn nonterminal_mode_is_json_and_read_only() {
        let root = crate::testutil::tmpdir("start-nonterminal");
        let home = crate::testutil::tmpdir("start-nonterminal-home");
        let args = vec![
            "--path".into(),
            root.display().to_string(),
            "--name".into(),
            "start-nonterminal".into(),
        ];
        let mut input = Cursor::new(b"y\ny\n".to_vec());
        let mut output = Vec::new();
        let mut error = String::new();
        assert_eq!(
            run_with(
                &args,
                true,
                false,
                &home,
                &root,
                &mut input,
                &mut output,
                &mut error
            ),
            0
        );
        let output = String::from_utf8(output).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(output.trim()).is_ok());
        assert!(!output.contains("[y/N]"));
        assert!(!home.join("registry.json").exists());
        assert!(!store::db_path(&home, "start-nonterminal").exists());
    }

    fn indexed_project(label: &str) -> (PathBuf, PathBuf, String) {
        let root = crate::testutil::tmpdir(&format!("start-{label}"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn selected() {}\n").unwrap();
        let root = paths::canonical_root(&root).unwrap();
        let home = crate::testutil::tmpdir(&format!("start-{label}-home"));
        let name = format!("start-{label}");
        let mut reg = registry::load(&home.join("registry.json")).unwrap();
        registry::add(&mut reg, &name, &root.to_string_lossy(), 1).unwrap();
        registry::save(&home.join("registry.json"), &reg).unwrap();
        index::build(&home, &name, &root, true).unwrap();
        (home, root, name)
    }

    #[test]
    fn folder_to_definition_packet_uses_internal_anchor() {
        let (home, root, name) = indexed_project("packet");
        let args = vec![
            "--path".into(),
            root.display().to_string(),
            "--name".into(),
            name,
        ];
        let mut input = Cursor::new(b"1\n".to_vec());
        let mut output = Vec::new();
        let mut error = String::new();
        assert_eq!(
            run_with(
                &args,
                true,
                true,
                &home,
                &root,
                &mut input,
                &mut output,
                &mut error
            ),
            0,
            "{error}"
        );
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("selected"));
        assert!(output.contains("\"snippets\""));
    }

    #[test]
    fn snapshot_change_discards_menu_choice_and_requires_new_choice() {
        let (home, root, name) = indexed_project("snapshot");
        let args = vec![
            "--path".into(),
            root.display().to_string(),
            "--name".into(),
            name.clone(),
        ];
        let mut input = Cursor::new(b"1\n1\n".to_vec());
        let mut output = Vec::new();
        let mut error = String::new();
        let mut changed = false;
        let code = run_with_hook(
            &args,
            true,
            true,
            &home,
            &root,
            &mut input,
            &mut output,
            &mut error,
            || {
                if !changed {
                    changed = true;
                    std::fs::write(root.join("src/lib.rs"), "pub fn selected() { }\n").unwrap();
                    index::build(&home, &name, &root, true).unwrap();
                }
            },
        );
        assert_eq!(code, 0, "{error}");
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("snapshot changed"));
        assert!(output.contains("\"snippets\""));
    }

    #[test]
    fn packet_snapshot_race_discards_result_and_requires_reselection() {
        let (home, root, name) = indexed_project("packet-race");
        std::fs::write(root.join("src/lib.rs"), "pub fn beta() {}\n").unwrap();
        index::build(&home, &name, &root, true).unwrap();
        let args = vec![
            "--path".into(),
            root.display().to_string(),
            "--name".into(),
            name.clone(),
        ];
        let mut input = Cursor::new(b"1\nq\n".to_vec());
        let mut output = Vec::new();
        let mut error = String::new();
        let mut changed = false;
        let code = run_with_packet_hook(
            &args,
            true,
            true,
            &home,
            &root,
            &mut input,
            &mut output,
            &mut error,
            || {},
            || {
                if !changed {
                    changed = true;
                    std::fs::write(root.join("src/lib.rs"), "pub fn alpha() {}\n").unwrap();
                    index::build(&home, &name, &root, true).unwrap();
                }
            },
        );
        assert_eq!(code, 0, "{error}");
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches("Choose a definition number").count(), 2);
        assert!(output.contains("snapshot changed before packet"));
        assert!(!output.contains("\"snippets\""));
    }

    #[test]
    fn source_change_uses_indexed_position_and_packet_reports_stale_source() {
        let root = crate::testutil::tmpdir("start-source-identity");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn alpha() {}\npub fn beta() {}\n",
        )
        .unwrap();
        let root = paths::canonical_root(&root).unwrap();
        let home = crate::testutil::tmpdir("start-source-identity-home");
        let name = "start-source-identity";
        let mut reg = registry::load(&home.join("registry.json")).unwrap();
        registry::add(&mut reg, name, &root.to_string_lossy(), 1).unwrap();
        registry::save(&home.join("registry.json"), &reg).unwrap();
        index::build(&home, name, &root, true).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "\npub fn alpha() {}\npub fn beta() {}\n",
        )
        .unwrap();
        let overview = overview::collect(&home, name, &root).unwrap();
        let beta = overview
            .definitions
            .iter()
            .find(|item| item.symbol_name.as_deref() == Some("beta"))
            .unwrap();
        let overview_json = serde_json::to_string(&overview).unwrap();
        assert!(!overview_json.contains("start_col"));
        assert_eq!(overview.snapshot.status, overview::SnapshotState::Stale);
        let anchor = selected_anchor(&root, beta).unwrap();
        assert_eq!(anchor.line, beta.line_start);
        assert_eq!(anchor.col, beta.start_col);

        let entry = registry::find(&reg, name).unwrap().clone();
        let mut conn = match store::open_read(&store::db_path(&home, name)) {
            store::IndexRead::Ready(conn) => conn,
            _ => panic!("indexed repository unavailable"),
        };
        let result = packet::query_conn(
            &mut conn,
            &home,
            &entry,
            name,
            &root,
            &packet::PacketRequest {
                anchors: vec![anchor],
                depth: crate::context::DEFAULT_DEPTH,
                limit: crate::context::DEFAULT_LIMIT,
                max_bytes: crate::context::DEFAULT_MAX_BYTES,
            },
        )
        .unwrap();
        assert_eq!(result.anchors[0].source_reason, Some("source-changed"));
        assert_eq!(result.snippets[0].status, "unavailable");
    }
}
