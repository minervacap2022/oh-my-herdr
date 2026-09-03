use std::time::{Duration, Instant};

use bytes::Bytes;

use super::{terminal_targets::TerminalTargetError, App};
use crate::agent_registry::{AgentMd, EffortLevel};
use crate::agent_spawn::{plan_spawn, CwdMode, PaneAgentState, SpawnAction, TabAgentContext};
use crate::api::schema::{
    AgentProfileCreateParams, AgentProfileEffort, AgentProfileInfo, AgentProfileMd,
    AgentProfileSetParams, AgentSpawnParams, AgentStartParams, SpawnedAgentInfo,
};
use crate::layout::PaneId;

const DEFAULT_AGENT_START_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const MAX_AGENT_START_TIMEOUT: Duration = Duration::from_secs(300);
pub(crate) const AGENT_START_SETTLE_DELAY: Duration = Duration::from_secs(3);
const AGENT_SPAWN_SHELL_READINESS_RETRY_TIMEOUT: Duration = Duration::from_secs(2);
const AGENT_SPAWN_SHELL_READINESS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const AGENT_SPAWN_RECENT_INPUT_GUARD: Duration = Duration::from_millis(500);
const INVALID_AGENT_TIMEOUT_MESSAGE: &str =
    "agent start timeout must be greater than 3000ms and at most 300000ms";
const INVALID_AGENT_NAME_MESSAGE: &str = "agent name must start with a lowercase letter and contain only lowercase letters, digits, '-' or '_' (1-32 characters)";

fn valid_agent_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && name.len() <= 32
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_'))
}

fn agent_start_timeout(timeout_ms: Option<u64>) -> Result<Duration, AgentStartError> {
    let timeout =
        Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_AGENT_START_TIMEOUT.as_millis() as u64));
    if timeout <= AGENT_START_SETTLE_DELAY || timeout > MAX_AGENT_START_TIMEOUT {
        return Err(AgentStartError::InvalidTimeout);
    }
    Ok(timeout)
}

fn validate_agent_start_inputs(name: &str, args: &[String]) -> Result<(), AgentStartError> {
    if !valid_agent_name(name) {
        return Err(AgentStartError::InvalidName);
    }
    if args.iter().any(|arg| arg.chars().any(char::is_control)) {
        return Err(AgentStartError::InvalidArgument);
    }
    Ok(())
}

fn profile_md_args(kind: &str, mds: &[AgentMd]) -> Result<Vec<String>, AgentSpawnError> {
    if mds.is_empty() {
        return Ok(Vec::new());
    }
    if kind == "codex" {
        if mds.len() == 1 {
            let path = mds[0].path.display().to_string();
            let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
            return Ok(vec![
                "-c".to_string(),
                format!("model_instructions_file=\"{escaped}\""),
            ]);
        }
        let mut instructions = String::new();
        for md in mds {
            let content = std::fs::read_to_string(&md.path)
                .map_err(|_| AgentSpawnError::ProfileMdPathNotFile(md.path.clone()))?;
            if !instructions.is_empty() {
                instructions.push_str("\n\n");
            }
            instructions.push_str(&content);
        }
        return Ok(vec![
            "-c".to_string(),
            format!(
                "developer_instructions={}",
                toml::Value::String(instructions).to_string()
            ),
        ]);
    }

    if !matches!(kind, "claude" | "pi") {
        return Err(AgentSpawnError::ProfileMdUnsupported(kind.to_string()));
    }

    let mut args = Vec::with_capacity(mds.len() * 2);
    for md in mds {
        if !md.path.is_file() {
            return Err(AgentSpawnError::ProfileMdPathNotFile(md.path.clone()));
        }
        match kind {
            "claude" => {
                args.push("--append-system-prompt-file".to_string());
                args.push(md.path.display().to_string());
            }
            "pi" => {
                args.push("--append-system-prompt".to_string());
                args.push(md.path.display().to_string());
            }
            _ => {
                return Err(AgentSpawnError::ProfileMdUnsupported(kind.to_string()));
            }
        }
    }
    Ok(args)
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('A'..='Z' | 'a'..='z' | '_'))
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn profile_allowlist_tools(value: &serde_json::Value) -> Option<Vec<String>> {
    let tools = value.get("tools")?.as_array()?;
    let tools = tools
        .iter()
        .map(|tool| tool.as_str().map(ToOwned::to_owned))
        .collect::<Option<Vec<_>>>()?;
    (!tools.is_empty()
        && tools
            .iter()
            .all(|tool| !tool.is_empty() && !tool.chars().any(char::is_control)))
    .then_some(tools)
}

fn profile_launch_settings(
    kind: &str,
    profile: &crate::agent_registry::AgentProfile,
) -> Result<(Vec<String>, Option<(String, String)>), AgentSpawnError> {
    let mut args = Vec::new();
    if let Some(model) = &profile.model {
        if !matches!(kind, "claude" | "codex" | "pi") {
            return Err(AgentSpawnError::ProfileSettingUnsupported(
                "model".into(),
                kind.into(),
            ));
        }
        args.extend(["--model".to_string(), model.clone()]);
    }
    if let Some(effort) = profile.effort {
        let level = match effort {
            EffortLevel::Low => "low",
            EffortLevel::Medium => "medium",
            EffortLevel::High => "high",
        };
        match kind {
            "claude" => args.extend(["--effort".to_string(), level.to_string()]),
            "codex" => args.extend([
                "-c".to_string(),
                format!("model_reasoning_effort=\"{level}\""),
            ]),
            "pi" => {
                let Some(model) = profile.model.as_ref() else {
                    return Err(AgentSpawnError::ProfileSettingUnsupported(
                        "effort requires a model".into(),
                        kind.into(),
                    ));
                };
                args.clear();
                args.extend(["--model".to_string(), format!("{model}:{level}")]);
            }
            _ => {
                return Err(AgentSpawnError::ProfileSettingUnsupported(
                    "effort".into(),
                    kind.into(),
                ))
            }
        }
    }
    if let Some(allowlist) = &profile.allowlist {
        let tools = profile_allowlist_tools(allowlist).ok_or(AgentSpawnError::InvalidAllowlist)?;
        match kind {
            "claude" => {
                for tool in tools {
                    args.extend(["--allowedTools".to_string(), tool]);
                }
            }
            "pi" => args.extend(["--tools".to_string(), tools.join(",")]),
            _ => {
                return Err(AgentSpawnError::ProfileSettingUnsupported(
                    "allowlist".into(),
                    kind.into(),
                ))
            }
        }
    }
    let env_binding = profile
        .apikey_ref
        .as_deref()
        .map(|reference| {
            let source = reference
                .strip_prefix("env:")
                .ok_or(AgentSpawnError::InvalidApiKeyRef)?;
            if !valid_env_name(source) || std::env::var_os(source).is_none() {
                return Err(AgentSpawnError::InvalidApiKeyRef);
            }
            let target = match kind {
                "claude" => "ANTHROPIC_API_KEY",
                "codex" => "OPENAI_API_KEY",
                _ => {
                    return Err(AgentSpawnError::ProfileSettingUnsupported(
                        "apikey_ref".into(),
                        kind.into(),
                    ))
                }
            };
            Ok((target.to_string(), source.to_string()))
        })
        .transpose()?;
    Ok((args, env_binding))
}

fn effort_to_schema(effort: EffortLevel) -> AgentProfileEffort {
    match effort {
        EffortLevel::Low => AgentProfileEffort::Low,
        EffortLevel::Medium => AgentProfileEffort::Medium,
        EffortLevel::High => AgentProfileEffort::High,
    }
}

fn effort_from_schema(effort: AgentProfileEffort) -> EffortLevel {
    match effort {
        AgentProfileEffort::Low => EffortLevel::Low,
        AgentProfileEffort::Medium => EffortLevel::Medium,
        AgentProfileEffort::High => EffortLevel::High,
    }
}

fn profile_info(profile: &crate::agent_registry::AgentProfile) -> AgentProfileInfo {
    AgentProfileInfo {
        role: profile.role.clone(),
        native_cwd: profile.native_cwd.display().to_string(),
        native_cwd_seeded: profile.native_cwd_seeded,
        mds: profile
            .mds
            .iter()
            .map(|md| AgentProfileMd {
                name: md.name.clone(),
                path: md.path.display().to_string(),
            })
            .collect(),
        harness: profile.harness.clone(),
        model: profile.model.clone(),
        effort: profile.effort.map(effort_to_schema),
        apikey_ref: profile.apikey_ref.clone(),
        allowlist: profile.allowlist.clone(),
        replicas_assigned: profile.replicas_assigned,
        created_at: profile.created_at,
        last_spawned_at: profile.last_spawned_at,
    }
}

fn validate_profile_md_canonical_path(
    path: std::path::PathBuf,
) -> Result<std::path::PathBuf, AgentProfileError> {
    if path.to_str().is_none() {
        return Err(AgentProfileError::PathNotUtf8);
    }
    Ok(path)
}

/// Errors from editing a saved agent profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AgentProfileError {
    /// The role did not match the valid agent-name format.
    InvalidRole,
    /// The md name was empty.
    InvalidName,
    /// The supplied md path did not exist on disk.
    PathNotFound(String),
    /// The supplied md path exists but is not a regular file.
    PathNotFile(String),
    /// The canonical md path cannot be represented in the JSON API or shell command.
    PathNotUtf8,
    /// Profile cwd values must be representable in the JSON API and shell command.
    NativeCwdNotUtf8,
    NotFound(String),
    AlreadyExists(String),
    InvalidInstructions,
    InvalidPatch,
    InvalidHarness(String),
    InvalidModel,
    InvalidAllowlist,
    InvalidApiKeyRef,
    NativeCwdNotFound(String),
    NativeCwdNotDirectory(String),
    LoadFailed(String),
    PersistFailed(String),
}

pub(crate) struct DeferredAgentSpawn {
    pub(crate) id: String,
    pub(crate) respond_to: std::sync::mpsc::Sender<String>,
    pub(crate) pending: PendingAgentSpawn,
    pub(crate) revived: bool,
}

pub(crate) struct DeferredAgentSpawnCompletion {
    pub(crate) id: String,
    pub(crate) respond_to: std::sync::mpsc::Sender<String>,
    pub(crate) result: Result<SpawnedAgentInfo, AgentSpawnError>,
    pub(crate) revived: bool,
}

pub(crate) enum AgentSpawnOutcome {
    Spawned(Box<SpawnedAgentInfo>),
    Pending(Box<PendingAgentSpawn>),
}

pub(crate) struct PendingAgentSpawn {
    completion: AgentSpawnCompletion,
    terminal_id: crate::terminal::TerminalId,
    cwd: std::path::PathBuf,
    start_params: AgentStartParams,
    deadline: Instant,
    next_retry_at: Instant,
    env_binding: Option<(String, String)>,
}

impl PendingAgentSpawn {
    pub(crate) fn pane_id(&self) -> &str {
        &self.completion.public_pane_id
    }
}

struct AgentSpawnCompletion {
    ws_idx: usize,
    target_pane_id: PaneId,
    split_tab_idx: Option<usize>,
    split: bool,
    role: String,
    tab_cwd: std::path::PathBuf,
    replica_index: Option<u32>,
    agent_name: String,
    instance_id: String,
    revived: bool,
    public_pane_id: String,
}

struct AgentSpawnReservation {
    agent_name: String,
    instance_id: String,
    replica_index: Option<u32>,
    revived: bool,
    profile: crate::agent_registry::AgentProfile,
}

struct AgentStartPreparation {
    name: String,
    kind: crate::detect::Agent,
    ws_idx: usize,
    pane_id: PaneId,
    terminal_id: crate::terminal::TerminalId,
    target: String,
    argv: Vec<String>,
    bytes: Bytes,
    timeout: Duration,
}

impl App {
    pub(crate) fn terminate_live_roster_instance(
        &mut self,
        roster_instance_id: Option<&str>,
        legacy_agent_name: Option<&str>,
    ) -> bool {
        if let Some(instance_id) = roster_instance_id {
            return match self
                .update_agent_registry(|registry| registry.roster_terminate_instance(instance_id))
            {
                Ok(terminated) => terminated,
                Err(err) => {
                    tracing::warn!(err = %err, instance_id, "failed to persist terminated agent roster entry");
                    false
                }
            };
        }

        let Some(agent_name) =
            legacy_agent_name.filter(|name| self.agent_registry.alive_instance(name).is_some())
        else {
            return false;
        };
        match self.update_agent_registry(|registry| registry.roster_terminate(agent_name)) {
            Ok(terminated) => terminated,
            Err(err) => {
                tracing::warn!(err = %err, agent_name, "failed to persist terminated agent roster entry");
                false
            }
        }
    }

