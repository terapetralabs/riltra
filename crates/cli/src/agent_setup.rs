// SPDX-License-Identifier: Apache-2.0
//! Read-only client configuration inspection and setup preview.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Client {
    Codex,
    Claude,
    OpenCode,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupScope {
    UserGlobal,
    ProjectLocal,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupStatus {
    NotRequested,
    AlreadyConfigured,
    ManualRequired,
    Planned,
    Applied,
    Cancelled,
    Failed,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationStatus {
    NotInspected,
    Absent,
    Found,
    Conflict,
    ManualRequired,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    NotRequested,
    Succeeded,
    Failed,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeScope {
    InProcessHandler,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InProcessSelfTestStatus {
    NotRequested,
    Succeeded,
    Failed,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    NotObserved,
    ReportedConnected,
    ReportedDisconnected,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EnvironmentAddition {
    pub name: String,
    pub value: String,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SetupReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<Client>,
    pub status: SetupStatus,
    pub configuration: ConfigurationStatus,
    pub probe: ProbeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_scope: Option<ProbeScope>,
    pub in_process_self_test: InProcessSelfTestStatus,
    pub connection: ConnectionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<SetupScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    pub argv: Vec<String>,
    pub environment_additions: Vec<EnvironmentAddition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn base(
    client: Client,
    status: SetupStatus,
    configuration: ConfigurationStatus,
    reason: &str,
) -> SetupReport {
    SetupReport {
        client: Some(client),
        status,
        configuration,
        probe: ProbeStatus::NotRequested,
        probe_scope: None,
        in_process_self_test: InProcessSelfTestStatus::NotRequested,
        connection: ConnectionStatus::NotObserved,
        scope: Some(SetupScope::UserGlobal),
        executable: None,
        argv: Vec::new(),
        environment_additions: Vec::new(),
        reason: Some(reason.into()),
    }
}

fn opencode_argv(binary: &Path, home: &Path) -> Vec<String> {
    vec![
        "mcp".into(),
        "add".into(),
        "riltra".into(),
        "--global".into(),
        "--env".into(),
        format!("RILTRA_HOME={}", home.display()),
        "--".into(),
        binary.display().to_string(),
        "mcp".into(),
    ]
}

/// Parses arbitrary JSON while rejecting duplicate object keys at every depth.
struct UniqueValue(serde_json::Value);
impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = UniqueValue;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("unambiguous JSON")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| UniqueValue(n.into()))
                    .ok_or_else(|| E::custom("non-finite number"))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueValue(serde_json::Value::Null))
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                self.visit_unit()
            }
            fn visit_some<D: serde::Deserializer<'de>>(
                self,
                d: D,
            ) -> Result<Self::Value, D::Error> {
                d.deserialize_any(V)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(v) = seq.next_element_seed(UniqueSeed)? {
                    values.push(v.0);
                }
                Ok(UniqueValue(values.into()))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                let mut keys = HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !keys.insert(key.clone()) {
                        return Err(serde::de::Error::custom("duplicate object key"));
                    }
                    values.insert(key, map.next_value_seed(UniqueSeed)?.0);
                }
                Ok(UniqueValue(values.into()))
            }
        }
        deserializer.deserialize_any(V)
    }
}
struct UniqueSeed;
impl<'de> DeserializeSeed<'de> for UniqueSeed {
    type Value = UniqueValue;
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        UniqueValue::deserialize(d)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ConfigSource {
    pub path: PathBuf,
    pub scope: SetupScope,
}

#[derive(Debug, Clone)]
pub(crate) struct ClaudeConfigSource {
    pub config: ConfigSource,
    /// Present for the project-specific section of `~/.claude.json`.
    pub project_root: Option<String>,
}

const MAX_SETUP_OUTPUT_BYTES: usize = 4096;
type BoundedOutput = std::io::Result<(Vec<u8>, bool)>;
type BoundedOutputReceiver = std::sync::mpsc::Receiver<BoundedOutput>;
type BoundedOutputReader = std::thread::JoinHandle<()>;
pub(crate) const SETUP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaudeSourceIdentity {
    path: PathBuf,
    scope: SetupScope,
    project_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceSnapshot {
    identity: ClaudeSourceIdentity,
    bytes: Option<Vec<u8>>,
}

/// An immutable approval preview plus the exact known Claude config bytes.
pub(crate) struct SetupPlan {
    report: SetupReport,
    requested_executable: String,
    executable_path: PathBuf,
    binary: PathBuf,
    home: PathBuf,
    sources: Vec<ClaudeSourceIdentity>,
    snapshots: Vec<SourceSnapshot>,
    #[cfg(test)]
    test_process: Option<TestProcess>,
}

#[cfg(test)]
struct TestProcess {
    executable: PathBuf,
    argv: Vec<String>,
    environment: Vec<EnvironmentAddition>,
}

impl SetupPlan {
    pub(crate) fn new(
        report: SetupReport,
        sources: &[ClaudeConfigSource],
        binary: &Path,
        home: &Path,
    ) -> Self {
        #[cfg(test)]
        {
            Self::new_inner(report, sources, binary, home, None)
        }
        #[cfg(not(test))]
        {
            Self::new_inner(report, sources, binary, home)
        }
    }

    #[cfg(test)]
    fn new_for_test(
        report: SetupReport,
        sources: &[ClaudeConfigSource],
        binary: &Path,
        home: &Path,
        executable: &Path,
    ) -> Self {
        Self::new_inner(report, sources, binary, home, Some(executable))
    }

    fn new_inner(
        mut report: SetupReport,
        sources: &[ClaudeConfigSource],
        binary: &Path,
        home: &Path,
        #[cfg(test)] test_executable: Option<&Path>,
    ) -> Self {
        if !matches!(
            report.status,
            SetupStatus::Planned | SetupStatus::AlreadyConfigured | SetupStatus::ManualRequired
        ) {
            report.status = SetupStatus::ManualRequired;
            report.configuration = ConfigurationStatus::ManualRequired;
            report.reason = Some("This setup result cannot be applied automatically.".into());
        }
        let identities: Vec<_> = sources.iter().map(ClaudeSourceIdentity::from).collect();
        let requested_executable = report.executable.clone().unwrap_or_default();
        let mut plan = Self {
            report,
            requested_executable,
            executable_path: PathBuf::new(),
            binary: binary.to_path_buf(),
            home: home.to_path_buf(),
            sources: identities,
            snapshots: Vec::new(),
            #[cfg(test)]
            test_process: None,
        };
        if plan.report.status != SetupStatus::Planned {
            return plan;
        }
        if plan.report.client != Some(Client::Claude)
            || plan.report.configuration != ConfigurationStatus::Absent
            || plan.report.scope != Some(SetupScope::UserGlobal)
            || plan.sources.is_empty()
            || !binary.is_absolute()
            || !home.is_absolute()
            || plan.sources.iter().any(|source| !source.path.is_absolute())
            || plan.report.argv.is_empty()
            || plan.report.environment_additions.is_empty()
        {
            plan.report = stale_plan(
                &plan.report,
                "Setup capability or effective scope is unknown; use the manual instructions.",
            );
            return plan;
        }
        let expected = preview(Client::Claude, "claude", binary, home);
        if plan.report.executable != expected.executable
            || plan.report.argv != expected.argv
            || plan.report.environment_additions != expected.environment_additions
        {
            plan.report = stale_plan(
                &plan.report,
                "Setup arguments or environment differ from the supported preview; use manual instructions.",
            );
            return plan;
        }
        let before = match source_snapshots(&plan.sources) {
            Ok(snapshots) => snapshots,
            Err(_) => {
                plan.report = stale_plan(
                    &plan.report,
                    "A selected client configuration cannot be read; use the manual instructions.",
                );
                return plan;
            }
        };
        let fresh = inspect_claude(sources, true, binary, home);
        let after = match source_snapshots(&plan.sources) {
            Ok(snapshots) => snapshots,
            Err(_) => {
                plan.report = stale_plan(
                    &plan.report,
                    "A selected client configuration cannot be read; use the manual instructions.",
                );
                return plan;
            }
        };
        if before != after
            || fresh.status != plan.report.status
            || fresh.configuration != plan.report.configuration
            || fresh.scope != plan.report.scope
        {
            plan.report = stale_plan(
                &plan.report,
                "Selected configuration changed while preparing the preview; inspect it again.",
            );
            return plan;
        }
        plan.snapshots = before;
        #[cfg(test)]
        let resolved = match test_executable {
            Some(path) => path.canonicalize().ok().filter(|p| p.is_file()),
            None => resolve_executable(&plan.requested_executable),
        };
        #[cfg(not(test))]
        let resolved = resolve_executable(&plan.requested_executable);
        let Some(resolved) = resolved else {
            plan.report = stale_plan(
                &plan.report,
                "Selected client executable is unavailable; use the manual instructions.",
            );
            return plan;
        };
        plan.report.executable = Some(resolved.display().to_string());
        plan.executable_path = resolved;
        #[cfg(test)]
        {
            plan.test_process = None;
        }
        plan
    }

    pub(crate) fn report(&self) -> &SetupReport {
        &self.report
    }
}

impl From<&ClaudeConfigSource> for ClaudeSourceIdentity {
    fn from(source: &ClaudeConfigSource) -> Self {
        Self {
            path: source.config.path.clone(),
            scope: source.config.scope,
            project_root: source.project_root.clone(),
        }
    }
}

fn source_snapshots(sources: &[ClaudeSourceIdentity]) -> Result<Vec<SourceSnapshot>, String> {
    sources
        .iter()
        .map(|identity| {
            let bytes = match std::fs::read(&identity.path) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => return Err("a selected client configuration cannot be read".into()),
            };
            Ok(SourceSnapshot {
                identity: identity.clone(),
                bytes,
            })
        })
        .collect()
}

fn resolve_executable(executable: &str) -> Option<PathBuf> {
    let path = Path::new(executable);
    if path.components().count() > 1 || path.is_absolute() {
        return path.canonicalize().ok().filter(|p| p.is_file());
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|directory| {
        let candidate = directory.join(executable);
        candidate
            .canonicalize()
            .ok()
            .filter(|p| p.is_file())
            .or_else(|| {
                let suffix = std::env::consts::EXE_SUFFIX;
                (!suffix.is_empty() && path.extension().is_none())
                    .then(|| directory.join(format!("{executable}{suffix}")))
                    .and_then(|candidate| candidate.canonicalize().ok())
                    .filter(|p| p.is_file())
            })
    })
}

pub(crate) struct CapturedOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) stdout_overflow: bool,
    pub(crate) stderr_overflow: bool,
}

fn drain_bounded<R: std::io::Read>(mut pipe: R) -> std::io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::with_capacity(MAX_SETUP_OUTPUT_BYTES);
    let mut overflow = false;
    let mut chunk = [0u8; 8192];
    loop {
        let read = pipe.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        let keep = (MAX_SETUP_OUTPUT_BYTES - output.len()).min(read);
        output.extend_from_slice(&chunk[..keep]);
        overflow |= keep < read;
    }
    Ok((output, overflow))
}

fn kill_and_reap(child: &mut std::process::Child) -> bool {
    if child.kill().is_ok() {
        child.wait().is_ok()
    } else {
        child.try_wait().ok().flatten().is_some()
    }
}

fn spawn_bounded_reader<R: std::io::Read + Send + 'static>(
    mut pipe: R,
) -> (BoundedOutputReceiver, BoundedOutputReader) {
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let _ = sender.send(drain_bounded(&mut pipe));
    });
    (receiver, reader)
}

fn receive_bounded_output(
    receiver: BoundedOutputReceiver,
    reader: BoundedOutputReader,
    deadline: Instant,
) -> Result<(Vec<u8>, bool), &'static str> {
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(bytes)) => {
            reader.join().map_err(|_| "client output reader failed")?;
            Ok(bytes)
        }
        Ok(Err(_)) => Err("client output could not be read"),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            // A client may leave inherited pipes open in a child process. The
            // reader remains memory-bounded and detached after our deadline.
            drop(reader);
            Err("client output pipes did not close before the deadline")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err("client output reader failed"),
    }
}

pub(crate) fn run_setup_process(
    executable: &Path,
    argv: &[String],
    environment: &[EnvironmentAddition],
    timeout: Duration,
) -> Result<(ExitStatus, CapturedOutput), &'static str> {
    run_setup_process_in_dir(executable, None, argv, environment, timeout)
}

fn run_setup_process_in_dir(
    executable: &Path,
    current_dir: Option<&Path>,
    argv: &[String],
    environment: &[EnvironmentAddition],
    timeout: Duration,
) -> Result<(ExitStatus, CapturedOutput), &'static str> {
    let mut command = Command::new(executable);
    command
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    for addition in environment {
        command.env(&addition.name, &addition.value);
    }
    let deadline = Instant::now() + timeout;
    let mut child = command
        .spawn()
        .map_err(|_| "client process could not be started")?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (stdout_output, stdout_reader) = spawn_bounded_reader(stdout);
    let (stderr_output, stderr_reader) = spawn_bounded_reader(stderr);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let reaped = kill_and_reap(&mut child);
                drop(stdout_reader);
                drop(stderr_reader);
                return Err(if reaped {
                    "client process timed out and was stopped"
                } else {
                    "client process timed out; child cleanup could not be confirmed"
                });
            }
            Err(_) => {
                let reaped = kill_and_reap(&mut child);
                drop(stdout_reader);
                drop(stderr_reader);
                return Err(if reaped {
                    "client process could not be monitored and was stopped"
                } else {
                    "client process could not be monitored; child cleanup could not be confirmed"
                });
            }
        }
    };
    let (stdout, stdout_overflow) = receive_bounded_output(stdout_output, stdout_reader, deadline)?;
    let (stderr, stderr_overflow) = receive_bounded_output(stderr_output, stderr_reader, deadline)?;
    Ok((
        status,
        CapturedOutput {
            stdout,
            stderr,
            stdout_overflow,
            stderr_overflow,
        },
    ))
}

fn stale_plan(report: &SetupReport, reason: &'static str) -> SetupReport {
    let mut report = report.clone();
    report.status = SetupStatus::ManualRequired;
    report.configuration = ConfigurationStatus::ManualRequired;
    report.reason = Some(reason.into());
    report
}

fn post_apply_configuration(
    plan: &SetupPlan,
    resolve_sources: &mut impl FnMut() -> (bool, Vec<ClaudeConfigSource>),
) -> (ConfigurationStatus, SetupScope) {
    let (all_sources_known, sources) = resolve_sources();
    let identities: Vec<_> = sources.iter().map(ClaudeSourceIdentity::from).collect();
    if !all_sources_known || identities != plan.sources {
        return (ConfigurationStatus::ManualRequired, SetupScope::Unknown);
    }
    let Ok(before) = source_snapshots(&identities) else {
        return (ConfigurationStatus::ManualRequired, SetupScope::Unknown);
    };
    let report = inspect_claude(&sources, all_sources_known, &plan.binary, &plan.home);
    let Ok(after) = source_snapshots(&identities) else {
        return (ConfigurationStatus::ManualRequired, SetupScope::Unknown);
    };
    if before != after {
        return (ConfigurationStatus::ManualRequired, SetupScope::Unknown);
    }
    if report.status == SetupStatus::AlreadyConfigured
        && report.configuration == ConfigurationStatus::Found
        && report.scope == plan.report.scope
    {
        (
            ConfigurationStatus::Found,
            report.scope.unwrap_or(SetupScope::Unknown),
        )
    } else if report.configuration == ConfigurationStatus::Found {
        (
            ConfigurationStatus::ManualRequired,
            report.scope.unwrap_or(SetupScope::Unknown),
        )
    } else {
        (
            report.configuration,
            report.scope.unwrap_or(SetupScope::Unknown),
        )
    }
}

/// Applies only a confirmed, current Claude preview. The caller re-resolves the
/// effective source list immediately before the executor rereads and reinspects it.
pub(crate) fn apply_setup(
    plan: SetupPlan,
    approved: bool,
    resolve_sources: impl FnMut() -> (bool, Vec<ClaudeConfigSource>),
) -> SetupReport {
    apply_setup_inner(plan, approved, resolve_sources, || {})
}

#[cfg(test)]
fn apply_setup_with_after_snapshot_hook(
    plan: SetupPlan,
    approved: bool,
    resolve_sources: impl FnMut() -> (bool, Vec<ClaudeConfigSource>),
    after_snapshot: impl FnOnce(),
) -> SetupReport {
    apply_setup_inner(plan, approved, resolve_sources, after_snapshot)
}

fn apply_setup_inner(
    plan: SetupPlan,
    approved: bool,
    mut resolve_sources: impl FnMut() -> (bool, Vec<ClaudeConfigSource>),
    after_snapshot: impl FnOnce(),
) -> SetupReport {
    if plan.report.status != SetupStatus::Planned {
        return plan.report;
    }
    if !approved {
        let mut report = plan.report;
        report.status = SetupStatus::Cancelled;
        report.reason = Some("Setup was not approved; no client process was started.".into());
        return report;
    }
    if plan.report.client != Some(Client::Claude)
        || plan.report.configuration != ConfigurationStatus::Absent
        || plan.report.scope != Some(SetupScope::UserGlobal)
    {
        return stale_plan(
            &plan.report,
            "This setup preview is not eligible for automatic apply.",
        );
    }
    let (all_sources_known, fresh_sources) = resolve_sources();
    if !all_sources_known {
        return stale_plan(
            &plan.report,
            "Effective client configuration layers are no longer known; inspect and approve again.",
        );
    }
    let identities: Vec<_> = fresh_sources
        .iter()
        .map(ClaudeSourceIdentity::from)
        .collect();
    if identities != plan.sources {
        return stale_plan(
            &plan.report,
            "The selected configuration or setup preview changed; inspect and approve it again.",
        );
    }
    let current = match source_snapshots(&identities) {
        Ok(current) if current == plan.snapshots => current,
        _ => return stale_plan(&plan.report, "The selected configuration changed or became unreadable; inspect and approve it again."),
    };
    after_snapshot();
    let fresh = inspect_claude(&fresh_sources, all_sources_known, &plan.binary, &plan.home);
    if fresh.status != SetupStatus::Planned
        || fresh.configuration != ConfigurationStatus::Absent
        || fresh.scope != Some(SetupScope::UserGlobal)
        || fresh.executable.as_deref() != Some(plan.requested_executable.as_str())
        || fresh.argv
            != preview(
                Client::Claude,
                &plan.requested_executable,
                &plan.binary,
                &plan.home,
            )
            .argv
        || fresh.environment_additions != plan.report.environment_additions
    {
        return stale_plan(&plan.report, "The selected configuration is no longer absent and applicable; inspect and approve it again.");
    }
    match source_snapshots(&identities) {
        Ok(after_inspection) if after_inspection == current => {}
        _ => return stale_plan(
            &plan.report,
            "The selected configuration changed during inspection; inspect and approve it again.",
        ),
    }
    #[cfg(test)]
    let current_executable = plan
        .test_process
        .as_ref()
        .map(|_| plan.executable_path.clone());
    #[cfg(test)]
    let current_executable =
        current_executable.or_else(|| resolve_executable(&plan.requested_executable));
    #[cfg(not(test))]
    let current_executable = resolve_executable(&plan.requested_executable);
    let Some(current_executable) = current_executable else {
        return stale_plan(&plan.report, "The selected client executable changed or became unavailable; inspect and approve again.");
    };
    if current_executable != plan.executable_path {
        return stale_plan(
            &plan.report,
            "The selected client executable changed; inspect and approve it again.",
        );
    }
    #[cfg(test)]
    let result = if let Some(test_process) = &plan.test_process {
        let mut environment = plan.report.environment_additions.clone();
        environment.extend(test_process.environment.clone());
        run_setup_process(
            &test_process.executable,
            &test_process.argv,
            &environment,
            SETUP_TIMEOUT,
        )
    } else {
        run_setup_process(
            &plan.executable_path,
            &plan.report.argv,
            &plan.report.environment_additions,
            SETUP_TIMEOUT,
        )
    };
    #[cfg(not(test))]
    let result = run_setup_process(
        &plan.executable_path,
        &plan.report.argv,
        &plan.report.environment_additions,
        SETUP_TIMEOUT,
    );
    match result {
        Ok((status, output)) if status.success() => {
            let _captured_bytes = output.stdout.len() + output.stderr.len();
            let (configuration, observed_scope) =
                post_apply_configuration(&plan, &mut resolve_sources);
            let mut report = plan.report;
            report.status = SetupStatus::Applied;
            report.configuration = configuration;
            report.scope = Some(observed_scope);
            report.reason = match configuration {
                ConfigurationStatus::Found => None,
                ConfigurationStatus::Absent => Some("The client process exited successfully, but no matching entry was observed afterward; verify the selected profile before retrying.".into()),
                ConfigurationStatus::Conflict => Some("The client process exited successfully, but the selected entry differs; review it manually before retrying.".into()),
                ConfigurationStatus::ManualRequired if observed_scope == SetupScope::ProjectLocal => Some("The client process exited successfully, but a matching entry was observed only in project-local scope; user-global configuration was not verified.".into()),
                ConfigurationStatus::ManualRequired | ConfigurationStatus::NotInspected => Some("The client process exited successfully, but configuration could not be verified; review the selected layers before retrying.".into()),
            };
            report
        }
        Ok((status, output)) => {
            let _captured_bytes = output.stdout.len() + output.stderr.len();
            let mut report = plan.report;
            report.status = SetupStatus::Failed;
            report.configuration = ConfigurationStatus::ManualRequired;
            report.reason = Some(format!(
                "The client setup process exited unsuccessfully (code {}).",
                status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown".into())
            ));
            report
        }
        Err(reason) => {
            let mut report = plan.report;
            report.status = SetupStatus::Failed;
            report.configuration = ConfigurationStatus::ManualRequired;
            report.reason = Some(reason.into());
            report
        }
    }
}