    pub(super) fn collect_agent_infos(&self) -> Vec<crate::api::schema::AgentInfo> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, ws)| {
                ws.tabs.iter().flat_map(move |tab| {
                    tab.layout
                        .pane_ids()
                        .into_iter()
                        .filter_map(move |pane_id| self.agent_info(ws_idx, pane_id))
                })
            })
            .collect()
    }

    pub(super) fn reconcile_managed_agent_target(&mut self, target: &str) {
        if self.resolve_agent_target(target).is_err() {
            return;
        }
        let updates = self.state.reconcile_managed_agents_at(Instant::now());
        if !updates.is_empty() {
            self.schedule_session_save();
            for update in updates {
                self.emit_pane_updated(update.ws_idx, update.pane_id);
                self.emit_pane_state_update(&update);
            }
        }
    }

    pub(super) fn agent_info_for_target(
        &self,
        target: &str,
    ) -> Result<crate::api::schema::AgentInfo, TerminalTargetError> {
        let resolved = self.resolve_agent_target(target)?;
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| TerminalTargetError::NotFound {
                target: target.to_string(),
            })
    }

    pub(super) fn focus_agent_target(
        &mut self,
        target: &str,
    ) -> Result<crate::api::schema::AgentInfo, TerminalTargetError> {
        let resolved = self.resolve_agent_target(target)?;
        self.state
            .focus_pane_in_workspace(resolved.ws_idx, resolved.pane_id);
        self.state.mark_active_tab_seen();
        self.state.settle_terminal_mode_after_focus();
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| TerminalTargetError::NotFound {
                target: target.to_string(),
            })
    }

    pub(super) fn rename_agent_target(
        &mut self,
        target: &str,
        name: Option<String>,
    ) -> Result<crate::api::schema::AgentInfo, AgentRenameError> {
        let resolved = self
            .resolve_agent_target(target)
            .map_err(AgentRenameError::Target)?;
        let normalized_name = match name {
            Some(name) if valid_agent_name(&name) => Some(name),
            Some(_) => return Err(AgentRenameError::InvalidName),
            None => None,
        };

        if let Some(name) = normalized_name.as_deref() {
            let conflicts = self.agent_name_conflicts(name, &resolved.terminal_id);
            if !conflicts.is_empty() {
                return Err(AgentRenameError::DuplicateName {
                    name: name.to_string(),
                    candidates: conflicts,
                });
            }
        }

        let Some(terminal) = self
            .state
            .terminals
            .values_mut()
            .find(|terminal| terminal.id.to_string() == resolved.terminal_id)
        else {
            return Err(AgentRenameError::Target(TerminalTargetError::NotFound {
                target: target.to_string(),
            }));
        };
        if terminal.managed_agent_launch_pending() {
            return Err(AgentRenameError::PendingLaunch);
        }
        if terminal.effective_agent_label().is_none() {
            return Err(AgentRenameError::NotAgent);
        }
        match normalized_name {
            Some(name) => terminal.set_agent_name(name),
            None => terminal.clear_agent_name(),
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        self.emit_pane_updated(resolved.ws_idx, resolved.pane_id);
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| {
                AgentRenameError::Target(TerminalTargetError::NotFound {
                    target: target.to_string(),
                })
            })
    }

    pub(super) fn start_agent(
        &mut self,
        params: AgentStartParams,
    ) -> Result<(crate::api::schema::AgentInfo, Vec<String>), AgentStartError> {
        let preparation = self.preflight_agent_start(&params, None, None)?;
        let instance_id = self.reserve_direct_agent_start(&preparation.name)?;
        let pane_id = params.pane_id;
        match self.submit_agent_start(preparation, Some(&instance_id)) {
            Ok((agent, argv)) => {
                self.activate_agent_roster_reservation(&instance_id, pane_id);
                Ok((agent, argv))
            }
            Err(err) => {
                self.release_agent_spawn_reservation(&instance_id, false);
                Err(err)
            }
        }
    }

    fn start_agent_with_cwd(
        &mut self,
        params: AgentStartParams,
        cwd: Option<&std::path::Path>,
        roster_instance_id: Option<&str>,
        env_binding: Option<(String, String)>,
    ) -> Result<(crate::api::schema::AgentInfo, Vec<String>), AgentStartError> {
        let preparation = self.preflight_agent_start(&params, cwd, env_binding)?;
        self.submit_agent_start(preparation, roster_instance_id)
    }

    fn preflight_agent_start(
        &self,
        params: &AgentStartParams,
        cwd: Option<&std::path::Path>,
        env_binding: Option<(String, String)>,
    ) -> Result<AgentStartPreparation, AgentStartError> {
        let name = params.name.clone();
        let Some(kind) = crate::detect::parse_agent_label(&params.kind) else {
            return Err(AgentStartError::UnsupportedKind(params.kind.clone()));
        };
        validate_agent_start_inputs(&name, &params.args)?;
        if cwd.is_some_and(|cwd| cwd.to_str().is_none()) {
            return Err(AgentStartError::CwdNotUtf8);
        }
        let conflicts = self.agent_name_conflicts(&name, "");
        if !conflicts.is_empty() {
            return Err(AgentStartError::DuplicateName {
                name,
                candidates: conflicts,
            });
        }
        let Some((ws_idx, pane_id)) = self.parse_current_public_pane_id(&params.pane_id) else {
            return Err(AgentStartError::TargetNotFound(params.pane_id.clone()));
        };
        let terminal_id = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id))
            .cloned()
            .ok_or_else(|| AgentStartError::TargetNotFound(params.pane_id.clone()))?;
        let terminal = self
            .state
            .terminals
            .get(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetNotFound(params.pane_id.clone()))?;
        if terminal.is_agent_terminal() || terminal.managed_agent_kind().is_some() {
            return Err(AgentStartError::TargetBusy(params.pane_id.clone()));
        }
        let runtime = self
            .terminal_runtimes
            .get(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetUnavailable(params.pane_id.clone()))?;
        let shell_name = available_shell_name(runtime)
            .ok_or_else(|| AgentStartError::TargetBusy(params.pane_id.clone()))?;

        let mut argv = vec![crate::detect::interactive_agent_executable(kind).to_string()];
        argv.extend(params.args.clone());
        let command = match (cwd, env_binding.as_ref()) {
            (Some(cwd), Some((target, source))) => {
                crate::platform::interactive_shell_command_with_env_in_cwd(
                    &argv,
                    &shell_name,
                    cwd,
                    target,
                    source,
                )
            }
            (Some(cwd), None) => {
                crate::platform::interactive_shell_command_in_cwd(&argv, &shell_name, cwd)
            }
            (None, _) => crate::platform::interactive_shell_command(&argv, &shell_name),
        }
        .ok_or(AgentStartError::InvalidArgument)?;
        let bytes = crate::app::api_helpers::encode_api_submission(runtime, &command);
        let timeout = agent_start_timeout(params.timeout_ms)?;

        Ok(AgentStartPreparation {
            name,
            kind,
            ws_idx,
            pane_id,
            terminal_id,
            target: params.pane_id.clone(),
            argv,
            bytes: Bytes::from(bytes),
            timeout,
        })
    }

    fn submit_agent_start(
        &mut self,
        preparation: AgentStartPreparation,
        roster_instance_id: Option<&str>,
    ) -> Result<(crate::api::schema::AgentInfo, Vec<String>), AgentStartError> {
        let runtime = self
            .terminal_runtimes
            .get(&preparation.terminal_id)
            .ok_or_else(|| AgentStartError::TargetUnavailable(preparation.target.clone()))?;
        let now = Instant::now();
        let terminal = self
            .state
            .terminals
            .get_mut(&preparation.terminal_id)
            .ok_or_else(|| AgentStartError::TargetUnavailable(preparation.target.clone()))?;
        terminal.begin_managed_agent(
            preparation.name,
            preparation.kind,
            roster_instance_id.map(ToOwned::to_owned),
            now,
            AGENT_START_SETTLE_DELAY,
            preparation.timeout,
        );
        if let Err(err) = runtime.try_send_bytes(preparation.bytes) {
            terminal.clear_agent_name();
            return Err(AgentStartError::InputFailed(err.to_string()));
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();

        let agent = self
            .agent_info(preparation.ws_idx, preparation.pane_id)
            .ok_or(AgentStartError::TargetUnavailable(preparation.target))?;
        Ok((agent, preparation.argv))
    }

    /// Spawn an interactive agent using the owlspace-style spawn model: pick the
    /// target workspace/tab, reuse an available shell or auto-split a new pane,
    /// resolve the cwd, start the agent, and register it as a persistent
    /// profile/roster entry. This is the runtime counterpart to the pure
    /// [`plan_spawn`] planner.
    pub(super) fn spawn_agent(
        &mut self,
        params: AgentSpawnParams,
    ) -> Result<AgentSpawnOutcome, AgentSpawnError> {
        self.spawn_agent_with_revival(params, None)
    }

    pub(super) fn revive_agent(
        &mut self,
        instance_id: String,
        tab_id: Option<String>,
        cwd_mode: String,
        timeout_ms: Option<u64>,
        args: Vec<String>,
    ) -> Result<AgentSpawnOutcome, AgentSpawnError> {
        self.refresh_agent_registry_for_read().map_err(|err| {
            AgentSpawnError::SpawnFailed(format!("failed to load agent registry: {err}"))
        })?;
        let Some(entry) = self.agent_registry.roster.get(&instance_id) else {
            return Err(AgentSpawnError::SpawnFailed(format!(
                "agent roster instance {instance_id} not found"
            )));
        };
        if entry.status != crate::agent_registry::AgentStatus::Terminated {
            return Err(AgentSpawnError::SpawnFailed(format!(
                "agent roster instance {instance_id} is already active"
            )));
        }
        self.spawn_agent_with_revival(
            AgentSpawnParams {
                role: entry.role.clone(),
                kind: None,
                tab_id,
                cwd_mode,
                timeout_ms,
                args,
            },
            Some(&instance_id),
        )
    }

    fn spawn_agent_with_revival(
        &mut self,
        params: AgentSpawnParams,
        revive_instance_id: Option<&str>,
    ) -> Result<AgentSpawnOutcome, AgentSpawnError> {
        let cwd_mode = match params.cwd_mode.as_str() {
            "tab" => CwdMode::Tab,
            "agent" => CwdMode::Agent,
            _ => return Err(AgentSpawnError::InvalidCwdMode),
        };

        let (ws_idx, tab_idx) = match params.tab_id.as_deref() {
            Some(tab_id) => self
                .parse_tab_id(tab_id)
                .ok_or_else(|| AgentSpawnError::TabNotFound(tab_id.to_string()))?,
            None => {
                let ws_idx = self
                    .state
                    .active
                    .ok_or(AgentSpawnError::NoActiveWorkspace)?;
                let tab_idx = self
                    .state
                    .workspaces
                    .get(ws_idx)
                    .map(|ws| ws.active_tab)
                    .ok_or(AgentSpawnError::NoActiveWorkspace)?;
                (ws_idx, tab_idx)
            }
        };

        // The tab's root pane drives the cwd fallback (tab-cwd mode + profile
        // seeding). Computed once and reused.
        let root_pane = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.tabs.get(tab_idx))
            .map(|tab| tab.root_pane)
            .unwrap_or_else(PaneId::alloc);

        // Snapshot the tab's live pane state (agent vs. available shell).
        let context = self.agent_spawn_context(ws_idx, tab_idx);

        let tab_cwd = self
            .launch_cwd_for_pane_in_workspace(ws_idx, root_pane)
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        if matches!(cwd_mode, CwdMode::Tab) && tab_cwd.to_str().is_none() {
            return Err(AgentSpawnError::CwdNotUtf8);
        }
        validate_agent_start_inputs(&params.role, &params.args)
            .map_err(AgentSpawnError::InvalidStartInput)?;
        agent_start_timeout(params.timeout_ms).map_err(AgentSpawnError::InvalidStartInput)?;
        self.refresh_agent_registry_for_read().map_err(|err| {
            AgentSpawnError::SpawnFailed(format!("failed to load agent registry: {err}"))
        })?;
        let preflight_profile = self.agent_registry.get(&params.role).cloned();
        let preflight_kind = params.kind.as_deref().unwrap_or_else(|| {
            preflight_profile
                .as_ref()
                .map(|profile| profile.harness.as_str())
                .unwrap_or("codex")
        });
        let Some(preflight_kind) = crate::detect::parse_agent_label(preflight_kind) else {
            return Err(AgentSpawnError::InvalidKind);
        };
        if !self.no_session
            && preflight_profile.is_none()
            && matches!(
                crate::detect::agent_label(preflight_kind),
                "codex" | "pi" | "claude"
            )
        {
            let harness = crate::detect::agent_label(preflight_kind).to_string();
            self.create_profile(AgentProfileCreateParams {
                role: params.role.clone(),
                harness,
                native_cwd: tab_cwd.display().to_string(),
                instructions: None,
            })
            .map_err(|err| {
                AgentSpawnError::SpawnFailed(self.agent_profile_error_body(err).message)
            })?;
            self.refresh_agent_registry_for_read().map_err(|err| {
                AgentSpawnError::SpawnFailed(format!("failed to load agent registry: {err}"))
            })?;
        }
        if let Some(profile) = &preflight_profile {
            profile_md_args(crate::detect::agent_label(preflight_kind), &profile.mds)?;
            profile_launch_settings(crate::detect::agent_label(preflight_kind), profile)?;
        }
        let local_agent_names = self
            .collect_agent_infos()
            .into_iter()
            .filter_map(|agent| agent.name)
            .chain(
                self.pending_agent_spawns
                    .iter()
                    .map(|spawn| spawn.pending.completion.agent_name.clone()),
            )
            .collect::<Vec<_>>();
        let local_role_is_live = local_agent_names.iter().any(|name| {
            name == &params.role
                || self
                    .agent_registry
                    .alive_instance(name)
                    .is_some_and(|entry| entry.role == params.role)
        });
        let reservation = match revive_instance_id {
            Some(instance_id) => {
                self.reserve_agent_revival(instance_id, &params.role, local_agent_names)?
            }
            None => self.reserve_agent_spawn(
                &params.role,
                tab_cwd.clone(),
                local_role_is_live,
                local_agent_names,
            )?,
        };
        let requested_kind = params
            .kind
            .as_deref()
            .unwrap_or(reservation.profile.harness.as_str());
        let Some(kind) = crate::detect::parse_agent_label(requested_kind) else {
            self.release_agent_spawn_reservation(&reservation.instance_id, reservation.revived);
            return Err(AgentSpawnError::InvalidKind);
        };
        let kind = crate::detect::agent_label(kind).to_string();
        let native_cwd = if reservation.profile.native_cwd_seeded {
            reservation.profile.native_cwd.clone()
        } else {
            tab_cwd.clone()
        };
        let spawn_cwd = match cwd_mode {
            CwdMode::Tab => &tab_cwd,
            CwdMode::Agent => &native_cwd,
        };
        if spawn_cwd.to_str().is_none() {
            self.release_agent_spawn_reservation(&reservation.instance_id, reservation.revived);
            return Err(AgentSpawnError::CwdNotUtf8);
        }
        let mut args = match profile_md_args(&kind, &reservation.profile.mds) {
            Ok(args) => args,
            Err(err) => {
                self.release_agent_spawn_reservation(&reservation.instance_id, reservation.revived);
                return Err(err);
            }
        };
        args.extend(params.args);
        let (profile_args, env_binding) = match profile_launch_settings(&kind, &reservation.profile)
        {
            Ok(settings) => settings,
            Err(err) => {
                self.release_agent_spawn_reservation(&reservation.instance_id, reservation.revived);
                return Err(err);
            }
        };
        args.extend(profile_args);
        if let Err(err) = validate_agent_start_inputs(&params.role, &args) {
            self.release_agent_spawn_reservation(&reservation.instance_id, reservation.revived);
            return Err(AgentSpawnError::InvalidStartInput(err));
        }
        let plan = plan_spawn(
            &context,
            &kind,
            &reservation.agent_name,
            cwd_mode,
            tab_cwd.clone(),
            native_cwd,
        );
        let target_is_starting_shell = match &plan.action {
            SpawnAction::UseExisting { pane_id } => context
                .panes
                .iter()
                .find(|pane| pane.pane_id == *pane_id)
                .is_some_and(|pane| pane.is_shell_starting),
            SpawnAction::Split { .. } => false,
        };
        if !plan.cwd.is_dir() {
            self.release_agent_spawn_reservation(&reservation.instance_id, reservation.revived);
            return Err(AgentSpawnError::CwdNotFound(plan.cwd));
        }
        let AgentSpawnReservation {
            agent_name,
            instance_id,
            replica_index,
            revived,
            profile: _,
        } = reservation;

        // Execute the plan: reuse an available shell or auto-split a new pane.
        let target_pane_id: PaneId;
        let mut split_made = false;
        let mut split_tab_idx = None;
        let mut split_terminal_id = None;
        match plan.action.clone() {
            SpawnAction::UseExisting { pane_id } => {
                target_pane_id = pane_id;
            }
            SpawnAction::Split {
                from_pane,
                direction,
            } => {
                let (rows, cols) = self.state.estimate_pane_size();
                let shell_config = crate::pane::PaneShellConfig::new(
                    &self.state.default_shell,
                    self.state.shell_mode,
                );
                let split_result = {
                    let ws = &mut self.state.workspaces[ws_idx];
                    ws.split_pane(
                        from_pane,
                        direction,
                        rows,
                        cols,
                        Some(plan.cwd.clone()),
                        self.state.pane_scrollback_limit_bytes,
                        self.state.host_terminal_theme,
                        self.state.host_terminal_appearance,
                        shell_config,
                        Vec::new(),
                        false,
                    )
                };
                let (new_tab_idx, new_pane) = match split_result {
                    Some(Ok(result)) => result,
                    Some(Err(err)) => {
                        self.release_agent_spawn_reservation(&instance_id, revived);
                        return Err(AgentSpawnError::SplitFailed(err.to_string()));
                    }
                    None => {
                        self.release_agent_spawn_reservation(&instance_id, revived);
                        return Err(AgentSpawnError::SplitFailed(
                            "split target pane not found".to_string(),
                        ));
                    }
                };
                target_pane_id = new_pane.pane_id;
                self.terminal_runtimes
                    .insert(new_pane.terminal.id.clone(), new_pane.runtime);
                self.state
                    .remove_alias_shadowed_by_new_pane(new_pane.pane_id);
                let terminal_id = new_pane.terminal.id.clone();
                self.state
                    .terminals
                    .insert(terminal_id.clone(), new_pane.terminal);
                split_terminal_id = Some(terminal_id);
                split_tab_idx = Some(new_tab_idx);
                split_made = true;
            }
        }

        let public_pane_id = match self.public_pane_id(ws_idx, target_pane_id) {
            Some(pane_id) => pane_id,
            None => {
                if let Some(terminal_id) = split_terminal_id.as_ref() {
                    self.rollback_spawn_split(ws_idx, target_pane_id, terminal_id);
                }
                self.release_agent_spawn_reservation(&instance_id, revived);
                return Err(AgentSpawnError::SplitFailed(
                    "pane id not found".to_string(),
                ));
            }
        };

        let start_params = AgentStartParams {
            name: agent_name.clone(),
            kind: kind.clone(),
            pane_id: public_pane_id.clone(),
            args,
            timeout_ms: params.timeout_ms,
        };
        let completion = AgentSpawnCompletion {
            ws_idx,
            target_pane_id,
            split_tab_idx,
            split: split_made,
            role: params.role,
            tab_cwd,
            replica_index,
            agent_name,
            instance_id,
            revived,
            public_pane_id,
        };
        let pending_terminal_id = split_terminal_id.clone().or_else(|| {
            if target_is_starting_shell {
                self.state.terminal_id_for_pane(ws_idx, target_pane_id)
            } else {
                None
            }
        });
        match self.start_agent_with_cwd(
            start_params.clone(),
            Some(&plan.cwd),
            Some(&completion.instance_id),
            env_binding.clone(),
        ) {
            Ok((agent, argv)) => Ok(AgentSpawnOutcome::Spawned(Box::new(
                self.complete_agent_spawn(completion, agent, argv),
            ))),
            Err(AgentStartError::TargetBusy(_)) => {
                if let Some(terminal_id) = pending_terminal_id {
                    let now = Instant::now();
                    Ok(AgentSpawnOutcome::Pending(Box::new(PendingAgentSpawn {
                        completion,
                        terminal_id,
                        cwd: plan.cwd,
                        start_params,
                        deadline: now + AGENT_SPAWN_SHELL_READINESS_RETRY_TIMEOUT,
                        next_retry_at: now + AGENT_SPAWN_SHELL_READINESS_POLL_INTERVAL,
                        env_binding,
                    })))
                } else {
                    self.release_agent_spawn_reservation(
                        &completion.instance_id,
                        completion.revived,
                    );
                    Err(AgentSpawnError::SpawnFailed(format!(
                        "agent pane {} busy",
                        completion.public_pane_id
                    )))
                }
            }
            Err(err) => {
                if let Some(terminal_id) = split_terminal_id.as_ref() {
                    self.rollback_spawn_split(ws_idx, target_pane_id, terminal_id);
                }
                self.release_agent_spawn_reservation(&completion.instance_id, completion.revived);
                Err(AgentSpawnError::SpawnFailed(self.agent_start_message(err)))
            }
        }
    }

    fn reserve_agent_spawn(
        &mut self,
        role: &str,
        tab_cwd: std::path::PathBuf,
        local_role_is_live: bool,
        unavailable_names: Vec<String>,
    ) -> Result<AgentSpawnReservation, AgentSpawnError> {
        let role = role.to_string();
        self.update_agent_registry(move |registry| {
            let role_is_live = local_role_is_live || registry.is_role_alive(&role);
            let (agent_name, replica_suffix, replica_index) = if role_is_live {
                let index = registry
                    .next_available_replica_index(&role, &unavailable_names)
                    .ok_or(AgentSpawnError::ReplicaLimit)?;
                let agent_name = crate::agent_registry::format_replica_name(&role, index);
                (agent_name, format!("-replica-{index}"), Some(index))
            } else {
                (role.clone(), String::new(), None)
            };
            let profile = {
                let profile = registry.register_or_get(role.clone(), tab_cwd);
                if let Some(index) = replica_index {
                    profile.record_replica_assignment(index);
                }
                profile.clone()
            };
            let instance_id = registry
                .roster_register(&agent_name, &role, &role, &replica_suffix, None)
                .map(|entry| entry.instance_id.clone())
                .ok_or_else(|| {
                    AgentSpawnError::SpawnFailed("failed to reserve agent roster entry".to_string())
                })?;
            Ok(AgentSpawnReservation {
                agent_name,
                instance_id,
                replica_index,
                revived: false,
                profile,
            })
        })
        .map_err(|err| {
            AgentSpawnError::SpawnFailed(format!("failed to reserve agent spawn: {err}"))
        })?
    }

    fn reserve_agent_revival(
        &mut self,
        instance_id: &str,
        role: &str,
        unavailable_names: Vec<String>,
    ) -> Result<AgentSpawnReservation, AgentSpawnError> {
        let instance_id = instance_id.to_string();
        let role = role.to_string();
        self.update_agent_registry(move |registry| {
            let entry = registry
                .roster
                .get(&instance_id)
                .filter(|entry| entry.status == crate::agent_registry::AgentStatus::Terminated)
                .cloned()
                .ok_or_else(|| {
                    AgentSpawnError::SpawnFailed("archived agent is not revivable".into())
                })?;
            if entry.role != role
                || unavailable_names
                    .iter()
                    .any(|name| name == &entry.display_name)
                || registry.alive_instance(&entry.display_name).is_some()
            {
                return Err(AgentSpawnError::SpawnFailed(
                    "archived agent identity is already in use".into(),
                ));
            }
            let profile = registry.get(&role).cloned().ok_or_else(|| {
                AgentSpawnError::SpawnFailed("saved agent profile not found".into())
            })?;
            if registry.roster_reserve_revival(&instance_id).is_none() {
                return Err(AgentSpawnError::SpawnFailed(
                    "archived agent is not revivable".into(),
                ));
            }
            let replica_index = entry
                .replica_suffix
                .strip_prefix("-replica-")
                .and_then(|suffix| suffix.parse::<u32>().ok());
            Ok(AgentSpawnReservation {
                agent_name: entry.display_name,
                instance_id,
                replica_index,
                revived: true,
                profile,
            })
        })
        .map_err(|err| {
            AgentSpawnError::SpawnFailed(format!("failed to reserve agent revival: {err}"))
        })?
    }

    fn reserve_direct_agent_start(&mut self, name: &str) -> Result<String, AgentStartError> {
        let name = name.to_string();
        let instance_id = self
            .update_agent_registry(|registry| {
                if registry.alive_instance(&name).is_some() {
                    return None;
                }
                registry
                    .roster_register(&name, &name, &name, "", None)
                    .map(|entry| entry.instance_id.clone())
            })
            .map_err(|err| AgentStartError::RegistryFailed(err.to_string()))?;
        instance_id.ok_or_else(|| AgentStartError::DuplicateName {
            name,
            candidates: Vec::new(),
        })
    }

    fn activate_agent_roster_reservation(&mut self, instance_id: &str, pane_id: String) {
        match self.update_agent_registry(|registry| {
            registry.roster_activate_instance(instance_id, pane_id)
        }) {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(
                    instance_id,
                    "reserved agent roster entry was unavailable during start completion"
                );
            }
            Err(err) => {
                tracing::warn!(err = %err, instance_id, "failed to persist started agent roster entry");
            }
        }
    }

    fn release_agent_spawn_reservation(&mut self, instance_id: &str, revived: bool) {
        if let Err(err) = self.update_agent_registry(|registry| {
            if revived {
                registry.roster_cancel_revival(instance_id)
            } else {
                registry.roster_release_reservation(instance_id)
            }
        }) {
            tracing::warn!(err = %err, instance_id, "failed to release agent spawn reservation");
        }
    }

    fn complete_agent_spawn(
        &mut self,
        completion: AgentSpawnCompletion,
        agent: crate::api::schema::AgentInfo,
        argv: Vec<String>,
    ) -> SpawnedAgentInfo {
        if let Some(tab_idx) = completion.split_tab_idx {
            if let Some(pane) = self.pane_info(completion.ws_idx, completion.target_pane_id) {
                self.emit_event(crate::api::schema::EventEnvelope {
                    event: crate::api::schema::EventKind::PaneCreated,
                    data: crate::api::schema::EventData::PaneCreated { pane },
                });
            }
            self.emit_layout_updated_event(completion.ws_idx, tab_idx);
        }

        let role = completion.role.clone();
        let tab_cwd = completion.tab_cwd.clone();
        let replica_index = completion.replica_index;
        let agent_name = completion.agent_name.clone();
        let instance_id = completion.instance_id.clone();
        let pane_id = completion.public_pane_id.clone();
        if let Err(err) = self.update_agent_registry(move |registry| {
            let profile = registry.register_or_get(role.clone(), tab_cwd.clone());
            if let Some(index) = replica_index {
                profile.record_replica_assignment(index);
            }
            profile.record_spawn(tab_cwd);
            if !registry.roster_activate_instance(&instance_id, pane_id) {
                tracing::warn!(agent = %agent_name, instance_id, "reserved agent roster entry was unavailable during spawn completion");
            }
        }) {
            tracing::warn!(err = %err, agent = %completion.agent_name, "failed to persist spawned agent roster entry");
        }

        SpawnedAgentInfo {
            agent,
            argv,
            name: completion.agent_name,
            pane_id: completion.public_pane_id,
            split: completion.split,
        }
    }

    pub(super) fn sync_pending_agent_spawn_deadline(&mut self) {
        self.pending_agent_spawn_deadline = self
            .pending_agent_spawns
            .iter()
            .map(|spawn| spawn.pending.next_retry_at.min(spawn.pending.deadline))
            .min();
    }

    pub(super) fn poll_pending_agent_spawns(
        &mut self,
        now: Instant,
    ) -> Vec<DeferredAgentSpawnCompletion> {
        let pending_spawns = std::mem::take(&mut self.pending_agent_spawns);
        let mut retained = Vec::new();
        let mut completions = Vec::new();

        for deferred in pending_spawns {
            let DeferredAgentSpawn {
                id,
                respond_to,
                mut pending,
                revived,
            } = deferred;
            if now < pending.next_retry_at {
                retained.push(DeferredAgentSpawn {
                    id,
                    respond_to,
                    pending,
                    revived,
                });
                continue;
            }

            let start = self.start_agent_with_cwd(
                pending.start_params.clone(),
                Some(&pending.cwd),
                Some(&pending.completion.instance_id),
                pending.env_binding.clone(),
            );
            let result = match start {
                Ok((agent, argv)) => Ok(self.complete_agent_spawn(pending.completion, agent, argv)),
                Err(AgentStartError::TargetBusy(_)) if now < pending.deadline => {
                    pending.next_retry_at =
                        (now + AGENT_SPAWN_SHELL_READINESS_POLL_INTERVAL).min(pending.deadline);
                    retained.push(DeferredAgentSpawn {
                        id,
                        respond_to,
                        pending,
                        revived,
                    });
                    continue;
                }
                Err(err) => {
                    self.cancel_pending_agent_spawn(pending);
                    Err(AgentSpawnError::SpawnFailed(self.agent_start_message(err)))
                }
            };
            completions.push(DeferredAgentSpawnCompletion {
                id,
                respond_to,
                result,
                revived,
            });
        }

        self.pending_agent_spawns = retained;
        self.sync_pending_agent_spawn_deadline();
        completions
    }

    pub(super) fn cancel_pending_agent_spawn(&mut self, pending: PendingAgentSpawn) {
        self.release_agent_spawn_reservation(
            &pending.completion.instance_id,
            pending.completion.revived,
        );
        if pending.completion.split {
            self.rollback_spawn_split(
                pending.completion.ws_idx,
                pending.completion.target_pane_id,
                &pending.terminal_id,
            );
        }
    }

    fn rollback_spawn_split(
        &mut self,
        ws_idx: usize,
        pane_id: PaneId,
        terminal_id: &crate::terminal::TerminalId,
    ) {
        self.terminal_runtimes.remove(terminal_id);
        self.state.terminals.remove(terminal_id);
        if let Some(workspace) = self.state.workspaces.get_mut(ws_idx) {
            workspace.remove_pane(pane_id);
        }
    }

    /// Set (or clear) one injected `.md` on a saved agent profile and persist
    /// the registry. Mirrors `agent spawn <ROLE>`: the role names the persistent
    /// profile directly, so profile editing never depends on which instance or
    /// replica is currently running.
    ///
    /// `{ role, native_cwd, mds }` is the persistent context that survives
    /// harness/model/effort/apikey swaps; the `.md`s here are the always-injected
    /// subset. `path` is stored as a canonical absolute file path; an empty or
    /// omitted path removes the `.md`. The file must exist at set-time so a
    /// typo is caught before the replica that depends on it is spawned.
    pub(super) fn set_profile_md(
        &mut self,
        role: &str,
        name: &str,
        path: Option<&str>,
    ) -> Result<AgentProfileInfo, AgentProfileError> {
        if !valid_agent_name(role) {
            return Err(AgentProfileError::InvalidRole);
        }
        if name.trim().is_empty() {
            return Err(AgentProfileError::InvalidName);
        }

        let seed_cwd = self.profile_seed_cwd()?;

        let validated_path = match path.map(str::trim) {
            Some("") | None => None,
            Some(path_str) => {
                let file = std::path::Path::new(path_str);
                if !file.exists() {
                    return Err(AgentProfileError::PathNotFound(path_str.to_string()));
                }
                if !file.is_file() {
                    return Err(AgentProfileError::PathNotFile(path_str.to_string()));
                }
                let canonical = std::fs::canonicalize(file)
                    .map_err(|_| AgentProfileError::PathNotFound(path_str.to_string()))?;
                Some(validate_profile_md_canonical_path(canonical)?)
            }
        };

        let role = role.to_string();
        let name = name.to_string();
        self.update_agent_registry(move |registry| {
            let profile_is_new = registry.get(&role).is_none();
            let profile = registry.register_or_get(role, seed_cwd);
            if profile_is_new {
                profile.native_cwd_seeded = false;
            }
            match validated_path {
                None => {
                    profile.remove_md(&name);
                }
                Some(file) => profile.set_md(name, file),
            }
            profile_info(profile)
        })
        .map_err(|err| AgentProfileError::PersistFailed(err.to_string()))
    }

    pub(super) fn profile(&mut self, role: &str) -> Result<AgentProfileInfo, AgentProfileError> {
        if !valid_agent_name(role) {
            return Err(AgentProfileError::InvalidRole);
        }
        self.refresh_agent_registry_for_read()
            .map_err(|err| AgentProfileError::LoadFailed(err.to_string()))?;
        self.agent_registry
            .get(role)
            .map(profile_info)
            .ok_or_else(|| AgentProfileError::NotFound(role.to_string()))
    }

    pub(super) fn profiles(&mut self) -> Result<Vec<AgentProfileInfo>, AgentProfileError> {
        self.refresh_agent_registry_for_read()
            .map_err(|err| AgentProfileError::LoadFailed(err.to_string()))?;
        Ok(self
            .agent_registry
            .profiles
            .values()
            .map(profile_info)
            .collect())
    }

    pub(super) fn delete_profile(
        &mut self,
        role: &str,
    ) -> Result<AgentProfileInfo, AgentProfileError> {
        if !valid_agent_name(role) {
            return Err(AgentProfileError::InvalidRole);
        }
        self.refresh_agent_registry_for_read()
            .map_err(|err| AgentProfileError::LoadFailed(err.to_string()))?;
        if self.agent_registry.get(role).is_none() {
            return Err(AgentProfileError::NotFound(role.to_string()));
        }
        let role = role.to_string();
        let registry_role = role.clone();
        let removed = self
            .update_agent_registry(move |registry| registry.remove_profile(&registry_role))
            .map_err(|err| AgentProfileError::PersistFailed(err.to_string()))?
            .ok_or_else(|| AgentProfileError::NotFound(role.clone()))?;
        if !self.no_session {
            if let Err(err) = crate::agent_registry::remove_owned_instructions(&removed.role) {
                tracing::warn!(err = %err, role = %removed.role, "failed to remove deleted agent profile instructions");
            }
        }
        Ok(profile_info(&removed))
    }

    pub(super) fn create_profile(
        &mut self,
        params: AgentProfileCreateParams,
    ) -> Result<AgentProfileInfo, AgentProfileError> {
        if !valid_agent_name(&params.role) {
            return Err(AgentProfileError::InvalidRole);
        }
        if params
            .instructions
            .as_deref()
            .is_some_and(|text| text.contains('\0'))
        {
            return Err(AgentProfileError::InvalidInstructions);
        }
        let harness = crate::detect::parse_agent_label(&params.harness)
            .map(|agent| crate::detect::agent_label(agent).to_string())
            .filter(|harness| matches!(harness.as_str(), "codex" | "pi" | "claude"))
            .ok_or_else(|| AgentProfileError::InvalidHarness(params.harness.clone()))?;
        let cwd = std::path::PathBuf::from(&params.native_cwd);
        if !cwd.exists() {
            return Err(AgentProfileError::NativeCwdNotFound(params.native_cwd));
        }
        if !cwd.is_dir() {
            return Err(AgentProfileError::NativeCwdNotDirectory(params.native_cwd));
        }
        let cwd = std::fs::canonicalize(&cwd)
            .map_err(|_| AgentProfileError::NativeCwdNotFound(params.native_cwd.clone()))?;
        if cwd.to_str().is_none() {
            return Err(AgentProfileError::NativeCwdNotUtf8);
        }
        self.refresh_agent_registry_for_read()
            .map_err(|err| AgentProfileError::LoadFailed(err.to_string()))?;
        if self.agent_registry.get(&params.role).is_some() {
            return Err(AgentProfileError::AlreadyExists(params.role));
        }
        if self.no_session {
            return Err(AgentProfileError::PersistFailed(
                "agent profiles require a persistent Herdr session".into(),
            ));
        }

        let mut profile = crate::agent_registry::AgentProfile::new(
            params.role.clone(),
            cwd,
            std::path::Path::new("/"),
        );
        profile.harness = harness;
        let instructions = params
            .instructions
            .unwrap_or_else(|| format!("# {} agent\n", params.role));
        let (profile, registry) =
            crate::agent_registry::create_with_owned_instructions(profile, &instructions).map_err(
                |err| {
                    if err.kind() == std::io::ErrorKind::AlreadyExists {
                        AgentProfileError::AlreadyExists(params.role.clone())
                    } else {
                        AgentProfileError::PersistFailed(err.to_string())
                    }
                },
            )?;
        self.agent_registry = registry;
        self.sync_saved_agent_profiles();
        self.state.mark_session_dirty();
        self.schedule_session_save();
        Ok(profile_info(&profile))
    }

    pub(super) fn set_profile(
        &mut self,
        params: AgentProfileSetParams,
    ) -> Result<AgentProfileInfo, AgentProfileError> {
        if !valid_agent_name(&params.role) {
            return Err(AgentProfileError::InvalidRole);
        }
        let has_patch = params.harness.is_some()
            || params.native_cwd.is_some()
            || params.model.is_some()
            || params.effort.is_some()
            || params.apikey_ref.is_some()
            || params.allowlist.is_some()
            || params.clear_model
            || params.clear_effort
            || params.clear_apikey_ref
            || params.clear_allowlist;
        let conflicting_clear = (params.model.is_some() && params.clear_model)
            || (params.effort.is_some() && params.clear_effort)
            || (params.apikey_ref.is_some() && params.clear_apikey_ref)
            || (params.allowlist.is_some() && params.clear_allowlist);
        if !has_patch || conflicting_clear {
            return Err(AgentProfileError::InvalidPatch);
        }

        let harness = params
            .harness
            .map(|harness| {
                crate::detect::parse_agent_label(&harness)
                    .map(|agent| crate::detect::agent_label(agent).to_string())
                    .ok_or(AgentProfileError::InvalidHarness(harness))
            })
            .transpose()?;
        if params
            .model
            .as_deref()
            .is_some_and(|model| model.is_empty() || model.chars().any(char::is_control))
        {
            return Err(AgentProfileError::InvalidModel);
        }
        if params
            .allowlist
            .as_ref()
            .is_some_and(|allowlist| profile_allowlist_tools(allowlist).is_none())
        {
            return Err(AgentProfileError::InvalidAllowlist);
        }
        if params.apikey_ref.as_deref().is_some_and(|reference| {
            reference
                .strip_prefix("env:")
                .map_or(true, |source| !valid_env_name(source))
        }) {
            return Err(AgentProfileError::InvalidApiKeyRef);
        }
        let native_cwd = params
            .native_cwd
            .map(|cwd| {
                let path = std::path::PathBuf::from(&cwd);
                if !path.exists() {
                    return Err(AgentProfileError::NativeCwdNotFound(cwd));
                }
                if !path.is_dir() {
                    return Err(AgentProfileError::NativeCwdNotDirectory(cwd));
                }
                let path = std::fs::canonicalize(&path)
                    .map_err(|_| AgentProfileError::NativeCwdNotFound(cwd.clone()))?;
                if path.to_str().is_none() {
                    return Err(AgentProfileError::NativeCwdNotUtf8);
                }
                Ok(path)
            })
            .transpose()?;
        let seed_cwd = self.profile_seed_cwd()?;
        self.update_agent_registry(move |registry| {
            let profile_is_new = registry.get(&params.role).is_none();
            let profile = registry.register_or_get(params.role, seed_cwd);
            if profile_is_new && native_cwd.is_none() {
                profile.native_cwd_seeded = false;
            }
            if let Some(harness) = harness {
                profile.harness = harness;
            }
            if let Some(native_cwd) = native_cwd {
                profile.native_cwd = native_cwd;
                profile.native_cwd_seeded = true;
            }
            if let Some(model) = params.model {
                profile.model = Some(model);
            } else if params.clear_model {
                profile.model = None;
            }
            if let Some(effort) = params.effort {
                profile.effort = Some(effort_from_schema(effort));
            } else if params.clear_effort {
                profile.effort = None;
            }
            if let Some(apikey_ref) = params.apikey_ref {
                profile.apikey_ref = Some(apikey_ref);
            } else if params.clear_apikey_ref {
                profile.apikey_ref = None;
            }
            if let Some(allowlist) = params.allowlist {
                profile.allowlist = Some(allowlist);
            } else if params.clear_allowlist {
                profile.allowlist = None;
            }

            profile_info(profile)
        })
        .map_err(|err| AgentProfileError::PersistFailed(err.to_string()))
    }

    fn profile_seed_cwd(&self) -> Result<std::path::PathBuf, AgentProfileError> {
        let seed_cwd = self
            .state
            .active
            .and_then(|ws_idx| self.focused_pane_cwd_in_workspace(ws_idx))
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        if seed_cwd.to_str().is_none() {
            return Err(AgentProfileError::NativeCwdNotUtf8);
        }
        Ok(seed_cwd)
    }

    /// Map an [`AgentProfileError`] to a stable API error body.
    pub(super) fn agent_profile_error_body(
        &self,
        err: AgentProfileError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            AgentProfileError::InvalidRole => crate::api::schema::ErrorBody {
                code: "invalid_profile_role".into(),
                message: "role must match the agent name format (1-32 chars)".into(),
            },
            AgentProfileError::InvalidName => crate::api::schema::ErrorBody {
                code: "invalid_md_name".into(),
                message: "md name must not be empty".into(),
            },
            AgentProfileError::PathNotFound(path) => crate::api::schema::ErrorBody {
                code: "md_path_not_found".into(),
                message: format!("md file not found: {path}"),
            },
            AgentProfileError::PathNotFile(path) => crate::api::schema::ErrorBody {
                code: "md_path_not_file".into(),
                message: format!("md path is not a regular file: {path}"),
            },
            AgentProfileError::PathNotUtf8 => crate::api::schema::ErrorBody {
                code: "md_path_not_utf8".into(),
                message: "md paths must resolve to valid UTF-8 paths".into(),
            },
            AgentProfileError::NativeCwdNotUtf8 => crate::api::schema::ErrorBody {
                code: "agent_cwd_not_utf8".into(),
                message: "agent working directories must be valid UTF-8 paths".into(),
            },
            AgentProfileError::NotFound(role) => crate::api::schema::ErrorBody {
                code: "agent_profile_not_found".into(),
                message: format!("agent profile {role} not found"),
            },
            AgentProfileError::AlreadyExists(role) => crate::api::schema::ErrorBody {
                code: "agent_profile_already_exists".into(),
                message: format!("agent profile {role} already exists"),
            },
            AgentProfileError::InvalidInstructions => crate::api::schema::ErrorBody {
                code: "invalid_agent_instructions".into(),
                message: "agent instructions must not contain NUL bytes".into(),
            },
            AgentProfileError::InvalidPatch => crate::api::schema::ErrorBody {
                code: "invalid_profile_request".into(),
                message: "profile update must include a non-conflicting setting change".into(),
            },
            AgentProfileError::InvalidHarness(harness) => crate::api::schema::ErrorBody {
                code: "invalid_agent_kind".into(),
                message: format!("unsupported interactive agent kind {harness}"),
            },
            AgentProfileError::InvalidModel => crate::api::schema::ErrorBody {
                code: "invalid_agent_model".into(),
                message: "model must not be empty or contain control characters".into(),
            },
            AgentProfileError::InvalidAllowlist => crate::api::schema::ErrorBody {
                code: "invalid_agent_allowlist".into(),
                message: "allowlist must contain a non-empty string tools array".into(),
            },
            AgentProfileError::InvalidApiKeyRef => crate::api::schema::ErrorBody {
                code: "invalid_agent_api_key_ref".into(),
                message: "apikey_ref must use the env:NAME format".into(),
            },
            AgentProfileError::NativeCwdNotFound(path) => crate::api::schema::ErrorBody {
                code: "agent_cwd_not_found".into(),
                message: format!("agent working directory does not exist: {path}"),
            },
            AgentProfileError::NativeCwdNotDirectory(path) => crate::api::schema::ErrorBody {
                code: "agent_cwd_not_directory".into(),
                message: format!("agent working directory is not a directory: {path}"),
            },
            AgentProfileError::LoadFailed(message) => crate::api::schema::ErrorBody {
                code: "agent_profile_load_failed".into(),
                message: format!("failed to load agent profile: {message}"),
            },
            AgentProfileError::PersistFailed(message) => crate::api::schema::ErrorBody {
                code: "agent_profile_persist_failed".into(),
                message: format!("failed to persist agent profile: {message}"),
            },
        }
    }

    /// Build the [`TabAgentContext`] for a tab from live terminal/runtime state.
    fn agent_spawn_context(&self, ws_idx: usize, tab_idx: usize) -> TabAgentContext {
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return TabAgentContext::empty(PaneId::alloc());
        };
        let Some(tab) = ws.tabs.get(tab_idx) else {
            return TabAgentContext::empty(PaneId::alloc());
        };
        let pane_ids = tab.layout.pane_ids();
        let is_single_pane_tab = pane_ids.len() == 1;
        let panes = pane_ids
            .into_iter()
            .map(|pane_id| {
                let terminal = ws
                    .pane_state(pane_id)
                    .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id));
                let (is_agent, is_available, is_shell_starting) = match terminal {
                    None => (false, false, false),
                    Some(terminal) => {
                        let is_agent =
                            terminal.is_agent_terminal() || terminal.managed_agent_kind().is_some();
                        let is_available = !is_agent
                            && self
                                .terminal_runtimes
                                .get(&terminal.id)
                                .is_some_and(|runtime| {
                                    !runtime.input_was_sent_within(AGENT_SPAWN_RECENT_INPUT_GUARD)
                                        && available_shell_name(runtime).is_some()
                                });
                        let is_shell_starting = !is_agent
                            && !is_available
                            && is_single_pane_tab
                            && !self.pending_agent_spawns.iter().any(|spawn| {
                                spawn.pending.completion.ws_idx == ws_idx
                                    && spawn.pending.completion.target_pane_id == pane_id
                            })
                            && self
                                .terminal_runtimes
                                .get(&terminal.id)
                                .is_some_and(|runtime| !runtime.has_received_input());
                        (is_agent, is_available, is_shell_starting)
                    }
                };
                PaneAgentState {
                    pane_id,
                    is_agent,
                    is_available,
                    is_shell_starting,
                }
            })
            .collect();
        TabAgentContext { panes }
    }

    /// Render an [`AgentStartError`] as a stable one-line message. The start
    /// error type has no `Display` impl, so the spawn flow maps it explicitly.
    fn agent_start_message(&self, err: AgentStartError) -> String {
        match err {
            AgentStartError::InvalidName => "invalid agent name".to_string(),
            AgentStartError::UnsupportedKind(kind) => format!("unsupported agent kind {kind}"),
            AgentStartError::InvalidArgument => "invalid agent argument".to_string(),
            AgentStartError::CwdNotUtf8 => "agent cwd is not valid UTF-8".to_string(),
            AgentStartError::InvalidTimeout => "invalid agent timeout".to_string(),
            AgentStartError::TargetNotFound(pane) => format!("agent pane {pane} not found"),
            AgentStartError::TargetBusy(pane) => format!("agent pane {pane} busy"),
            AgentStartError::TargetUnavailable(pane) => format!("agent pane {pane} unavailable"),
            AgentStartError::InputFailed(message) => format!("agent input failed: {message}"),
            AgentStartError::RegistryFailed(message) => {
                format!("agent registry update failed: {message}")
            }
            AgentStartError::DuplicateName { name, .. } => format!("agent name {name} taken"),
        }
    }

    pub(super) fn agent_spawn_error_body(
        &self,
        err: AgentSpawnError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            AgentSpawnError::InvalidKind => crate::api::schema::ErrorBody {
                code: "invalid_agent_kind".into(),
                message: "unsupported interactive agent kind".into(),
            },
            AgentSpawnError::InvalidCwdMode => crate::api::schema::ErrorBody {
                code: "invalid_cwd_mode".into(),
                message: "cwd mode must be \"tab\" or \"agent\"".into(),
            },
            AgentSpawnError::InvalidStartInput(err) => self.agent_start_error_body(err),
            AgentSpawnError::NoActiveWorkspace => crate::api::schema::ErrorBody {
                code: "no_active_workspace".into(),
                message: "no active workspace to spawn into".into(),
            },
            AgentSpawnError::TabNotFound(tab_id) => crate::api::schema::ErrorBody {
                code: "agent_tab_not_found".into(),
                message: format!("agent target tab {tab_id} not found"),
            },
            AgentSpawnError::CwdNotFound(path) => crate::api::schema::ErrorBody {
                code: "agent_cwd_not_found".into(),
                message: format!("agent working directory does not exist: {}", path.display()),
            },
            AgentSpawnError::CwdNotUtf8 => crate::api::schema::ErrorBody {
                code: "agent_cwd_not_utf8".into(),
                message: "agent working directories must be valid UTF-8 paths".into(),
            },
            AgentSpawnError::ProfileMdUnsupported(kind) => crate::api::schema::ErrorBody {
                code: "agent_profile_md_unsupported".into(),
                message: format!("agent kind {kind} does not support profile markdown injection"),
            },
            AgentSpawnError::ProfileMdPathNotFile(path) => crate::api::schema::ErrorBody {
                code: "md_path_not_file".into(),
                message: format!("md path is not a regular file: {}", path.display()),
            },
            AgentSpawnError::ReplicaLimit => crate::api::schema::ErrorBody {
                code: "agent_replica_limit".into(),
                message: "agent replica index is exhausted".into(),
            },
            AgentSpawnError::ProfileSettingUnsupported(setting, kind) => {
                crate::api::schema::ErrorBody {
                    code: "agent_profile_setting_unsupported".into(),
                    message: format!(
                        "profile setting {setting} is not supported by agent kind {kind}"
                    ),
                }
            }
            AgentSpawnError::InvalidAllowlist => crate::api::schema::ErrorBody {
                code: "invalid_agent_allowlist".into(),
                message: "allowlist must contain a non-empty string tools array".into(),
            },
            AgentSpawnError::InvalidApiKeyRef => crate::api::schema::ErrorBody {
                code: "invalid_agent_api_key_ref".into(),
                message: "apikey_ref must reference an exported environment variable as env:NAME"
                    .into(),
            },
            AgentSpawnError::SpawnFailed(message) => crate::api::schema::ErrorBody {
                code: "agent_spawn_failed".into(),
                message,
            },
            AgentSpawnError::SplitFailed(message) => crate::api::schema::ErrorBody {
                code: "agent_spawn_split_failed".into(),
                message,
            },
        }
    }

    pub(super) fn agent_start_error_body(
        &self,
        err: AgentStartError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            AgentStartError::InvalidName => crate::api::schema::ErrorBody {
                code: "invalid_agent_name".into(),
                message: INVALID_AGENT_NAME_MESSAGE.into(),
            },
            AgentStartError::UnsupportedKind(kind) => crate::api::schema::ErrorBody {
                code: "unsupported_agent_kind".into(),
                message: format!("unsupported interactive agent kind {kind}"),
            },
            AgentStartError::InvalidArgument => crate::api::schema::ErrorBody {
                code: "invalid_agent_argument".into(),
                message: "agent arguments cannot be encoded safely for the target shell".into(),
            },
            AgentStartError::CwdNotUtf8 => crate::api::schema::ErrorBody {
                code: "agent_cwd_not_utf8".into(),
                message: "agent working directories must be valid UTF-8 paths".into(),
            },
            AgentStartError::InvalidTimeout => crate::api::schema::ErrorBody {
                code: "invalid_agent_timeout".into(),
                message: INVALID_AGENT_TIMEOUT_MESSAGE.into(),
            },
            AgentStartError::TargetNotFound(target) => crate::api::schema::ErrorBody {
                code: "agent_pane_not_found".into(),
                message: format!("agent target pane {target} not found"),
            },
            AgentStartError::TargetBusy(target) => crate::api::schema::ErrorBody {
                code: "agent_pane_busy".into(),
                message: format!("agent target pane {target} is not an available shell"),
            },
            AgentStartError::TargetUnavailable(target) => crate::api::schema::ErrorBody {
                code: "agent_pane_unavailable".into(),
                message: format!("agent target pane {target} has no live terminal"),
            },
            AgentStartError::InputFailed(message) => crate::api::schema::ErrorBody {
                code: "agent_start_input_failed".into(),
                message,
            },
            AgentStartError::RegistryFailed(message) => crate::api::schema::ErrorBody {
                code: "agent_registry_failed".into(),
                message,
            },
            AgentStartError::DuplicateName { name, candidates } => crate::api::schema::ErrorBody {
                code: "agent_name_taken".into(),
                message: format!(
                    "agent name {name} is already used; candidates: {}",
                    candidates
                        .into_iter()
                        .map(|candidate| format!(
                            "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                            candidate.terminal_id,
                            candidate.pane_id,
                            candidate.workspace_id,
                            candidate.tab_id,
                            candidate.cwd.unwrap_or_else(|| "unknown".into()),
                            candidate.agent_status,
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            },
        }
    }

    pub(super) fn agent_target_error_body(
        &self,
        err: TerminalTargetError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            TerminalTargetError::NotFound { target } => crate::api::schema::ErrorBody {
                code: "agent_not_found".into(),
                message: format!("agent target {target} not found"),
            },
            TerminalTargetError::Ambiguous { target, candidates } => {
                crate::api::schema::ErrorBody {
                    code: "agent_target_ambiguous".into(),
                    message: format!(
                        "agent target {target} is ambiguous; candidates: {}",
                        candidates
                            .into_iter()
                            .map(|candidate| format!(
                                "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                                candidate.terminal_id,
                                candidate.pane_id,
                                candidate.workspace_id,
                                candidate.tab_id,
                                candidate.cwd.unwrap_or_else(|| "unknown".into()),
                                candidate.agent_status,
                            ))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                }
            }
        }
    }

    pub(super) fn agent_rename_error_body(
        &self,
        err: AgentRenameError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            AgentRenameError::Target(err) => self.agent_target_error_body(err),
            AgentRenameError::InvalidName => crate::api::schema::ErrorBody {
                code: "invalid_agent_name".into(),
                message: INVALID_AGENT_NAME_MESSAGE.into(),
            },
            AgentRenameError::NotAgent => crate::api::schema::ErrorBody {
                code: "agent_not_found".into(),
                message: "agent target does not currently host an agent".into(),
            },
            AgentRenameError::PendingLaunch => crate::api::schema::ErrorBody {
                code: "agent_launch_pending".into(),
                message: "agent name cannot change while startup is pending".into(),
            },
            AgentRenameError::DuplicateName { name, candidates } => crate::api::schema::ErrorBody {
                code: "agent_name_taken".into(),
                message: format!(
                    "agent name {name} is already used; candidates: {}",
                    candidates
                        .into_iter()
                        .map(|candidate| format!(
                            "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                            candidate.terminal_id,
                            candidate.pane_id,
                            candidate.workspace_id,
                            candidate.tab_id,
                            candidate.cwd.unwrap_or_else(|| "unknown".into()),
                            candidate.agent_status,
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            },
        }
    }

    pub(super) fn agent_info(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<crate::api::schema::AgentInfo> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_state = ws.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane_state.attached_terminal_id)?;
        if !terminal.is_agent_terminal() {
            return None;
        }
        let pane = self.pane_info(ws_idx, pane_id)?;
        Some(crate::api::schema::AgentInfo {
            terminal_id: pane.terminal_id,
            name: terminal.agent_name.clone(),
            agent: pane.agent,
            title: pane.title,
            terminal_title: pane.terminal_title,
            terminal_title_stripped: pane.terminal_title_stripped,
            display_agent: pane.display_agent,
            agent_status: pane.agent_status,
            screen_detection_skipped: terminal.full_lifecycle_hook_authority_active(),
            state_labels: pane.state_labels,
            tokens: pane.tokens,
            agent_session: pane.agent_session,
            workspace_id: pane.workspace_id,
            tab_id: pane.tab_id,
            pane_id: pane.pane_id,
            focused: pane.focused,
            launch_pending: terminal.managed_agent_launch_pending(),
            interactive_ready: terminal.managed_agent_interactive_ready(),
            state_change_seq: terminal.last_agent_state_change_seq.unwrap_or(0),
            cwd: pane.cwd,
            foreground_cwd: pane.foreground_cwd,
            revision: pane.revision,
        })
    }

    fn agent_name_conflicts(
        &self,
        name: &str,
        except_terminal_id: &str,
    ) -> Vec<crate::api::schema::AgentInfo> {
        self.collect_agent_infos()
            .into_iter()
            .filter(|agent| {
                agent.name.as_deref() == Some(name) && agent.terminal_id != except_terminal_id
            })
            .collect()
    }
}

fn available_shell_name(runtime: &crate::terminal::TerminalRuntime) -> Option<String> {
    #[cfg(test)]
    if runtime.child_pid().is_none() {
        return Some("sh".into());
    }
    crate::platform::available_pane_shell(runtime.child_pid()?)
}

pub(super) fn runtime_hosts_agent(
    runtime: &crate::terminal::TerminalRuntime,
    expected: crate::detect::Agent,
) -> bool {
    #[cfg(test)]
    if runtime.child_pid().is_none() {
        return true;
    }
    live_runtime_agent(runtime) == Some(expected)
}

fn live_runtime_agent(runtime: &crate::terminal::TerminalRuntime) -> Option<crate::detect::Agent> {
    let job = crate::detect::foreground_job(runtime.child_pid()?)?;
    crate::detect::identify_agent_in_job(&job)
        .map(|(agent, _)| agent)
        .or_else(|| {
            job.processes
                .iter()
                .find_map(|process| crate::platform::process_agent_hint(process.pid))
        })
}

#[derive(Debug)]
pub(super) enum AgentStartError {
    InvalidName,
    UnsupportedKind(String),
    InvalidArgument,
    CwdNotUtf8,
    InvalidTimeout,
    TargetNotFound(String),
    TargetBusy(String),
    TargetUnavailable(String),
    InputFailed(String),
    RegistryFailed(String),
    DuplicateName {
        name: String,
        candidates: Vec<crate::api::schema::AgentInfo>,
    },
}

pub(super) enum AgentRenameError {
    Target(TerminalTargetError),
    InvalidName,
    NotAgent,
    PendingLaunch,
    DuplicateName {
        name: String,
        candidates: Vec<crate::api::schema::AgentInfo>,
    },
}

/// Errors from the owlspace-style spawn flow.
#[derive(Debug)]
pub(super) enum AgentSpawnError {
    /// The requested interactive agent kind is not supported.
    InvalidKind,
    /// The cwd mode string was neither `tab` nor `agent`.
    InvalidCwdMode,
    /// A shared agent-start input validation rule rejected the request.
    InvalidStartInput(AgentStartError),
    /// There is no active workspace to spawn into.
    NoActiveWorkspace,
    /// The requested target tab does not exist.
    TabNotFound(String),
    /// The resolved working directory no longer exists.
    CwdNotFound(std::path::PathBuf),
    /// The working directory cannot be represented by the JSON API or shell command.
    CwdNotUtf8,
    /// The chosen harness cannot safely receive profile markdown files.
    ProfileMdUnsupported(String),
    /// A profile markdown file disappeared or became non-regular before launch.
    ProfileMdPathNotFile(std::path::PathBuf),
    /// The profile has used every representable replica index.
    ReplicaLimit,
    /// A saved setting cannot be represented by the selected harness.
    ProfileSettingUnsupported(String, String),
    /// The allowlist must be an object containing a non-empty string `tools` array.
    InvalidAllowlist,
    /// API-key references are environment indirections in the form `env:NAME`.
    InvalidApiKeyRef,
    /// `start_agent` failed once the pane was ready.
    SpawnFailed(String),
    /// Auto-splitting a pane to make room for the agent failed.
    SplitFailed(String),
}

#[cfg(test)]
mod tests {
    use super::{
        profile_launch_settings, profile_md_args, valid_agent_name,
        validate_profile_md_canonical_path, AgentProfileEffort, AgentProfileError,
        AgentProfileSetParams, AgentSpawnError, AgentSpawnOutcome, AgentStartError,
    };
    use crate::{
        agent_registry::{AgentProfile, EffortLevel},
        api::schema::AgentStartParams,
        app::App,
        config::Config,
        workspace::Workspace,
    };

    #[test]
    fn profile_launch_settings_map_to_claude_flags_without_exposing_key_values() {
        let root = std::env::temp_dir();
        let mut profile = AgentProfile::new("reviewer", root.clone(), &root);
        profile.model = Some("sonnet".into());
        profile.effort = Some(EffortLevel::High);
        profile.apikey_ref = Some("env:PATH".into());
        profile.allowlist = Some(serde_json::json!({"tools":["Read", "Bash(git *)"]}));

        assert_eq!(
            profile_launch_settings("claude", &profile).unwrap(),
            (
                vec![
                    "--model".into(),
                    "sonnet".into(),
                    "--effort".into(),
                    "high".into(),
                    "--allowedTools".into(),
                    "Read".into(),
                    "--allowedTools".into(),
                    "Bash(git *)".into(),
                ],
                Some(("ANTHROPIC_API_KEY".into(), "PATH".into())),
            )
        );
    }

    #[test]
    fn profile_launch_settings_map_codex_and_pi_conventions() {
        let root = std::env::temp_dir();
        let mut codex = AgentProfile::new("reviewer", root.clone(), &root);
        codex.model = Some("gpt-5.2".into());
        codex.effort = Some(EffortLevel::Low);
        codex.apikey_ref = Some("env:PATH".into());
        assert_eq!(
            profile_launch_settings("codex", &codex).unwrap(),
            (
                vec![
                    "--model".into(),
                    "gpt-5.2".into(),
                    "-c".into(),
                    "model_reasoning_effort=\"low\"".into(),
                ],
                Some(("OPENAI_API_KEY".into(), "PATH".into())),
            )
        );

        let mut pi = AgentProfile::new("reviewer", root.clone(), &root);
        pi.model = Some("anthropic/sonnet".into());
        pi.effort = Some(EffortLevel::High);
        pi.allowlist = Some(serde_json::json!({"tools":["read", "write"]}));
        assert_eq!(
            profile_launch_settings("pi", &pi).unwrap().0,
            Vec::<String>::from([
                "--model".into(),
                "anthropic/sonnet:high".into(),
                "--tools".into(),
                "read,write".into(),
            ])
        );
    }

    #[tokio::test]
    async fn spawn_reuses_an_available_shell_in_profile_cwd_with_claude_markdown() {
        let root =
            std::env::temp_dir().join(format!("herdr-agent-spawn-profile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let tab_cwd = root.join("tab");
        let profile_cwd = root.join("profile");
        std::fs::create_dir_all(&tab_cwd).unwrap();
        std::fs::create_dir_all(&profile_cwd).unwrap();
        let markdown = root.join("context.md");
        std::fs::write(&markdown, "team context").unwrap();

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let mut workspace = Workspace::test_new("spawn-profile");
        workspace.identity_cwd = tab_cwd;
        let pane_id = workspace.tabs[0].root_pane;
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        let terminal_id = app.state.terminal_id_for_pane(0, pane_id).unwrap();
        let profile = app
            .agent_registry
            .register_or_get("reviewer", profile_cwd.clone());
        profile.harness = "claude".into();
        profile.set_md("context.md", markdown.clone());
        profile.model = Some("sonnet".into());
        profile.effort = Some(EffortLevel::High);
        profile.apikey_ref = Some("env:PATH".into());
        profile.allowlist = Some(serde_json::json!({"tools":["Read"]}));

        let (runtime, mut submitted) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 1);
        app.terminal_runtimes.insert(terminal_id, runtime);

        let spawned = match app.spawn_agent(crate::api::schema::AgentSpawnParams {
            role: "reviewer".into(),
            kind: None,
            tab_id: None,
            cwd_mode: "agent".into(),
            timeout_ms: Some(30_000),
            args: Vec::new(),
        }) {
            Ok(AgentSpawnOutcome::Spawned(spawned)) => spawned,
            Ok(AgentSpawnOutcome::Pending(_)) => panic!("profile spawn should not be deferred"),
            Err(_) => panic!("profile spawn should succeed"),
        };

        assert!(!spawned.split);
        assert_eq!(spawned.pane_id, app.public_pane_id(0, pane_id).unwrap());
        assert_eq!(
            spawned.argv,
            vec![
                "claude".to_string(),
                "--append-system-prompt-file".to_string(),
                markdown.display().to_string(),
                "--model".to_string(),
                "sonnet".to_string(),
                "--effort".to_string(),
                "high".to_string(),
                "--allowedTools".to_string(),
                "Read".to_string(),
            ]
        );
        let command = String::from_utf8(submitted.try_recv().unwrap().to_vec()).unwrap();
        assert!(command.contains(profile_cwd.to_str().unwrap()));
        assert!(command.contains("--append-system-prompt-file"));
        assert!(command.contains(markdown.to_str().unwrap()));
        assert!(command.contains("ANTHROPIC_API_KEY=\"${PATH}\""));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn revive_reuses_the_exact_archived_instance_identity() {
        let root = std::env::temp_dir().join(format!("herdr-agent-revive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let mut workspace = Workspace::test_new("revive");
        workspace.identity_cwd = root.clone();
        let pane_id = workspace.tabs[0].root_pane;
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        let terminal_id = app.state.terminal_id_for_pane(0, pane_id).unwrap();
        let (runtime, _submitted) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 1);
        app.terminal_runtimes.insert(terminal_id, runtime);
        app.agent_registry.register_or_get("reviewer", root.clone());
        let instance_id = app
            .agent_registry
            .roster_register(
                "reviewer-replica-1",
                "reviewer",
                "reviewer",
                "-replica-1",
                Some("w1:t1:p9".into()),
            )
            .unwrap()
            .instance_id
            .clone();
        assert!(app.agent_registry.roster_terminate_instance(&instance_id));

        let revived = match app.revive_agent(
            instance_id.clone(),
            None,
            "tab".into(),
            Some(30_000),
            Vec::new(),
        ) {
            Ok(AgentSpawnOutcome::Spawned(revived)) => revived,
            Ok(AgentSpawnOutcome::Pending(_)) => panic!("revive should use the available shell"),
            Err(_) => panic!("revive should succeed"),
        };
        assert_eq!(revived.name, "reviewer-replica-1");
        assert_eq!(app.agent_registry.roster.len(), 1);
        assert_eq!(
            app.agent_registry.roster[&instance_id].last_pane.as_deref(),
            Some(revived.pane_id.as_str())
        );
        assert_ne!(
            app.agent_registry.roster[&instance_id].status,
            crate::agent_registry::AgentStatus::Terminated
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn profile_markdown_codex_uses_native_instructions_file() {
        let root = std::env::temp_dir().join(format!(
            "herdr-agent-spawn-profile-rejection-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let markdown = root.join("context.md");
        std::fs::write(&markdown, "team context").unwrap();
        assert_eq!(
            profile_md_args(
                "codex",
                &[crate::agent_registry::AgentMd {
                    name: "AGENTS.md".into(),
                    path: markdown.clone(),
                }],
            )
            .unwrap(),
            vec![
                "-c".into(),
                format!("model_instructions_file=\"{}\"", markdown.display()),
            ]
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn profile_markdown_injects_each_owned_document_for_claude() {
        let root = std::env::temp_dir().join(format!(
            "herdr-agent-spawn-profile-documents-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let agents = root.join("AGENTS.md");
        let review = root.join("review.md");
        std::fs::write(&agents, "base instructions").unwrap();
        std::fs::write(&review, "review instructions").unwrap();

        assert_eq!(
            profile_md_args(
                "claude",
                &[
                    crate::agent_registry::AgentMd {
                        name: "AGENTS.md".into(),
                        path: agents.clone(),
                    },
                    crate::agent_registry::AgentMd {
                        name: "review.md".into(),
                        path: review.clone(),
                    },
                ],
            )
            .unwrap(),
            vec![
                "--append-system-prompt-file".into(),
                agents.display().to_string(),
                "--append-system-prompt-file".into(),
                review.display().to_string(),
            ]
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn spawn_rejects_non_utf8_cwd_before_registry_or_layout_mutation() {
        use std::os::unix::ffi::OsStringExt;

        let cwd =
            std::path::PathBuf::from(std::ffi::OsString::from_vec(b"workspace-\x80".to_vec()));

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let mut workspace = Workspace::test_new("non-utf8-cwd");
        workspace.identity_cwd = cwd;
        let pane_id = workspace.tabs[0].root_pane;
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;

        assert!(matches!(
            app.spawn_agent(crate::api::schema::AgentSpawnParams {
                role: "reviewer".into(),
                kind: Some("claude".into()),
                tab_id: None,
                cwd_mode: "tab".into(),
                timeout_ms: Some(30_000),
                args: Vec::new(),
            }),
            Err(AgentSpawnError::CwdNotUtf8)
        ));
        assert!(app.agent_registry.profiles.is_empty());
        assert!(app.agent_registry.roster.is_empty());
        assert_eq!(
            app.state.workspaces[0].tabs[0].layout.pane_ids(),
            vec![pane_id]
        );
    }

    #[test]
    fn set_profile_md_sets_validates_and_clears_injected_md() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );

        // A missing path is rejected so a typo never reaches a spawned replica.
        let missing = app.set_profile_md(
            "mdtest",
            "context.md",
            Some("/no/such/whereever/context.md"),
        );
        assert_eq!(
            missing,
            Err(AgentProfileError::PathNotFound(
                "/no/such/whereever/context.md".into()
            ))
        );

        // A brand-new profile with no `.md`s is empty; the native cwd falls back
        // to "/" when no active workspace supplies one.
        let empty = app.set_profile_md("mdtest", "context.md", None).unwrap();
        assert_eq!(empty.role, "mdtest");
        assert_eq!(empty.native_cwd, "/");
        assert!(empty.mds.is_empty());

        // Inject a real `.md`, then replace its path under the same name.
        let root = std::env::temp_dir().join(format!("herdr-md-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let directory = root.join("directory");
        std::fs::create_dir_all(&directory).unwrap();
        assert_eq!(
            app.set_profile_md("mdtest", "context.md", Some(directory.to_str().unwrap()),),
            Err(AgentProfileError::PathNotFile(
                directory.to_str().unwrap().into()
            ))
        );
        let first = root.join("context.md");
        std::fs::write(&first, "team rules").unwrap();
        let injected = app
            .set_profile_md("mdtest", "context.md", Some(first.to_str().unwrap()))
            .unwrap();
        assert_eq!(injected.mds.len(), 1);
        assert_eq!(injected.mds[0].name, "context.md");
        assert_eq!(
            injected.mds[0].path,
            std::fs::canonicalize(&first).unwrap().display().to_string()
        );

        let second = root.join("todo.md");
        std::fs::write(&second, "tasks").unwrap();
        let replaced = app
            .set_profile_md("mdtest", "context.md", Some(second.to_str().unwrap()))
            .unwrap();
        assert_eq!(replaced.mds.len(), 1);
        assert_eq!(
            replaced.mds[0].path,
            std::fs::canonicalize(&second)
                .unwrap()
                .display()
                .to_string()
        );

        // An empty path removes the `.md`.
        let removed = app
            .set_profile_md("mdtest", "context.md", Some(""))
            .unwrap();
        assert!(removed.mds.is_empty());

        // Invalid role and empty name are rejected before any mutation.
        assert!(matches!(
            app.set_profile_md("Bad Role", "x", None),
            Err(AgentProfileError::InvalidRole)
        ));
        assert!(matches!(
            app.set_profile_md("mdtest", "   ", None),
            Err(AgentProfileError::InvalidName)
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn set_profile_md_rejects_a_non_utf8_canonical_path() {
        use std::os::unix::ffi::OsStringExt;

        assert_eq!(
            validate_profile_md_canonical_path(std::path::PathBuf::from(
                std::ffi::OsString::from_vec(b"context-\x80.md".to_vec()),
            )),
            Err(AgentProfileError::PathNotUtf8)
        );
    }

    #[test]
    fn set_profile_md_stores_an_absolute_path_for_cwd_independent_injection() {
        let relative_root = format!("herdr-agent-profile-relative-md-{}", std::process::id());
        let root = std::env::current_dir().unwrap().join(&relative_root);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let markdown = root.join("context.md");
        std::fs::write(&markdown, "persistent profile context").unwrap();

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );

        let profile = app
            .set_profile_md(
                "reviewer",
                "context.md",
                Some(&format!("{relative_root}/context.md")),
            )
            .unwrap();

        assert_eq!(
            profile.mds[0].path,
            std::fs::canonicalize(&markdown)
                .unwrap()
                .display()
                .to_string()
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn saved_profile_settings_are_explicitly_editable_without_clobbering_markdown() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let root = std::env::temp_dir().join(format!(
            "herdr-agent-profile-settings-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let markdown = root.join("context.md");
        std::fs::write(&markdown, "context").unwrap();
        app.set_profile_md("reviewer", "context.md", markdown.to_str())
            .unwrap();

        let profile = app
            .set_profile(AgentProfileSetParams {
                role: "reviewer".into(),
                harness: Some("claude".into()),
                native_cwd: Some(root.display().to_string()),
                model: Some("sonnet".into()),
                effort: Some(AgentProfileEffort::High),
                apikey_ref: Some("env:REVIEWER_API_KEY".into()),
                allowlist: Some(serde_json::json!({"tools":["read"]})),
                clear_model: false,
                clear_effort: false,
                clear_apikey_ref: false,
                clear_allowlist: false,
            })
            .unwrap();

        assert_eq!(profile.harness, "claude");
        assert!(profile.native_cwd_seeded);
        assert_eq!(profile.model.as_deref(), Some("sonnet"));
        assert_eq!(profile.effort, Some(AgentProfileEffort::High));
        assert_eq!(profile.mds.len(), 1);
        assert_eq!(app.profiles().unwrap(), vec![profile.clone()]);

        let cleared = app
            .set_profile(AgentProfileSetParams {
                role: "reviewer".into(),
                harness: None,
                native_cwd: None,
                model: None,
                effort: None,
                apikey_ref: None,
                allowlist: None,
                clear_model: true,
                clear_effort: true,
                clear_apikey_ref: true,
                clear_allowlist: true,
            })
            .unwrap();
        assert!(cleared.model.is_none());
        assert!(cleared.effort.is_none());
        assert!(cleared.apikey_ref.is_none());
        assert!(cleared.allowlist.is_none());
        assert_eq!(cleared.mds.len(), 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn deleting_profile_removes_saved_profile_and_archived_roster() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.set_profile_md("reviewer", "context.md", None).unwrap();
        app.agent_registry.roster_register(
            "reviewer-replica-1",
            "reviewer",
            "reviewer",
            "-replica-1",
            None,
        );
        assert!(app.delete_profile("reviewer").is_ok());
        assert!(app.agent_registry.get("reviewer").is_none());
        assert!(app.agent_registry.roster.is_empty());
        assert!(app.state.saved_agent_profiles.is_empty());
    }

    #[test]
    fn failed_profile_persistence_leaves_the_in_memory_registry_unchanged() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.no_session = false;
        app.agent_registry.version = crate::agent_registry::REGISTRY_VERSION + 1;

        let err = app
            .set_profile(AgentProfileSetParams {
                role: "reviewer".into(),
                harness: Some("claude".into()),
                native_cwd: None,
                model: None,
                effort: None,
                apikey_ref: None,
                allowlist: None,
                clear_model: false,
                clear_effort: false,
                clear_apikey_ref: false,
                clear_allowlist: false,
            })
            .unwrap_err();
        assert!(matches!(err, AgentProfileError::PersistFailed(_)));
        assert_eq!(
            app.agent_profile_error_body(err).code,
            "agent_profile_persist_failed"
        );
        assert!(app.agent_registry.get("reviewer").is_none());
    }

    #[tokio::test]
    async fn spawn_targets_the_requested_background_tab_without_changing_focus() {
        let root = std::env::temp_dir().join(format!(
            "herdr-agent-spawn-target-tab-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let mut workspace = Workspace::test_new("spawn-target-tab");
        workspace.identity_cwd = root.clone();
        let target_tab_idx = workspace.test_add_tab(Some("background"));
        let target_pane_id = workspace.tabs[target_tab_idx].root_pane;
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;

        let target_terminal_id = app.state.terminal_id_for_pane(0, target_pane_id).unwrap();
        let (runtime, mut submitted) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 1);
        app.terminal_runtimes.insert(target_terminal_id, runtime);

        let target_tab_id = app.public_tab_id(0, target_tab_idx).unwrap();
        let spawned = match app.spawn_agent(crate::api::schema::AgentSpawnParams {
            role: "background".into(),
            kind: Some("claude".into()),
            tab_id: Some(target_tab_id),
            cwd_mode: "tab".into(),
            timeout_ms: Some(30_000),
            args: Vec::new(),
        }) {
            Ok(AgentSpawnOutcome::Spawned(spawned)) => spawned,
            Ok(AgentSpawnOutcome::Pending(_)) => {
                panic!("background-tab spawn should not be deferred")
            }
            Err(_) => panic!("background-tab spawn should succeed"),
        };

        assert_eq!(
            spawned.pane_id,
            app.public_pane_id(0, target_pane_id).unwrap()
        );
        assert_eq!(app.state.workspaces[0].active_tab, 0);
        assert!(String::from_utf8(submitted.try_recv().unwrap().to_vec())
            .unwrap()
            .contains(root.to_str().unwrap()));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn spawn_skips_an_already_live_replica_name_when_the_counter_is_stale() {
        let root = std::env::temp_dir().join(format!(
            "herdr-agent-spawn-replica-name-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let mut workspace = Workspace::test_new("spawn-replica-name");
        workspace.identity_cwd = root.clone();
        let primary_pane_id = workspace.tabs[0].root_pane;
        let first_replica_pane_id = workspace.test_split(ratatui::layout::Direction::Horizontal);
        let available_pane_id = workspace.test_split(ratatui::layout::Direction::Horizontal);
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;

        let primary_terminal_id = app.state.terminal_id_for_pane(0, primary_pane_id).unwrap();
        let first_replica_terminal_id = app
            .state
            .terminal_id_for_pane(0, first_replica_pane_id)
            .unwrap();
        app.state
            .terminals
            .get_mut(&primary_terminal_id)
            .unwrap()
            .set_agent_name("reviewer".into());
        app.state
            .terminals
            .get_mut(&primary_terminal_id)
            .unwrap()
            .set_detected_state(
                Some(crate::detect::Agent::Claude),
                crate::detect::AgentState::Idle,
            );
        app.state
            .terminals
            .get_mut(&first_replica_terminal_id)
            .unwrap()
            .set_agent_name("reviewer-replica-1".into());
        app.state
            .terminals
            .get_mut(&first_replica_terminal_id)
            .unwrap()
            .set_detected_state(
                Some(crate::detect::Agent::Claude),
                crate::detect::AgentState::Idle,
            );

        let available_terminal_id = app
            .state
            .terminal_id_for_pane(0, available_pane_id)
            .unwrap();
        let (runtime, _submitted) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 1);
        app.terminal_runtimes.insert(available_terminal_id, runtime);
        app.agent_registry
            .register_or_get("reviewer", root.clone())
            .replicas_assigned = 0;

        let spawned = match app.spawn_agent(crate::api::schema::AgentSpawnParams {
            role: "reviewer".into(),
            kind: Some("claude".into()),
            tab_id: None,
            cwd_mode: "tab".into(),
            timeout_ms: Some(30_000),
            args: Vec::new(),
        }) {
            Ok(AgentSpawnOutcome::Spawned(spawned)) => spawned,
            Ok(AgentSpawnOutcome::Pending(_)) => panic!("next free replica should not be deferred"),
            Err(_) => panic!("next free replica should spawn"),
        };

        assert_eq!(spawned.name, "reviewer-replica-2");
        assert!(!spawned.split);
        assert_eq!(
            app.agent_registry
                .get("reviewer")
                .unwrap()
                .replicas_assigned,
            2
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_spawn_releases_its_name_reservation() {
        let root = std::env::temp_dir().join(format!(
            "herdr-agent-spawn-reservation-release-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let mut workspace = Workspace::test_new("spawn-reservation-release");
        workspace.identity_cwd = root.clone();
        let pane_id = workspace.tabs[0].root_pane;
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        let terminal_id = app.state.terminal_id_for_pane(0, pane_id).unwrap();
        let (blocked_runtime, submitted) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 1);
        drop(submitted);
        app.terminal_runtimes
            .insert(terminal_id.clone(), blocked_runtime);

        let params = crate::api::schema::AgentSpawnParams {
            role: "reviewer".into(),
            kind: Some("claude".into()),
            tab_id: None,
            cwd_mode: "tab".into(),
            timeout_ms: Some(30_000),
            args: Vec::new(),
        };
        assert!(matches!(
            app.spawn_agent(params.clone()),
            Err(AgentSpawnError::SpawnFailed(_))
        ));
        assert!(app.agent_registry.alive_instance("reviewer").is_none());

        let (runtime, _submitted) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 1);
        app.terminal_runtimes.insert(terminal_id, runtime);
        let spawned = match app.spawn_agent(params) {
            Ok(AgentSpawnOutcome::Spawned(spawned)) => spawned,
            Ok(AgentSpawnOutcome::Pending(_)) => panic!("retry should find an available shell"),
            Err(_) => panic!("retry should succeed"),
        };
        assert_eq!(spawned.name, "reviewer");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn released_replica_reservation_is_removed_and_reused() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let root = std::path::PathBuf::from("/tmp/herdr-agent-reservation-release");
        app.agent_registry.register_or_get("reviewer", root.clone());
        app.agent_registry.roster_register(
            "reviewer",
            "reviewer",
            "reviewer",
            "",
            Some("w1:t1:p1".into()),
        );

        let reservation = match app.reserve_agent_spawn("reviewer", root.clone(), true, Vec::new())
        {
            Ok(reservation) => reservation,
            Err(_) => panic!("replica reservation should succeed"),
        };
        assert_eq!(reservation.agent_name, "reviewer-replica-1");
        let later_reservation =
            match app.reserve_agent_spawn("reviewer", root.clone(), true, Vec::new()) {
                Ok(reservation) => reservation,
                Err(_) => panic!("later replica reservation should succeed"),
            };
        assert_eq!(later_reservation.agent_name, "reviewer-replica-2");

        app.release_agent_spawn_reservation(&reservation.instance_id, false);

        assert_eq!(
            app.agent_registry
                .get("reviewer")
                .unwrap()
                .replicas_assigned,
            0
        );
        assert_eq!(app.agent_registry.roster.len(), 2);
        assert!(app.agent_registry.roster.values().all(|entry| {
            matches!(
                entry.display_name.as_str(),
                "reviewer" | "reviewer-replica-2"
            )
        }));

        let retry = match app.reserve_agent_spawn("reviewer", root, true, Vec::new()) {
            Ok(reservation) => reservation,
            Err(_) => panic!("released replica name should be reusable"),
        };
        assert_eq!(retry.agent_name, "reviewer-replica-1");
    }

    #[tokio::test]
    async fn start_releases_roster_reservation_when_input_fails() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let workspace = Workspace::test_new("start-reservation-release");
        let pane_id = workspace.tabs[0].root_pane;
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        let terminal_id = app.state.terminal_id_for_pane(0, pane_id).unwrap();
        let (runtime, submitted) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 1);
        drop(submitted);
        app.terminal_runtimes.insert(terminal_id, runtime);
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();

        assert!(matches!(
            app.start_agent(AgentStartParams {
                name: "reviewer".into(),
                kind: "claude".into(),
                pane_id: public_pane_id,
                args: Vec::new(),
                timeout_ms: Some(30_000),
            }),
            Err(AgentStartError::InputFailed(_))
        ));
        assert!(app.agent_registry.alive_instance("reviewer").is_none());
    }

    #[test]
    fn agent_names_use_a_small_cli_safe_grammar() {
        for name in ["a", "reviewer-one", "reviewer_2", &"a".repeat(32)] {
            assert!(valid_agent_name(name), "expected {name:?} to be valid");
        }
        for name in [
            "",
            " reviewer",
            "reviewer ",
            "reviewer one",
            "Reviewer",
            "1reviewer",
            "reviewer.one",
            &"a".repeat(33),
        ] {
            assert!(!valid_agent_name(name), "expected {name:?} to be invalid");
        }
    }
}