fn preview(client: Client, executable: &str, binary: &Path, home: &Path) -> SetupReport {
    let env = EnvironmentAddition {
        name: "RILTRA_HOME".into(),
        value: home.display().to_string(),
    };
    let argv = match client {
        Client::OpenCode => opencode_argv(binary, home),
        Client::Codex => vec![
            "mcp".into(),
            "add".into(),
            "riltra".into(),
            "--env".into(),
            format!("RILTRA_HOME={}", home.display()),
            "--".into(),
            binary.display().to_string(),
            "mcp".into(),
        ],
        Client::Claude => vec![
            "mcp".into(),
            "add".into(),
            "--env".into(),
            format!("RILTRA_HOME={}", home.display()),
            "--transport".into(),
            "stdio".into(),
            "--scope".into(),
            "user".into(),
            "riltra".into(),
            "--".into(),
            binary.display().to_string(),
            "mcp".into(),
        ],
    };
    let reason = if binary.is_file() {
        "Selected configuration or effective precedence is not established; review the targeted preview manually."
    } else {
        "Riltra executable is missing; the setup command preview cannot be applied as shown."
    };
    let mut report = base(
        client,
        SetupStatus::ManualRequired,
        ConfigurationStatus::ManualRequired,
        reason,
    );
    report.executable = Some(executable.into());
    report.argv = argv;
    report.environment_additions.push(env);
    report
}

/// A bounded setup preview when the selected integration cannot be inspected
/// safely with the evidence available to the caller.
pub(crate) fn manual_preview(
    client: Client,
    executable: &str,
    binary: &Path,
    home: &Path,
) -> SetupReport {
    preview(client, executable, binary, home)
}

fn parse_json(bytes: &[u8]) -> Result<serde_json::Value, ()> {
    serde_json::from_slice::<UniqueValue>(bytes)
        .map(|v| v.0)
        .map_err(|_| ())
}

fn opencode_entry(value: &serde_json::Value) -> Result<Option<&serde_json::Value>, ()> {
    let root = value.as_object().ok_or(())?;
    let Some(mcp) = root.get("mcp") else {
        return Ok(None);
    };
    let Some(mcp) = mcp.as_object() else {
        return Err(());
    };
    let Some(servers) = mcp.get("servers") else {
        return if mcp.keys().all(|key| key == "timeout") {
            Ok(None)
        } else {
            Err(())
        };
    };
    let Some(servers) = servers.as_object() else {
        return Err(());
    };
    Ok(servers.get("riltra"))
}

/// Whether an entry's arguments after the binary start the MCP server: `mcp`,
/// with `--all-tools` (lists every tool) or `--compact` (text `code_grep`
/// answers), which are the same server.
fn serves_mcp(words: &[String]) -> bool {
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    matches!(
        words.as_slice(),
        ["mcp"]
            | ["mcp", "--all-tools" | "--compact"]
            | ["mcp", "--all-tools", "--compact"]
            | ["mcp", "--compact", "--all-tools"]
    )
}

fn opencode_matches(entry: &serde_json::Value, binary: &Path, home: &Path) -> bool {
    let expected_keys = ["type", "command", "environment"];
    let Some(object) = entry.as_object() else {
        return false;
    };
    let has_only_expected_keys = object
        .keys()
        .all(|k| expected_keys.contains(&k.as_str()) || k == "disabled");
    let command: Vec<String> = entry
        .get("command")
        .and_then(serde_json::Value::as_array)
        .and_then(|v| v.iter().map(|x| x.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let expected_home = home.display().to_string();
    let env = entry
        .get("environment")
        .and_then(serde_json::Value::as_object);
    let enabled = match entry.get("disabled") {
        None => true,
        Some(disabled) => disabled.as_bool() == Some(false),
    };
    has_only_expected_keys
        && enabled
        && entry.get("type").and_then(serde_json::Value::as_str) == Some("local")
        && command.split_first().is_some_and(|(first, words)| {
            *first == binary.display().to_string() && serves_mcp(words)
        })
        && env.is_some_and(|e| {
            e.len() == 1
                && e.get("RILTRA_HOME").and_then(serde_json::Value::as_str)
                    == Some(expected_home.as_str())
        })
}

/// `sources` must be ordered low-to-high precedence. `all_sources_known` may
/// be true only after the caller resolved every global, project, and system layer.
pub(crate) fn inspect_opencode(
    sources: &[ConfigSource],
    all_sources_known: bool,
    binary: &Path,
    home: &Path,
) -> SetupReport {
    if !binary.is_file() {
        return preview(Client::OpenCode, "opencode", binary, home);
    }
    let mut selected = None;
    let mut definitions = 0usize;
    for source in sources {
        let bytes = match std::fs::read(&source.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return preview(Client::OpenCode, "opencode", binary, home),
        };
        let value = match parse_json(&bytes) {
            Ok(v) => v,
            Err(()) => return preview(Client::OpenCode, "opencode", binary, home),
        };
        match opencode_entry(&value) {
            Ok(Some(entry)) => {
                definitions += 1;
                if definitions > 1 {
                    let mut report = preview(Client::OpenCode, "opencode", binary, home);
                    report.configuration = ConfigurationStatus::ManualRequired;
                    report.scope = Some(SetupScope::Unknown);
                    report.reason = Some("Multiple OpenCode scopes define riltra; overrides require manual resolution and were preserved.".into());
                    return report;
                }
                selected = Some((entry.clone(), source.scope));
            }
            Ok(None) => {}
            Err(()) => return preview(Client::OpenCode, "opencode", binary, home),
        }
    }
    let Some((entry, scope)) = selected else {
        let mut report = preview(Client::OpenCode, "opencode", binary, home);
        report.scope = Some(if all_sources_known {
            SetupScope::UserGlobal
        } else {
            SetupScope::Unknown
        });
        report.configuration = if all_sources_known {
            ConfigurationStatus::Absent
        } else {
            ConfigurationStatus::ManualRequired
        };
        report.reason = Some(if all_sources_known {
            "No riltra entry exists in the inspected effective layers; native add remains disabled until isolation is proven.".into()
        } else {
            "No riltra entry was found in the inspected scope; higher-precedence layers are unknown.".into()
        });
        return report;
    };
    if !all_sources_known {
        let mut report = preview(Client::OpenCode, "opencode", binary, home);
        report.configuration = ConfigurationStatus::Found;
        report.scope = Some(scope);
        report.reason = Some("A riltra entry exists in the inspected scope, but higher-precedence layers are unknown.".into());
        return report;
    }
    if opencode_matches(&entry, binary, home) {
        let mut report = base(
            Client::OpenCode,
            SetupStatus::AlreadyConfigured,
            ConfigurationStatus::Found,
            "Matching enabled OpenCode riltra entry found; no changes planned.",
        );
        report.scope = Some(scope);
        report.reason = None;
        report
    } else {
        let disabled = entry.get("disabled").and_then(serde_json::Value::as_bool) == Some(true);
        let mut report = base(
            Client::OpenCode,
            SetupStatus::ManualRequired,
            ConfigurationStatus::Conflict,
            if disabled {
                "The selected OpenCode riltra entry is disabled; it was preserved."
            } else {
                "A different OpenCode riltra entry exists and was preserved; resolve it manually."
            },
        );
        report.scope = Some(scope);
        report
    }
}

fn claude_entry<'a>(
    value: &'a serde_json::Value,
    source: &ClaudeConfigSource,
) -> Result<Option<&'a serde_json::Value>, ()> {
    let root = value.as_object().ok_or(())?;
    let root = if let Some(project) = &source.project_root {
        let Some(projects) = root.get("projects") else {
            return Ok(None);
        };
        let Some(projects) = projects.as_object() else {
            return Err(());
        };
        let Some(project) = projects.get(project) else {
            return Ok(None);
        };
        project.as_object().ok_or(())?
    } else {
        root
    };
    let Some(servers) = root.get("mcpServers") else {
        return Ok(None);
    };
    let Some(servers) = servers.as_object() else {
        return Err(());
    };
    Ok(servers.get("riltra"))
}

fn claude_matches(entry: &serde_json::Value, binary: &Path, home: &Path) -> bool {
    let Some(object) = entry.as_object() else {
        return false;
    };
    let allowed = ["type", "command", "args", "env"];
    let args: Vec<String> = entry
        .get("args")
        .and_then(serde_json::Value::as_array)
        .and_then(|v| v.iter().map(|x| x.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let expected_home = home.display().to_string();
    let env = entry.get("env").and_then(serde_json::Value::as_object);
    object.keys().all(|k| allowed.contains(&k.as_str()))
        && matches!(
            entry.get("type").and_then(serde_json::Value::as_str),
            None | Some("stdio")
        )
        && entry.get("command").and_then(serde_json::Value::as_str)
            == Some(binary.to_string_lossy().as_ref())
        && serves_mcp(&args)
        && env.is_some_and(|e| {
            e.len() == 1
                && e.get("RILTRA_HOME").and_then(serde_json::Value::as_str)
                    == Some(expected_home.as_str())
        })
}

/// Claude sources must be ordered user, project, then local; include managed
/// policy state in `all_sources_known` before claiming absence or a no-op.
pub(crate) fn inspect_claude(
    sources: &[ClaudeConfigSource],
    all_sources_known: bool,
    binary: &Path,
    home: &Path,
) -> SetupReport {
    if !binary.is_file() {
        return preview(Client::Claude, "claude", binary, home);
    }
    let mut selected = None;
    for source in sources {
        let bytes = match std::fs::read(&source.config.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return preview(Client::Claude, "claude", binary, home),
        };
        let value = match parse_json(&bytes) {
            Ok(v) => v,
            Err(()) => return preview(Client::Claude, "claude", binary, home),
        };
        match claude_entry(&value, source) {
            Ok(Some(entry)) => selected = Some((entry.clone(), source.config.scope)),
            Ok(None) => {}
            Err(()) => return preview(Client::Claude, "claude", binary, home),
        }
    }
    let Some((entry, scope)) = selected else {
        let mut report = preview(Client::Claude, "claude", binary, home);
        report.scope = Some(if all_sources_known {
            SetupScope::UserGlobal
        } else {
            SetupScope::Unknown
        });
        report.configuration = if all_sources_known {
            ConfigurationStatus::Absent
        } else {
            ConfigurationStatus::ManualRequired
        };
        report.status = if all_sources_known {
            SetupStatus::Planned
        } else {
            SetupStatus::ManualRequired
        };
        report.reason = Some(if all_sources_known {
            "No riltra entry exists in the inspected Claude scopes; review the user-scope command preview.".into()
        } else {
            "No riltra entry was found in the inspected scope; other Claude scopes or managed policy are unknown.".into()
        });
        return report;
    };
    if !all_sources_known {
        let mut report = preview(Client::Claude, "claude", binary, home);
        report.configuration = ConfigurationStatus::Found;
        report.scope = Some(scope);
        report.reason = Some("A riltra entry exists in the inspected Claude scope, but higher-precedence scopes or policy are unknown.".into());
        return report;
    }
    if claude_matches(&entry, binary, home) {
        let mut report = base(
            Client::Claude,
            SetupStatus::AlreadyConfigured,
            ConfigurationStatus::Found,
            "Matching Claude riltra entry found; no changes planned.",
        );
        report.scope = Some(scope);
        report.reason = None;
        report
    } else {
        let mut report = base(
            Client::Claude,
            SetupStatus::ManualRequired,
            ConfigurationStatus::Conflict,
            "A different Claude riltra entry exists and was preserved; resolve it manually.",
        );
        report.scope = Some(scope);
        report
    }
}

fn codex_report_from_get(
    executable: &str,
    binary: &Path,
    home: &Path,
    exit_code: Option<i32>,
    output: &[u8],
) -> SetupReport {
    let mut report = preview(Client::Codex, executable, binary, home);
    if !binary.is_file() {
        return report;
    }
    if exit_code != Some(0) {
        report.reason = Some("Codex targeted mcp get did not establish the selected entry; use the preview and inspect manually.".into());
        return report;
    }
    let Ok(value) = parse_json(output) else {
        return report;
    };
    let Some(entry) = value.as_object() else {
        return report;
    };
    if entry.get("name").and_then(serde_json::Value::as_str) != Some("riltra") {
        return report;
    }
    report.configuration = ConfigurationStatus::Found;
    report.scope = Some(SetupScope::Unknown);
    let allowed_entry_keys = [
        "name",
        "enabled",
        "disabled_reason",
        "enabled_tools",
        "disabled_tools",
        "transport",
        "startup_timeout_sec",
        "tool_timeout_sec",
    ];
    let Some(transport) = entry
        .get("transport")
        .and_then(serde_json::Value::as_object)
    else {
        report.configuration = ConfigurationStatus::Conflict;
        report.reason =
            Some("The selected Codex riltra transport is unsupported and was preserved.".into());
        return report;
    };
    let allowed_transport_keys = ["type", "command", "args", "env", "env_vars", "cwd"];
    let args: Vec<String> = transport
        .get("args")
        .and_then(serde_json::Value::as_array)
        .and_then(|v| v.iter().map(|x| x.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let expected_home = home.display().to_string();
    let env = transport.get("env").and_then(serde_json::Value::as_object);
    let env_vars_empty = transport
        .get("env_vars")
        .map(|v| v.as_array().is_some_and(Vec::is_empty))
        .unwrap_or(false);
    let transport_safe = transport
        .keys()
        .all(|key| allowed_transport_keys.contains(&key.as_str()))
        && transport.get("type").and_then(serde_json::Value::as_str) == Some("stdio")
        && transport.get("cwd").is_some_and(serde_json::Value::is_null);
    let entry_safe = entry
        .keys()
        .all(|key| allowed_entry_keys.contains(&key.as_str()))
        && entry.get("enabled").and_then(serde_json::Value::as_bool) == Some(true)
        && entry
            .get("disabled_reason")
            .is_some_and(serde_json::Value::is_null)
        && entry
            .get("startup_timeout_sec")
            .is_some_and(serde_json::Value::is_null)
        && entry
            .get("tool_timeout_sec")
            .is_some_and(serde_json::Value::is_null)
        && entry
            .get("enabled_tools")
            .is_some_and(serde_json::Value::is_null)
        && entry
            .get("disabled_tools")
            .is_some_and(serde_json::Value::is_null);
    let matches = entry_safe
        && transport_safe
        && env_vars_empty
        && transport.get("command").and_then(serde_json::Value::as_str)
            == Some(binary.to_string_lossy().as_ref())
        && serves_mcp(&args)
        && env.is_some_and(|e| {
            e.len() == 1
                && e.get("RILTRA_HOME").and_then(serde_json::Value::as_str)
                    == Some(expected_home.as_str())
        });
    if matches {
        report.status = SetupStatus::AlreadyConfigured;
        report.reason = None;
        report.argv.clear();
        report.environment_additions.clear();
    } else {
        report.configuration = ConfigurationStatus::Conflict;
        report.reason = Some(
            "The selected Codex riltra entry differs from this setup and was preserved.".into(),
        );
    }
    report
}

pub(crate) fn inspect_codex(
    executable: &Path,
    project_root: &Path,
    binary: &Path,
    home: &Path,
) -> SetupReport {
    inspect_codex_with(executable, binary, home, || {
        let argv = ["mcp", "get", "riltra", "--json"].map(str::to_owned);
        run_setup_process_in_dir(executable, Some(project_root), &argv, &[], SETUP_TIMEOUT)
            .map(|(status, output)| (status.code(), output))
    })
}

fn inspect_codex_with(
    executable: &Path,
    binary: &Path,
    home: &Path,
    inspect: impl FnOnce() -> Result<(Option<i32>, CapturedOutput), &'static str>,
) -> SetupReport {
    if !binary.is_file() {
        return preview(
            Client::Codex,
            &executable.display().to_string(),
            binary,
            home,
        );
    }
    let result = inspect();
    match result {
        Ok((_exit_code, output)) if output.stdout_overflow || output.stderr_overflow => {
            let mut report = preview(
                Client::Codex,
                &executable.display().to_string(),
                binary,
                home,
            );
            report.reason = Some(
                "Codex targeted mcp get output exceeded the capture limit; inspect manually."
                    .into(),
            );
            report
        }
        Ok((exit_code, output)) => codex_report_from_get(
            &executable.display().to_string(),
            binary,
            home,
            exit_code,
            &output.stdout,
        ),
        Err(reason) => {
            let mut report = preview(
                Client::Codex,
                &executable.display().to_string(),
                binary,
                home,
            );
            report.reason = Some(format!(
                "Codex targeted mcp get could not establish the selected entry ({reason}); inspect manually."
            ));
            report
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn apply_requires_confirmation_and_rechecks_plan() {
        let root = crate::testutil::tmpdir("agent-setup-apply-guard");
        let config = root.join("claude.json");
        fs::write(&config, "{}").unwrap();
        let source = ClaudeConfigSource {
            config: ConfigSource {
                path: config.clone(),
                scope: SetupScope::UserGlobal,
            },
            project_root: None,
        };
        let binary = root.join("riltra.exe");
        fs::write(&binary, b"placeholder").unwrap();
        let home = root.join("home");
        let report = inspect_claude(std::slice::from_ref(&source), true, &binary, &home);
        assert_eq!(report.status, SetupStatus::Planned);
        let marker = root.join("spawned");
        let plan = stubbed_plan(
            report.clone(),
            std::slice::from_ref(&source),
            &binary,
            &home,
            &marker,
            "marker",
        );

        let cancelled = apply_setup(plan, false, || panic!("cancelled setup must not reinspect"));
        assert_eq!(cancelled.status, SetupStatus::Cancelled);
        assert!(!marker.exists());
        assert_eq!(fs::read(&config).unwrap(), b"{}");

        let plan = stubbed_plan(
            report,
            std::slice::from_ref(&source),
            &binary,
            &home,
            &marker,
            "marker",
        );
        fs::write(&config, "{\"changed\":true}").unwrap();
        let stale = apply_setup(plan, true, || (true, vec![source.clone()]));
        assert_eq!(stale.status, SetupStatus::ManualRequired);
        assert!(!marker.exists());
    }

    fn stubbed_plan(
        report: SetupReport,
        sources: &[ClaudeConfigSource],
        binary: &Path,
        home: &Path,
        marker: &Path,
        mode: &str,
    ) -> SetupPlan {
        let executable = std::env::current_exe().unwrap();
        let mut plan = SetupPlan::new_for_test(report, sources, binary, home, &executable);
        plan.test_process = Some(TestProcess {
            executable,
            argv: vec![
                "--exact".into(),
                "agent_setup::tests::setup_executor_process_stub".into(),
                "--nocapture".into(),
            ],
            environment: vec![
                EnvironmentAddition {
                    name: "RILTRA_TEST_STUB_MARKER".into(),
                    value: marker.display().to_string(),
                },
                EnvironmentAddition {
                    name: "RILTRA_TEST_STUB_MODE".into(),
                    value: mode.into(),
                },
                EnvironmentAddition {
                    name: "RILTRA_TEST_STUB_CONFIG".into(),
                    value: sources[0].config.path.display().to_string(),
                },
                EnvironmentAddition {
                    name: "RILTRA_TEST_STUB_BINARY".into(),
                    value: binary.display().to_string(),
                },
            ],
        });
        plan
    }

    #[test]
    fn setup_executor_applies_only_after_confirmation_and_reports_process_failures() {
        let root = crate::testutil::tmpdir("agent-setup-apply-result");
        let config = root.join("claude.json");
        fs::write(&config, "{}").unwrap();
        let source = ClaudeConfigSource {
            config: ConfigSource {
                path: config.clone(),
                scope: SetupScope::UserGlobal,
            },
            project_root: None,
        };
        let binary = root.join("riltra.exe");
        fs::write(&binary, b"placeholder").unwrap();
        let home = root.join("home");
        let marker = root.join("started");
        let make_plan = |mode: &str| {
            stubbed_plan(
                inspect_claude(std::slice::from_ref(&source), true, &binary, &home),
                std::slice::from_ref(&source),
                &binary,
                &home,
                &marker,
                mode,
            )
        };
        let recheck = || (true, vec![source.clone()]);

        let applied = apply_setup(make_plan("marker"), true, recheck);
        assert_eq!(applied.status, SetupStatus::Applied);
        assert_eq!(applied.configuration, ConfigurationStatus::Absent);
        assert_eq!(applied.probe, ProbeStatus::NotRequested);
        assert_eq!(applied.connection, ConnectionStatus::NotObserved);
        assert_eq!(fs::read(&marker).unwrap(), b"started");
        assert_eq!(fs::read(&config).unwrap(), b"{}");

        fs::remove_file(&marker).unwrap();
        let applied = apply_setup(make_plan("apply_config"), true, recheck);
        assert_eq!(applied.status, SetupStatus::Applied);
        assert_eq!(applied.configuration, ConfigurationStatus::Found);
        assert_eq!(applied.probe, ProbeStatus::NotRequested);
        assert_eq!(applied.connection, ConnectionStatus::NotObserved);

        fs::write(&config, "{}").unwrap();
        fs::remove_file(&marker).unwrap();
        let failed = apply_setup(make_plan("fail"), true, || (true, vec![source.clone()]));
        assert_eq!(failed.status, SetupStatus::Failed);
        assert_eq!(failed.configuration, ConfigurationStatus::ManualRequired);
        assert!(failed.reason.as_deref().unwrap().contains("code 7"));
        assert_eq!(fs::read(&marker).unwrap(), b"started");
        assert_eq!(fs::read(&config).unwrap(), b"{}");
    }

    #[test]
    fn setup_executor_successful_process_stays_applied_when_post_apply_layers_are_unknown() {
        let root = crate::testutil::tmpdir("agent-setup-post-apply-unknown");
        let config = root.join("claude.json");
        fs::write(&config, "{}").unwrap();
        let source = ClaudeConfigSource {
            config: ConfigSource {
                path: config.clone(),
                scope: SetupScope::UserGlobal,
            },
            project_root: None,
        };
        let binary = root.join("riltra.exe");
        fs::write(&binary, b"placeholder").unwrap();
        let home = root.join("home");
        let marker = root.join("started");
        let plan = stubbed_plan(
            inspect_claude(std::slice::from_ref(&source), true, &binary, &home),
            std::slice::from_ref(&source),
            &binary,
            &home,
            &marker,
            "apply_config",
        );
        let mut resolutions = 0;
        let report = apply_setup(plan, true, || {
            resolutions += 1;
            if resolutions == 1 {
                (true, vec![source.clone()])
            } else {
                (false, vec![source.clone()])
            }
        });
        assert_eq!(report.status, SetupStatus::Applied);
        assert_eq!(report.configuration, ConfigurationStatus::ManualRequired);
        assert_eq!(report.probe, ProbeStatus::NotRequested);
        assert_eq!(report.connection, ConnectionStatus::NotObserved);
        assert!(report
            .reason
            .as_deref()
            .unwrap()
            .contains("could not be verified"));
        assert_eq!(resolutions, 2);
        assert_eq!(fs::read(&marker).unwrap(), b"started");
        assert_ne!(fs::read(&config).unwrap(), b"{}");
    }

    #[test]
    fn setup_executor_does_not_claim_user_global_when_only_project_entry_is_found() {
        let root = crate::testutil::tmpdir("agent-setup-project-only-after-apply");
        let global_path = root.join("claude-user.json");
        let project_path = root.join("claude-project.json");
        fs::write(&global_path, "{}").unwrap();
        fs::write(&project_path, "{}").unwrap();
        let project_root = "/workspace/project";
        let sources = vec![
            ClaudeConfigSource {
                config: ConfigSource {
                    path: global_path.clone(),
                    scope: SetupScope::UserGlobal,
                },
                project_root: None,
            },
            ClaudeConfigSource {
                config: ConfigSource {
                    path: project_path.clone(),
                    scope: SetupScope::ProjectLocal,
                },
                project_root: Some(project_root.into()),
            },
        ];
        let binary = root.join("riltra.exe");
        fs::write(&binary, b"placeholder").unwrap();
        let home = root.join("home");
        let marker = root.join("started");
        let plan = stubbed_plan(
            inspect_claude(&sources, true, &binary, &home),
            &sources,
            &binary,
            &home,
            &marker,
            "marker",
        );
        assert_eq!(plan.report().scope, Some(SetupScope::UserGlobal));
        let mut resolutions = 0;
        let report = apply_setup(plan, true, || {
            resolutions += 1;
            if resolutions == 2 {
                let entry = serde_json::json!({
                    "command": binary.display().to_string(),
                    "args": ["mcp"],
                    "env": {"RILTRA_HOME": home.display().to_string()}
                });
                fs::write(
                    &project_path,
                    serde_json::json!({"projects": {
                        project_root: {"mcpServers": {"riltra": entry}}
                    }})
                    .to_string(),
                )
                .unwrap();
            }
            (true, sources.clone())
        });

        assert_eq!(report.status, SetupStatus::Applied);
        assert_eq!(report.configuration, ConfigurationStatus::ManualRequired);
        assert_eq!(report.scope, Some(SetupScope::ProjectLocal));
        assert!(report
            .reason
            .as_deref()
            .unwrap()
            .contains("user-global configuration was not verified"));
        assert_eq!(resolutions, 2);
        assert_eq!(fs::read(&global_path).unwrap(), b"{}");
        assert_eq!(fs::read(&marker).unwrap(), b"started");
    }

    #[test]
    fn setup_plan_rejects_unpreviewed_argv_environment_and_relative_paths() {
        let root = crate::testutil::tmpdir("agent-setup-plan-validation");
        let config = root.join("claude.json");
        fs::write(&config, "{}").unwrap();
        let source = ClaudeConfigSource {
            config: ConfigSource {
                path: config,
                scope: SetupScope::UserGlobal,
            },
            project_root: None,
        };
        let home = root.join("home");
        let binary = root.join("riltra.exe");
        fs::write(&binary, b"stub").unwrap();
        let selected = std::env::current_exe().unwrap();

        for mutate in ["executable", "argv", "environment"] {
            let mut altered = inspect_claude(std::slice::from_ref(&source), true, &binary, &home);
            match mutate {
                "executable" => altered.executable = Some(selected.display().to_string()),
                "argv" => altered.argv = vec!["unpreviewed-command".into()],
                "environment" => altered.environment_additions[0].value = "other-home".into(),
                _ => unreachable!(),
            }
            let rejected = SetupPlan::new(altered, std::slice::from_ref(&source), &binary, &home);
            assert_eq!(
                rejected.report().status,
                SetupStatus::ManualRequired,
                "{mutate}"
            );
            assert!(rejected
                .report()
                .reason
                .as_deref()
                .unwrap()
                .contains("arguments or environment"));
        }

        let relative_binary = Path::new("Cargo.toml");
        assert!(relative_binary.is_file());
        let relative_binary_report =
            inspect_claude(std::slice::from_ref(&source), true, relative_binary, &home);
        let rejected = SetupPlan::new(
            relative_binary_report,
            std::slice::from_ref(&source),
            relative_binary,
            &home,
        );
        assert_eq!(rejected.report().status, SetupStatus::ManualRequired);

        let relative_home = Path::new(".");
        let relative_home_report =
            inspect_claude(std::slice::from_ref(&source), true, &binary, relative_home);
        let rejected = SetupPlan::new(
            relative_home_report,
            std::slice::from_ref(&source),
            &binary,
            relative_home,
        );
        assert_eq!(rejected.report().status, SetupStatus::ManualRequired);
    }

    #[test]
    fn setup_executor_rechecks_bytes_after_inspection_before_spawn() {
        let root = crate::testutil::tmpdir("agent-setup-inspection-race");
        let config = root.join("claude.json");
        fs::write(&config, "{}").unwrap();
        let source = ClaudeConfigSource {
            config: ConfigSource {
                path: config.clone(),
                scope: SetupScope::UserGlobal,
            },
            project_root: None,
        };
        let binary = root.join("riltra.exe");
        fs::write(&binary, b"placeholder").unwrap();
        let home = root.join("home");
        let marker = root.join("spawned");
        let report = inspect_claude(std::slice::from_ref(&source), true, &binary, &home);
        let plan = stubbed_plan(
            report,
            std::slice::from_ref(&source),
            &binary,
            &home,
            &marker,
            "marker",
        );
        let result = apply_setup_with_after_snapshot_hook(
            plan,
            true,
            || (true, vec![source.clone()]),
            || fs::write(&config, r#"{"unrelated":"changed","mcpServers":{}}"#).unwrap(),
        );
        assert_eq!(result.status, SetupStatus::ManualRequired);
        assert!(!marker.exists());
    }

    #[test]
    fn timeout_cleanup_confirms_owned_child_reaped_and_stopped() {
        let root = crate::testutil::tmpdir("agent-setup-reap-proof");
        let started = root.join("child-started");
        let late = root.join("child-late");
        let executable = std::env::current_exe().unwrap();
        let mut child = Command::new(executable)
            .args([
                "--exact",
                "agent_setup::tests::setup_executor_process_stub",
                "--nocapture",
            ])
            .env("RILTRA_TEST_STUB_MARKER", &started)
            .env("RILTRA_TEST_STUB_LATE_MARKER", &late)
            .env("RILTRA_TEST_STUB_MODE", "sleep_late")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !started.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(started.exists());
        assert!(kill_and_reap(&mut child));
        assert!(child.try_wait().unwrap().is_some());
        std::thread::sleep(std::time::Duration::from_millis(700));
        assert!(!late.exists());
    }

    #[test]
    fn setup_executor_rejects_changed_source_set_and_existing_or_manual_reports() {
        let root = crate::testutil::tmpdir("agent-setup-apply-source-set");
        let config = root.join("claude.json");
        fs::write(&config, "{}").unwrap();
        let source = ClaudeConfigSource {
            config: ConfigSource {
                path: config.clone(),
                scope: SetupScope::UserGlobal,
            },
            project_root: None,
        };
        let binary = root.join("riltra.exe");
        fs::write(&binary, b"placeholder").unwrap();
        let home = root.join("home");
        let marker = root.join("started");
        let make_plan = || {
            stubbed_plan(
                inspect_claude(std::slice::from_ref(&source), true, &binary, &home),
                std::slice::from_ref(&source),
                &binary,
                &home,
                &marker,
                "marker",
            )
        };
        let extra = ClaudeConfigSource {
            config: ConfigSource {
                path: root.join("newly-resolved-layer.json"),
                scope: SetupScope::ProjectLocal,
            },
            project_root: None,
        };
        let stale = apply_setup(make_plan(), true, || {
            (true, vec![source.clone(), extra.clone()])
        });
        assert_eq!(stale.status, SetupStatus::ManualRequired);
        assert!(!marker.exists());

        let incomplete = apply_setup(make_plan(), true, || (false, vec![source.clone()]));
        assert_eq!(incomplete.status, SetupStatus::ManualRequired);
        assert!(!marker.exists());

        for status in [SetupStatus::AlreadyConfigured, SetupStatus::ManualRequired] {
            let report = if status == SetupStatus::AlreadyConfigured {
                fs::write(
                    &config,
                    serde_json::json!({"mcpServers":{"riltra":{
                        "command":binary.display().to_string(),
                        "args":["mcp"],
                        "env":{"RILTRA_HOME":home.display().to_string()}
                    }}})
                    .to_string(),
                )
                .unwrap();
                inspect_claude(std::slice::from_ref(&source), true, &binary, &home)
            } else {
                fs::write(
                    &config,
                    r#"{"mcpServers":{"riltra":{"command":"other","args":[]}}}"#,
                )
                .unwrap();
                inspect_claude(std::slice::from_ref(&source), true, &binary, &home)
            };
            assert_eq!(report.status, status);
            let result = apply_setup(
                SetupPlan::new(report, std::slice::from_ref(&source), &binary, &home),
                false,
                || panic!("no-op and manual plans must not reinspect or launch"),
            );
            assert_eq!(result.status, status);
            assert!(!marker.exists());
        }
    }

    #[test]
    fn setup_executor_keeps_missing_client_executable_manual() {
        let root = crate::testutil::tmpdir("agent-setup-missing-client");
        let config = root.join("claude.json");
        fs::write(&config, "{}").unwrap();
        let source = ClaudeConfigSource {
            config: ConfigSource {
                path: config.clone(),
                scope: SetupScope::UserGlobal,
            },
            project_root: None,
        };
        let binary = root.join("riltra.exe");
        fs::write(&binary, b"placeholder").unwrap();
        let home = root.join("home");
        let marker = root.join("started");
        let report = inspect_claude(std::slice::from_ref(&source), true, &binary, &home);
        let plan = SetupPlan::new_for_test(
            report,
            std::slice::from_ref(&source),
            &binary,
            &home,
            &root.join("missing-claude.exe"),
        );
        assert_eq!(plan.report().status, SetupStatus::ManualRequired);
        assert!(plan
            .report()
            .reason
            .as_deref()
            .unwrap()
            .contains("executable is unavailable"));
        let result = apply_setup(plan, true, || panic!("unavailable client must not spawn"));
        assert_eq!(result.status, SetupStatus::ManualRequired);
        assert!(!marker.exists());
        assert_eq!(fs::read(&config).unwrap(), b"{}");
        assert!(run_setup_process(
            &root.join("missing-claude.exe"),
            &[],
            &[],
            std::time::Duration::from_millis(100)
        )
        .is_err());
    }

    #[test]
    fn setup_executor_bounds_output_and_reaps_timed_out_stub() {
        let root = crate::testutil::tmpdir("agent-setup-apply-process");
        let marker = root.join("spawned");
        let executable = std::env::current_exe().unwrap();
        let args = vec![
            "--exact".into(),
            "agent_setup::tests::setup_executor_process_stub".into(),
            "--nocapture".into(),
        ];
        let env = vec![
            EnvironmentAddition {
                name: "RILTRA_TEST_STUB_MARKER".into(),
                value: marker.display().to_string(),
            },
            EnvironmentAddition {
                name: "RILTRA_TEST_STUB_MODE".into(),
                value: "flood".into(),
            },
        ];
        let (status, output) =
            run_setup_process(&executable, &args, &env, std::time::Duration::from_secs(5)).unwrap();
        assert!(status.success());
        assert!(marker.exists());
        assert!(output.stdout.len() <= MAX_SETUP_OUTPUT_BYTES);
        assert!(output.stderr.len() <= MAX_SETUP_OUTPUT_BYTES);
        assert!(output.stdout_overflow);
        assert!(output.stderr_overflow);

        let timed = vec![
            EnvironmentAddition {
                name: "RILTRA_TEST_STUB_MARKER".into(),
                value: marker.display().to_string(),
            },
            EnvironmentAddition {
                name: "RILTRA_TEST_STUB_MODE".into(),
                value: "sleep".into(),
            },
        ];
        let started = std::time::Instant::now();
        assert!(run_setup_process(
            &executable,
            &args,
            &timed,
            std::time::Duration::from_millis(100)
        )
        .is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));

        let holder_marker = root.join("holder-finished");
        let held = vec![
            EnvironmentAddition {
                name: "RILTRA_TEST_STUB_MARKER".into(),
                value: marker.display().to_string(),
            },
            EnvironmentAddition {
                name: "RILTRA_TEST_STUB_MODE".into(),
                value: "hold_pipe".into(),
            },
            EnvironmentAddition {
                name: "RILTRA_TEST_STUB_HOLDER_MARKER".into(),
                value: holder_marker.display().to_string(),
            },
        ];
        let started = std::time::Instant::now();
        assert!(run_setup_process(
            &executable,
            &args,
            &held,
            std::time::Duration::from_millis(200)
        )
        .is_err());
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        let done_by = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !holder_marker.exists() && std::time::Instant::now() < done_by {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(holder_marker.exists(), "test descendant exits on its own");
    }

    #[test]
    fn setup_executor_process_stub() {
        let Ok(marker) = std::env::var("RILTRA_TEST_STUB_MARKER") else {
            return;
        };
        fs::write(marker, b"started").unwrap();
        match std::env::var("RILTRA_TEST_STUB_MODE").as_deref() {
            Ok("sleep") => std::thread::sleep(std::time::Duration::from_secs(30)),
            Ok("sleep_late") => {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let late = std::env::var_os("RILTRA_TEST_STUB_LATE_MARKER").unwrap();
                fs::write(late, b"still-running").unwrap();
            }
            Ok("fail") => std::process::exit(7),
            Ok("flood") => {
                use std::io::Write;
                let block = vec![b'x'; 8192];
                for _ in 0..32 {
                    std::io::stdout().write_all(&block).unwrap();
                    std::io::stderr().write_all(&block).unwrap();
                }
            }
            Ok("apply_config") => {
                let config = std::env::var_os("RILTRA_TEST_STUB_CONFIG").unwrap();
                let binary = std::env::var_os("RILTRA_TEST_STUB_BINARY").unwrap();
                let home = std::env::var_os("RILTRA_HOME").unwrap();
                fs::write(
                    config,
                    serde_json::json!({"mcpServers":{"riltra":{
                        "command":PathBuf::from(binary).display().to_string(),
                        "args":["mcp"],
                        "env":{"RILTRA_HOME":PathBuf::from(home).display().to_string()}
                    }}})
                    .to_string(),
                )
                .unwrap();
            }
            Ok("hold_pipe") => {
                use std::process::Stdio;
                let holder_marker = std::env::var_os("RILTRA_TEST_STUB_HOLDER_MARKER").unwrap();
                // This descendant intentionally keeps inherited pipes open after its parent exits.
                #[allow(clippy::zombie_processes)]
                let _holder = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "agent_setup::tests::setup_executor_pipe_holder_stub",
                        "--nocapture",
                    ])
                    .env("RILTRA_TEST_STUB_MODE", "hold")
                    .env("RILTRA_TEST_STUB_HOLDER_MARKER", holder_marker)
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .unwrap();
            }
            _ => {}
        }
    }

    #[test]
    fn setup_executor_pipe_holder_stub() {
        if std::env::var("RILTRA_TEST_STUB_MODE").as_deref() == Ok("hold") {
            std::thread::sleep(std::time::Duration::from_millis(750));
            let marker = std::env::var_os("RILTRA_TEST_STUB_HOLDER_MARKER").unwrap();
            fs::write(marker, b"done").unwrap();
        }
    }

    #[test]
    fn opencode_inspection_never_invokes_cli() {
        let root = crate::testutil::tmpdir("agent-setup-absent");
        let profile = root.join("config with ü/opencode.json");
        let bin = root.join("Riltra bin/riltra.exe");
        let home = root.join("riltra home");
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::write(&bin, b"test executable").unwrap();
        let report = inspect_opencode(
            &[ConfigSource {
                path: profile.clone(),
                scope: SetupScope::UserGlobal,
            }],
            false,
            &bin,
            &home,
        );
        assert_eq!(report.status, SetupStatus::ManualRequired);
        assert_eq!(report.configuration, ConfigurationStatus::ManualRequired);
        assert_eq!(report.executable.as_deref(), Some("opencode"));
        assert_eq!(report.argv, opencode_argv(&bin, &home));
        assert!(report
            .reason
            .as_deref()
            .unwrap()
            .contains("higher-precedence layers are unknown"));
        assert!(!profile.exists());
    }

    #[test]
    fn missing_riltra_executable_keeps_a_manual_preview() {
        let root = crate::testutil::tmpdir("agent-setup-missing-binary");
        let binary = root.join("missing riltra.exe");
        let report = preview(Client::Codex, "codex", &binary, &root.join("home"));
        assert_eq!(report.status, SetupStatus::ManualRequired);
        assert!(report
            .reason
            .as_deref()
            .unwrap()
            .contains("executable is missing"));
        assert!(report.argv.contains(&binary.display().to_string()));
    }

    #[test]
    fn opencode_matching_entry_is_a_noop_and_conflicts_are_preserved() {
        let root = crate::testutil::tmpdir("agent-setup-config");
        let path = root.join("opencode.json");
        let bin = root.join("riltra.exe");
        let home = root.join("riltra-home");
        fs::write(&bin, b"test executable").unwrap();
        let config = serde_json::json!({
            "mcp": {"servers": {
                "other": {"type": "local", "command": ["noop"]},
                "riltra": {
                    "type": "local", "command": [bin.display().to_string(), "mcp"],
                    "environment": {"RILTRA_HOME": home.display().to_string()}
                }
            }}
        });
        fs::write(&path, config.to_string()).unwrap();
        let sources = [ConfigSource {
            path: path.clone(),
            scope: SetupScope::UserGlobal,
        }];
        let report = inspect_opencode(&sources, true, &bin, &home);
        assert_eq!(report.status, SetupStatus::AlreadyConfigured);
        assert_eq!(report.configuration, ConfigurationStatus::Found);
        assert!(report.argv.is_empty());
        fs::write(
            &path,
            r#"{"mcp":{"servers":{"riltra":{"type":"local","command":["other"]}}}}"#,
        )
        .unwrap();
        let before = fs::read(&path).unwrap();
        let report = inspect_opencode(&sources, true, &bin, &home);
        assert_eq!(report.status, SetupStatus::ManualRequired);
        assert_eq!(report.configuration, ConfigurationStatus::Conflict);
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn opencode_ambiguous_and_unsupported_clients_require_manual_setup() {
        let root = crate::testutil::tmpdir("agent-setup-ambiguous");
        let path = root.join("opencode.json");
        let bin = root.join("riltra.exe");
        let home = root.join("home");
        fs::write(&bin, b"test executable").unwrap();
        fs::write(&path, r#"{"mcp":{"servers":{},"servers":{}}}"#).unwrap();
        assert_eq!(
            inspect_opencode(
                &[ConfigSource {
                    path: path.clone(),
                    scope: SetupScope::UserGlobal
                }],
                true,
                &bin,
                &home
            )
            .configuration,
            ConfigurationStatus::ManualRequired
        );
        fs::write(&path, r#"{ "mcp": { "servers": {}, }, }"#).unwrap();
        assert_eq!(
            inspect_opencode(
                &[ConfigSource {
                    path: path.clone(),
                    scope: SetupScope::UserGlobal
                }],
                true,
                &bin,
                &home
            )
            .configuration,
            ConfigurationStatus::ManualRequired
        );
        fs::write(
            &path,
            r#"{"mcp":{"riltra":{"type":"local","command":["x"]}}}"#,
        )
        .unwrap();
        assert_eq!(
            inspect_opencode(
                &[ConfigSource {
                    path: path.clone(),
                    scope: SetupScope::UserGlobal
                }],
                true,
                &bin,
                &home,
            )
            .configuration,
            ConfigurationStatus::ManualRequired
        );
        assert_eq!(
            inspect_opencode(
                &[ConfigSource {
                    path: root.join("missing"),
                    scope: SetupScope::UserGlobal
                }],
                true,
                &bin,
                &home
            )
            .configuration,
            ConfigurationStatus::Absent
        );
        assert_eq!(
            preview(Client::Claude, "claude", &bin, &home).status,
            SetupStatus::ManualRequired
        );
        assert_eq!(
            preview(Client::Codex, "codex", &bin, &home).status,
            SetupStatus::ManualRequired
        );
        let unreadable = root.join("unreadable-config");
        fs::create_dir(&unreadable).unwrap();
        assert_eq!(
            inspect_opencode(
                &[ConfigSource {
                    path: unreadable,
                    scope: SetupScope::UserGlobal
                }],
                true,
                &bin,
                &home
            )
            .configuration,
            ConfigurationStatus::ManualRequired
        );
    }

    #[test]
    fn non_object_config_roots_and_claude_project_sections_require_manual_setup() {
        let root = crate::testutil::tmpdir("agent-setup-non-object-config");
        let binary = root.join("riltra.exe");
        let home = root.join("home");
        fs::write(&binary, b"test executable").unwrap();

        let opencode_path = root.join("opencode.json");
        let opencode_source = ConfigSource {
            path: opencode_path.clone(),
            scope: SetupScope::UserGlobal,
        };
        for json in ["null", "[]", r#""config""#] {
            fs::write(&opencode_path, json).unwrap();
            let report =
                inspect_opencode(std::slice::from_ref(&opencode_source), true, &binary, &home);
            assert_eq!(report.status, SetupStatus::ManualRequired, "{json}");
            assert_eq!(
                report.configuration,
                ConfigurationStatus::ManualRequired,
                "{json}"
            );
        }
        fs::write(&opencode_path, "{}").unwrap();
        assert_eq!(
            inspect_opencode(&[opencode_source], true, &binary, &home).configuration,
            ConfigurationStatus::Absent
        );

        let claude_path = root.join("claude.json");
        let claude_source = ClaudeConfigSource {
            config: ConfigSource {
                path: claude_path.clone(),
                scope: SetupScope::ProjectLocal,
            },
            project_root: Some("/workspace/project".into()),
        };
        for json in ["null", "[]", r#""config""#] {
            fs::write(&claude_path, json).unwrap();
            let report = inspect_claude(std::slice::from_ref(&claude_source), true, &binary, &home);
            assert_eq!(report.status, SetupStatus::ManualRequired, "{json}");
            assert_eq!(
                report.configuration,
                ConfigurationStatus::ManualRequired,
                "{json}"
            );
        }
        fs::write(&claude_path, r#"{"projects":{"/workspace/project":null}}"#).unwrap();
        let report = inspect_claude(std::slice::from_ref(&claude_source), true, &binary, &home);
        assert_eq!(report.status, SetupStatus::ManualRequired);
        assert_eq!(report.configuration, ConfigurationStatus::ManualRequired);

        fs::write(&claude_path, "{}").unwrap();
        assert_eq!(
            inspect_claude(&[claude_source], true, &binary, &home).configuration,
            ConfigurationStatus::Absent
        );
    }

    #[test]
    fn opencode_higher_precedence_override_and_unknown_layers_never_claim_noop() {
        let root = crate::testutil::tmpdir("agent-setup-layered-opencode");
        let binary = root.join("riltra.exe");
        let home = root.join("riltra-home");
        fs::write(&binary, b"test executable").unwrap();
        let global = root.join("global.json");
        let project = root.join("project.json");
        let matching = serde_json::json!({"type":"local","command":[binary.display().to_string(),"mcp"],"environment":{"RILTRA_HOME":home.display().to_string()}});
        fs::write(
            &global,
            serde_json::json!({"mcp":{"servers":{"riltra":matching}}}).to_string(),
        )
        .unwrap();
        fs::write(
            &project,
            r#"{"mcp":{"servers":{"riltra":{"type":"local","command":["different"]}}}}"#,
        )
        .unwrap();
        let sources = [
            ConfigSource {
                path: global.clone(),
                scope: SetupScope::UserGlobal,
            },
            ConfigSource {
                path: project.clone(),
                scope: SetupScope::ProjectLocal,
            },
        ];
        let report = inspect_opencode(&sources, true, &binary, &home);
        assert_eq!(report.status, SetupStatus::ManualRequired);
        assert_eq!(report.configuration, ConfigurationStatus::ManualRequired);
        let partial = inspect_opencode(&sources[..1], false, &binary, &home);
        assert_eq!(partial.status, SetupStatus::ManualRequired);
        assert_ne!(partial.configuration, ConfigurationStatus::Absent);
        assert_ne!(partial.status, SetupStatus::AlreadyConfigured);

        // Even an identical higher-precedence definition is an override and
        // stays manual under the Task 4 contract.
        fs::write(
            &project,
            serde_json::json!({"mcp":{"servers":{"riltra":matching}}}).to_string(),
        )
        .unwrap();
        let matching_override = inspect_opencode(&sources, true, &binary, &home);
        assert_eq!(matching_override.status, SetupStatus::ManualRequired);
        assert_eq!(
            matching_override.configuration,
            ConfigurationStatus::ManualRequired
        );
    }

    #[test]
    fn disabled_opencode_entry_is_not_reported_as_already_configured() {
        let root = crate::testutil::tmpdir("agent-setup-disabled");
        let binary = root.join("riltra.exe");
        let home = root.join("riltra-home");
        let path = root.join("opencode.json");
        fs::write(&binary, b"test executable").unwrap();
        fs::write(
            &path,
            serde_json::json!({"mcp":{"servers":{"riltra":{
                "type":"local", "command":[binary.display().to_string(),"mcp"],
                "environment":{"RILTRA_HOME":home.display().to_string()}, "disabled":true
            }}}})
            .to_string(),
        )
        .unwrap();
        let report = inspect_opencode(
            &[ConfigSource {
                path,
                scope: SetupScope::UserGlobal,
            }],
            true,
            &binary,
            &home,
        );
        assert_eq!(report.status, SetupStatus::ManualRequired);
        assert_eq!(report.configuration, ConfigurationStatus::Conflict);
        assert!(report.reason.as_deref().unwrap().contains("disabled"));
    }

    #[test]
    fn codex_and_claude_inspection_report_selected_entry_and_argv_preview() {
        let root = crate::testutil::tmpdir("agent-setup-client-previews");
        let binary = root.join("riltra bin.exe");
        let home = root.join("riltra home");
        fs::write(&binary, b"test executable").unwrap();
        let codex = codex_report_from_get(
            "codex",
            &binary,
            &home,
            Some(0),
            br#"{"name":"riltra","command":"x","args":[]}"#,
        );
        assert_eq!(codex.status, SetupStatus::ManualRequired);
        assert_eq!(codex.configuration, ConfigurationStatus::Conflict);
        assert_eq!(codex.executable.as_deref(), Some("codex"));
        assert_eq!(codex.scope, Some(SetupScope::Unknown));
        assert!(codex.argv.contains(&"--env".to_string()));
        let claude_path = root.join(".mcp.json");
        fs::write(
            &claude_path,
            serde_json::json!({"mcpServers":{"riltra":{
                "command":binary.display().to_string(), "args":["mcp"],
                "env":{"RILTRA_HOME":home.display().to_string()}
            }}})
            .to_string(),
        )
        .unwrap();
        let claude = inspect_claude(
            &[ClaudeConfigSource {
                config: ConfigSource {
                    path: claude_path,
                    scope: SetupScope::ProjectLocal,
                },
                project_root: None,
            }],
            true,
            &binary,
            &home,
        );
        assert_eq!(claude.status, SetupStatus::AlreadyConfigured);
        assert_eq!(claude.configuration, ConfigurationStatus::Found);
        assert_eq!(claude.scope, Some(SetupScope::ProjectLocal));
        assert!(claude.argv.is_empty());
        let claude_preview = preview(Client::Claude, "claude", &binary, &home);
        assert_eq!(claude_preview.scope, Some(SetupScope::UserGlobal));
        assert!(claude_preview
            .argv
            .windows(2)
            .any(|w| w[0] == "--env" && w[1].starts_with("RILTRA_HOME=")));
        assert!(claude_preview.argv.contains(&"--scope".to_string()));
    }

    #[test]
    fn codex_inspection_timeout_returns_manual_preview_without_claiming_absence() {
        let root = crate::testutil::tmpdir("agent-setup-codex-timeout");
        let binary = root.join("riltra.exe");
        let home = root.join("home");
        fs::write(&binary, b"test executable").unwrap();
        let executable = std::env::current_exe().unwrap();
        let args = vec![
            "--exact".into(),
            "agent_setup::tests::setup_executor_process_stub".into(),
            "--nocapture".into(),
        ];
        let environment = vec![
            EnvironmentAddition {
                name: "RILTRA_TEST_STUB_MARKER".into(),
                value: root.join("started").display().to_string(),
            },
            EnvironmentAddition {
                name: "RILTRA_TEST_STUB_MODE".into(),
                value: "sleep".into(),
            },
        ];
        let report = inspect_codex_with(&executable, &binary, &home, || {
            run_setup_process(
                &executable,
                &args,
                &environment,
                std::time::Duration::from_millis(100),
            )
            .map(|(status, output)| (status.code(), output))
        });
        assert_eq!(report.status, SetupStatus::ManualRequired);
        assert_eq!(report.configuration, ConfigurationStatus::ManualRequired);
        assert!(report.reason.as_deref().unwrap().contains("timed out"));
        assert_ne!(report.configuration, ConfigurationStatus::Absent);
    }

    #[test]
    fn codex_inspection_rejects_valid_json_prefix_when_capture_overflows() {
        let root = crate::testutil::tmpdir("agent-setup-codex-overflow");
        let binary = root.join("riltra.exe");
        let home = root.join("home");
        fs::write(&binary, b"test executable").unwrap();
        let matching = serde_json::json!({
            "name":"riltra",
            "enabled":true,
            "disabled_reason":null,
            "enabled_tools":null,
            "disabled_tools":null,
            "transport":{
                "type":"stdio",
                "command":binary.display().to_string(),
                "args":["mcp"],
                "env":{"RILTRA_HOME":home.display().to_string()},
                "env_vars":[],
                "cwd":null
            },
            "startup_timeout_sec":null,
            "tool_timeout_sec":null
        });
        let mut prefix = serde_json::to_vec(&matching).unwrap();
        assert!(prefix.len() < MAX_SETUP_OUTPUT_BYTES);
        assert_eq!(
            codex_report_from_get("codex", &binary, &home, Some(0), &prefix).status,
            SetupStatus::AlreadyConfigured
        );
        prefix.resize(MAX_SETUP_OUTPUT_BYTES, b' ');
        let report = inspect_codex_with(&root.join("codex.exe"), &binary, &home, || {
            Ok((
                Some(0),
                CapturedOutput {
                    stdout: prefix,
                    stderr: Vec::new(),
                    stdout_overflow: true,
                    stderr_overflow: false,
                },
            ))
        });
        assert_eq!(report.status, SetupStatus::ManualRequired);
        assert_eq!(report.configuration, ConfigurationStatus::ManualRequired);
        assert!(report
            .reason
            .as_deref()
            .unwrap()
            .contains("exceeded the capture limit"));
    }

    #[test]
    fn mcp_flags_in_either_order_are_the_same_server() {
        let words = |a: &[&str]| a.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        for good in [
            &["mcp"][..],
            &["mcp", "--compact"],
            &["mcp", "--all-tools", "--compact"],
            &["mcp", "--compact", "--all-tools"],
        ] {
            assert!(serves_mcp(&words(good)), "{good:?}");
        }
        for other in [
            &["mcp", "--compact", "--compact"][..],
            &["mcp", "--other"],
            &["--compact"],
        ] {
            assert!(!serves_mcp(&words(other)), "{other:?}");
        }
    }

    #[test]
    fn all_tools_entry_is_already_configured_for_every_client() {
        let root = crate::testutil::tmpdir("agent-setup-all-tools");
        let binary = root.join("riltra.exe");
        let home = root.join("riltra-home");
        fs::write(&binary, b"test executable").unwrap();
        let bin = binary.display().to_string();
        let home_text = home.display().to_string();

        let opencode = root.join("opencode.json");
        fs::write(
            &opencode,
            serde_json::json!({"mcp":{"servers":{"riltra":{
                "type":"local", "command":[bin.clone(), "mcp", "--all-tools"],
                "environment":{"RILTRA_HOME":home_text.clone()}
            }}}})
            .to_string(),
        )
        .unwrap();
        let report = inspect_opencode(
            &[ConfigSource {
                path: opencode,
                scope: SetupScope::UserGlobal,
            }],
            true,
            &binary,
            &home,
        );
        assert_eq!(report.status, SetupStatus::AlreadyConfigured);
        assert_eq!(report.configuration, ConfigurationStatus::Found);

        let claude = root.join(".mcp.json");
        fs::write(
            &claude,
            serde_json::json!({"mcpServers":{"riltra":{
                "command":bin.clone(), "args":["mcp", "--all-tools"],
                "env":{"RILTRA_HOME":home_text.clone()}
            }}})
            .to_string(),
        )
        .unwrap();
        let report = inspect_claude(
            &[ClaudeConfigSource {
                config: ConfigSource {
                    path: claude,
                    scope: SetupScope::ProjectLocal,
                },
                project_root: None,
            }],
            true,
            &binary,
            &home,
        );
        assert_eq!(report.status, SetupStatus::AlreadyConfigured);

        let codex = serde_json::json!({
            "name":"riltra",
            "enabled":true,
            "disabled_reason":null,
            "enabled_tools":null,
            "disabled_tools":null,
            "transport":{
                "type":"stdio",
                "command":bin,
                "args":["mcp", "--all-tools"],
                "env":{"RILTRA_HOME":home_text},
                "env_vars":[],
                "cwd":null
            },
            "startup_timeout_sec":null,
            "tool_timeout_sec":null
        })
        .to_string();
        let report = codex_report_from_get("codex", &binary, &home, Some(0), codex.as_bytes());
        assert_eq!(report.status, SetupStatus::AlreadyConfigured);
    }

    #[test]
    fn codex_get_transport_shape_matches_only_when_behavior_fields_match() {
        let root = crate::testutil::tmpdir("agent-setup-codex-transport");
        let binary = root.join("riltra.exe");
        let home = root.join("home");
        fs::write(&binary, b"test executable").unwrap();
        let output = serde_json::json!({
            "name":"riltra",
            "enabled":true,
            "disabled_reason":null,
            "enabled_tools":null,
            "disabled_tools":null,
            "transport":{
                "type":"stdio",
                "command":binary.display().to_string(),
                "args":["mcp"],
                "env":{"RILTRA_HOME":home.display().to_string()},
                "env_vars":[],
                "cwd":null
            },
            "startup_timeout_sec":null,
            "tool_timeout_sec":null
        })
        .to_string();
        let report = codex_report_from_get("codex", &binary, &home, Some(0), output.as_bytes());
        assert_eq!(report.status, SetupStatus::AlreadyConfigured);
        assert_eq!(report.configuration, ConfigurationStatus::Found);

        for field in ["enabled_tools", "disabled_tools"] {
            for tools in [
                serde_json::json!(["restricted-tool"]),
                serde_json::json!([]),
            ] {
                let mut restricted: serde_json::Value = serde_json::from_str(&output).unwrap();
                restricted[field] = tools;
                let report = codex_report_from_get(
                    "codex",
                    &binary,
                    &home,
                    Some(0),
                    restricted.to_string().as_bytes(),
                );
                assert_eq!(report.status, SetupStatus::ManualRequired, "{field}");
                assert_eq!(
                    report.configuration,
                    ConfigurationStatus::Conflict,
                    "{field}"
                );
            }
        }

        for field in ["enabled_tools", "disabled_tools"] {
            let mut missing_tools: serde_json::Value = serde_json::from_str(&output).unwrap();
            missing_tools.as_object_mut().unwrap().remove(field);
            let report = codex_report_from_get(
                "codex",
                &binary,
                &home,
                Some(0),
                missing_tools.to_string().as_bytes(),
            );
            assert_eq!(report.status, SetupStatus::ManualRequired, "{field}");
            assert_eq!(
                report.configuration,
                ConfigurationStatus::Conflict,
                "{field}"
            );
        }

        let mut changed: serde_json::Value = serde_json::from_str(&output).unwrap();
        changed["transport"]["cwd"] = serde_json::json!("other-working-directory");
        let conflict = codex_report_from_get(
            "codex",
            &binary,
            &home,
            Some(0),
            changed.to_string().as_bytes(),
        );
        assert_eq!(conflict.status, SetupStatus::ManualRequired);
        assert_eq!(conflict.configuration, ConfigurationStatus::Conflict);
    }

    #[test]
    fn missing_binary_stays_manual_after_each_inspector_branch() {
        let root = crate::testutil::tmpdir("agent-setup-inspector-missing-binary");
        let binary = root.join("missing-riltra.exe");
        let home = root.join("home");
        let claude_path = root.join("claude.json");
        fs::write(
            &claude_path,
            serde_json::json!({"mcpServers":{"riltra":{
                "command":binary.display().to_string(), "args":["mcp"],
                "env":{"RILTRA_HOME":home.display().to_string()}
            }}})
            .to_string(),
        )
        .unwrap();
        let claude = inspect_claude(
            &[ClaudeConfigSource {
                config: ConfigSource {
                    path: claude_path,
                    scope: SetupScope::UserGlobal,
                },
                project_root: None,
            }],
            true,
            &binary,
            &home,
        );
        assert_eq!(claude.status, SetupStatus::ManualRequired);
        assert!(claude
            .reason
            .as_deref()
            .unwrap()
            .contains("executable is missing"));

        let opencode_path = root.join("opencode.json");
        fs::write(
            &opencode_path,
            serde_json::json!({"mcp":{"servers":{"riltra":{
                "type":"local", "command":[binary.display().to_string(),"mcp"],
                "environment":{"RILTRA_HOME":home.display().to_string()}
            }}}})
            .to_string(),
        )
        .unwrap();
        let opencode = inspect_opencode(
            &[ConfigSource {
                path: opencode_path,
                scope: SetupScope::UserGlobal,
            }],
            true,
            &binary,
            &home,
        );
        assert_eq!(opencode.status, SetupStatus::ManualRequired);
        assert!(opencode
            .reason
            .as_deref()
            .unwrap()
            .contains("executable is missing"));

        let codex_output = serde_json::json!({
            "name":"riltra", "enabled":true, "disabled_reason":null,
            "transport":{"type":"stdio", "command":binary.display().to_string(),
                "args":["mcp"], "env":{"RILTRA_HOME":home.display().to_string()},
                "env_vars":[], "cwd":null},
            "startup_timeout_sec":null, "tool_timeout_sec":null, "auth_status":"unsupported"
        })
        .to_string();
        let codex =
            codex_report_from_get("codex", &binary, &home, Some(0), codex_output.as_bytes());
        assert_eq!(codex.status, SetupStatus::ManualRequired);
        assert!(codex
            .reason
            .as_deref()
            .unwrap()
            .contains("executable is missing"));
    }
}
