use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::api::schema::{
    AgentBroadcastParams, AgentProfileCreateParams, AgentProfileSetMdParams, AgentPromptParams,
    AgentRenameParams, AgentReviveParams, AgentRosterEntryInfo, AgentRosterStatus,
    AgentSendKeysParams, AgentSpawnParams, AgentStartParams, AgentTarget, PaneReadResult,
    ResponseResult,
};
use crate::app::App;

use super::responses::{encode_error, encode_error_body, encode_success};

const AGENT_PROMPT_SUBMIT_DELAY: Duration = Duration::from_millis(300);

impl App {
    pub(super) fn handle_agent_list(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::AgentList {
                agents: self.collect_agent_infos(),
            },
        )
    }

    pub(super) fn handle_agent_get(&mut self, id: String, target: AgentTarget) -> String {
        self.reconcile_managed_agent_target(&target.target);
        let agent = match self.agent_info_for_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_focus(&mut self, id: String, target: AgentTarget) -> String {
        let agent = match self.focus_agent_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_rename(&mut self, id: String, params: AgentRenameParams) -> String {
        let agent = match self.rename_agent_target(&params.target, params.name) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_rename_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_start(&mut self, id: String, params: AgentStartParams) -> String {
        let (agent, argv) = match self.start_agent(params) {
            Ok(started) => started,
            Err(err) => return encode_error_body(id, self.agent_start_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentStarted { agent, argv })
    }

    pub(super) fn handle_agent_spawn(&mut self, id: String, params: AgentSpawnParams) -> String {
        match self.spawn_agent(params) {
            Ok(crate::app::agents::AgentSpawnOutcome::Spawned(spawned)) => {
                encode_success(id, ResponseResult::AgentSpawned { spawned: *spawned })
            }
            Ok(crate::app::agents::AgentSpawnOutcome::Pending(pending)) => {
                let pane_id = pending.pane_id().to_string();
                self.cancel_pending_agent_spawn(*pending);
                encode_error(
                    id,
                    "agent_spawn_failed",
                    format!("agent pane {pane_id} busy"),
                )
            }
            Err(err) => encode_error_body(id, self.agent_spawn_error_body(err)),
        }
    }

    pub(super) fn handle_agent_roster_list(&mut self, id: String) -> String {
        if let Err(err) = self.refresh_agent_registry_for_read() {
            return encode_error_body(
                id,
                self.agent_profile_error_body(crate::app::agents::AgentProfileError::LoadFailed(
                    err.to_string(),
                )),
            );
        }
        let entries = self
            .agent_registry
            .roster
            .values()
            .cloned()
            .map(|entry| AgentRosterEntryInfo {
                instance_id: entry.instance_id,
                profile_id: entry.profile_id,
                role: entry.role,
                replica_suffix: entry.replica_suffix,
                display_name: entry.display_name,
                status: match entry.status {
                    crate::agent_registry::AgentStatus::Active => AgentRosterStatus::Active,
                    crate::agent_registry::AgentStatus::Idle => AgentRosterStatus::Idle,
                    crate::agent_registry::AgentStatus::Working => AgentRosterStatus::Working,
                    crate::agent_registry::AgentStatus::Blocked => AgentRosterStatus::Blocked,
                    crate::agent_registry::AgentStatus::Done => AgentRosterStatus::Done,
                    crate::agent_registry::AgentStatus::Unknown => AgentRosterStatus::Unknown,
                    crate::agent_registry::AgentStatus::Terminated => AgentRosterStatus::Terminated,
                },
                last_pane: entry.last_pane,
                last_seen_at: entry.last_seen_at,
            })
            .collect();
        encode_success(id, ResponseResult::AgentRosterList { entries })
    }

    pub(super) fn handle_agent_revive(&mut self, id: String, params: AgentReviveParams) -> String {
        match self.revive_agent(
            params.instance_id,
            params.tab_id,
            params.cwd_mode,
            params.timeout_ms,
            params.args,
        ) {
            Ok(crate::app::agents::AgentSpawnOutcome::Spawned(revived)) => {
                encode_success(id, ResponseResult::AgentRevived { revived: *revived })
            }
            Ok(crate::app::agents::AgentSpawnOutcome::Pending(pending)) => {
                let pane_id = pending.pane_id().to_string();
                self.cancel_pending_agent_spawn(*pending);
                encode_error(
                    id,
                    "agent_revive_failed",
                    format!("agent pane {pane_id} busy"),
                )
            }
            Err(err) => encode_error_body(id, self.agent_spawn_error_body(err)),
        }
    }

    pub(crate) fn handle_deferred_agent_spawn_api_request(
        &mut self,
        id: String,
        params: AgentSpawnParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) -> bool {
        match self.spawn_agent(params) {
            Ok(crate::app::agents::AgentSpawnOutcome::Spawned(spawned)) => {
                let _ = respond_to.send(encode_success(
                    id,
                    ResponseResult::AgentSpawned { spawned: *spawned },
                ));
            }
            Ok(crate::app::agents::AgentSpawnOutcome::Pending(pending)) => {
                self.pending_agent_spawns
                    .push(crate::app::agents::DeferredAgentSpawn {
                        id,
                        respond_to,
                        pending: *pending,
                        revived: false,
                    });
                self.sync_pending_agent_spawn_deadline();
            }
            Err(err) => {
                let _ = respond_to.send(encode_error_body(id, self.agent_spawn_error_body(err)));
            }
        }
        true
    }

    pub(crate) fn handle_deferred_agent_revive_api_request(
        &mut self,
        id: String,
        params: AgentReviveParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) -> bool {
        match self.revive_agent(
            params.instance_id,
            params.tab_id,
            params.cwd_mode,
            params.timeout_ms,
            params.args,
        ) {
            Ok(crate::app::agents::AgentSpawnOutcome::Spawned(revived)) => {
                let _ = respond_to.send(encode_success(
                    id,
                    ResponseResult::AgentRevived { revived: *revived },
                ));
            }
            Ok(crate::app::agents::AgentSpawnOutcome::Pending(pending)) => {
                self.pending_agent_spawns
                    .push(crate::app::agents::DeferredAgentSpawn {
                        id,
                        respond_to,
                        pending: *pending,
                        revived: true,
                    });
                self.sync_pending_agent_spawn_deadline();
            }
            Err(err) => {
                let _ = respond_to.send(encode_error_body(id, self.agent_spawn_error_body(err)));
            }
        }
        true
    }

    pub(crate) fn complete_due_agent_spawns(&mut self, now: Instant) -> bool {
        let completions = self.poll_pending_agent_spawns(now);
        if completions.is_empty() {
            return false;
        }

        for completion in completions {
            let response = match completion.result {
                Ok(spawned) if completion.revived => encode_success(
                    completion.id,
                    ResponseResult::AgentRevived { revived: spawned },
                ),
                Ok(spawned) => {
                    encode_success(completion.id, ResponseResult::AgentSpawned { spawned })
                }
                Err(err) => encode_error_body(completion.id, self.agent_spawn_error_body(err)),
            };
            let _ = completion.respond_to.send(response);
        }
        true
    }

    pub(super) fn handle_agent_profile_set_md(
        &mut self,
        id: String,
        params: AgentProfileSetMdParams,
    ) -> String {
        match self.set_profile_md(&params.role, &params.name, params.path.as_deref()) {
            Ok(profile) => encode_success(id, ResponseResult::AgentProfileInfoSet { profile }),
            Err(err) => encode_error_body(id, self.agent_profile_error_body(err)),
        }
    }

    pub(super) fn handle_agent_profile_create(
        &mut self,
        id: String,
        params: AgentProfileCreateParams,
    ) -> String {
        match self.create_profile(params) {
            Ok(profile) => encode_success(id, ResponseResult::AgentProfileInfo { profile }),
            Err(err) => encode_error_body(id, self.agent_profile_error_body(err)),
        }
    }

    pub(super) fn handle_agent_profile_get(
        &mut self,
        id: String,
        params: crate::api::schema::AgentProfileGetParams,
    ) -> String {
        match self.profile(&params.role) {
            Ok(profile) => encode_success(id, ResponseResult::AgentProfileInfo { profile }),
            Err(err) => encode_error_body(id, self.agent_profile_error_body(err)),
        }
    }

    pub(super) fn handle_agent_profile_list(&mut self, id: String) -> String {
        match self.profiles() {
            Ok(profiles) => encode_success(id, ResponseResult::AgentProfileList { profiles }),
            Err(err) => encode_error_body(id, self.agent_profile_error_body(err)),
        }
    }

    pub(super) fn handle_agent_profile_set(
        &mut self,
        id: String,
        params: crate::api::schema::AgentProfileSetParams,
    ) -> String {
        match self.set_profile(params) {
            Ok(profile) => encode_success(id, ResponseResult::AgentProfileInfo { profile }),
            Err(err) => encode_error_body(id, self.agent_profile_error_body(err)),
        }
    }

    pub(super) fn handle_agent_prompt(&mut self, id: String, params: AgentPromptParams) -> String {
        if params.text.is_empty() {
            return encode_error(id, "empty_agent_prompt", "agent prompt must not be empty");
        }
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        match self.send_agent_prompt(&resolved, &params.target, &params.text) {
            Ok(agent) => encode_success(id, ResponseResult::AgentPrompted { agent }),
            Err(error) => encode_error_body(id, error),
        }
    }

    pub(super) fn handle_agent_broadcast(
        &mut self,
        id: String,
        params: AgentBroadcastParams,
    ) -> String {
        if params.text.is_empty() {
            return encode_error(id, "empty_agent_prompt", "agent prompt must not be empty");
        }
        let targets = self.agent_targets_for_role(&params.role);
        if targets.is_empty() {
            return agent_not_found(id, &params.role);
        }

        // Validate all destinations before submitting to any of them. A role
        // broadcast must never silently skip a blocked or stale replica.
        for target in &targets {
            if let Err(error) = self.prepare_agent_prompt(target, &params.role) {
                return encode_error_body(id, error);
            }
        }
        let agents = targets
            .iter()
            .map(|target| self.send_agent_prompt(target, &params.role, &params.text))
            .collect::<Result<Vec<_>, _>>();
        match agents {
            Ok(agents) => encode_success(id, ResponseResult::AgentBroadcast { agents }),
            Err(error) => encode_error_body(id, error),
        }
    }

    fn prepare_agent_prompt(
        &self,
        resolved: &crate::app::terminal_targets::TerminalTarget,
        target: &str,
    ) -> Result<
        (&crate::terminal::TerminalRuntime, crate::detect::Agent),
        crate::api::schema::ErrorBody,
    > {
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .cloned()
        else {
            return Err(self.agent_target_error_body(
                crate::app::terminal_targets::TerminalTargetError::NotFound {
                    target: target.to_string(),
                },
            ));
        };
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return Err(self.agent_target_error_body(
                crate::app::terminal_targets::TerminalTargetError::NotFound {
                    target: target.to_string(),
                },
            ));
        };
        if terminal.state == crate::detect::AgentState::Blocked {
            return Err(crate::api::schema::ErrorBody {
                code: "agent_blocked".into(),
                message: format!("agent {target} is blocked and requires interactive input"),
            });
        }
        let Some(expected_agent) = terminal.effective_known_agent() else {
            return Err(crate::api::schema::ErrorBody {
                code: "agent_not_ready".into(),
                message: format!("agent {target} is not ready for prompts"),
            });
        };
        if terminal.managed_agent_launch_pending() {
            return Err(crate::api::schema::ErrorBody {
                code: "agent_not_ready".into(),
                message: format!("agent {target} is not ready for prompts"),
            });
        }
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return Err(self.agent_target_error_body(
                crate::app::terminal_targets::TerminalTargetError::NotFound {
                    target: target.to_string(),
                },
            ));
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return Err(crate::api::schema::ErrorBody {
                code: "agent_not_ready".into(),
                message: format!("agent {target} is no longer the pane foreground process"),
            });
        }
        Ok((runtime, expected_agent))
    }

    fn send_agent_prompt(
        &self,
        resolved: &crate::app::terminal_targets::TerminalTarget,
        target: &str,
        prompt: &str,
    ) -> Result<crate::api::schema::AgentInfo, crate::api::schema::ErrorBody> {
        let (runtime, expected_agent) = self.prepare_agent_prompt(resolved, target)?;
        if expected_agent == crate::detect::Agent::GithubCopilot {
            // Copilot ignores synthetic Enter after focus loss until it receives focus gained.
            let focus = match crate::ghostty::encode_focus(crate::ghostty::FocusEvent::Gained) {
                Ok(focus) => focus,
                Err(err) => {
                    return Err(crate::api::schema::ErrorBody {
                        code: "agent_prompt_failed".into(),
                        message: err.to_string(),
                    })
                }
            };
            if let Err(err) = runtime.try_send_bytes(Bytes::from(focus)) {
                return Err(crate::api::schema::ErrorBody {
                    code: "agent_prompt_failed".into(),
                    message: err.to_string(),
                });
            }
        }
        let (text, enter) = crate::app::api_helpers::encode_api_submission_parts(runtime, prompt);
        if let Err(err) = runtime.try_send_bytes(Bytes::from(text)) {
            return Err(crate::api::schema::ErrorBody {
                code: "agent_prompt_failed".into(),
                message: err.to_string(),
            });
        }
        runtime.send_bytes_after(Bytes::from(enter), AGENT_PROMPT_SUBMIT_DELAY);
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| {
                self.agent_target_error_body(
                    crate::app::terminal_targets::TerminalTargetError::NotFound {
                        target: target.to_string(),
                    },
                )
            })
    }

    pub(super) fn handle_agent_read(
        &mut self,
        id: String,
        params: crate::api::schema::AgentReadParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &params.target);
        };
        let snapshot = crate::app::api_helpers::read_terminal_snapshot(
            pane,
            params.source,
            params.format,
            params.lines,
        );

        encode_success(
            id,
            ResponseResult::PaneRead {
                read: PaneReadResult {
                    pane_id: self
                        .public_pane_id(resolved.ws_idx, resolved.pane_id)
                        .unwrap_or_else(|| params.target.clone()),
                    workspace_id,
                    tab_id: self
                        .public_tab_id(resolved.ws_idx, resolved.tab_idx)
                        .unwrap(),
                    source: params.source,
                    format: params.format,
                    text: snapshot.text,
                    revision: 0,
                    truncated: snapshot.truncated,
                },
            },
        )
    }

    pub(super) fn handle_agent_explain(&mut self, id: String, target: AgentTarget) -> String {
        let resolved = match self.resolve_agent_target(&target.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, _workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return agent_not_found(id, &target.target);
        };
        if terminal.full_lifecycle_hook_authority_active() {
            let explain = serde_json::json!({
                "agent": terminal.effective_agent_label().unwrap_or("unknown"),
                "state": crate::detect::manifest::agent_state_label(terminal.state),
                "manifest_source": null,
                "manifest_version": null,
                "cached_remote_version": null,
                "local_override_shadowing_remote": false,
                "remote_update_status": null,
                "remote_update_error": null,
                "matched_rule": null,
                "visible_idle": false,
                "visible_blocker": false,
                "visible_working": false,
                "screen_detection_skipped": true,
                "screen_detection_skip_reason": "full_lifecycle_hook_authority",
                "skip_state_update": false,
                "skipped_update_reason": null,
                "fallback_reason": null,
                "warning": null,
                "evaluated_rules": [],
            });
            return encode_success(id, ResponseResult::AgentExplain { explain });
        }
        let Some(agent) = terminal.effective_known_agent().or(terminal.detected_agent) else {
            return encode_error(
                id,
                "agent_explain_unavailable",
                format!(
                    "agent target {} does not have a detected agent label",
                    target.target
                ),
            );
        };

        let screen = pane.detection_text();
        let osc_title = pane.agent_osc_title();
        let osc_progress = pane.agent_osc_progress();
        let explain = crate::detect::manifest::explain_with_input(
            agent,
            crate::detect::manifest::DetectionInput {
                screen: &screen,
                osc_title: &osc_title,
                osc_progress: &osc_progress,
            },
        );
        let value = crate::detect::manifest::explain_to_json_value(&explain);

        encode_success(id, ResponseResult::AgentExplain { explain: value })
    }

    pub(super) fn handle_agent_send_keys(
        &mut self,
        id: String,
        params: AgentSendKeysParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(id, &params.target);
        };
        let Some(expected_agent) = self
            .state
            .terminals
            .get(terminal_id)
            .and_then(|terminal| terminal.effective_known_agent())
        else {
            return agent_not_ready(id, &params.target);
        };
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return agent_not_found(id, &params.target);
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return agent_not_ready(id, &params.target);
        }
        let encoded = match super::super::api_helpers::encode_api_keys(runtime, &params.keys) {
            Ok(encoded) => encoded,
            Err(key) => {
                return encode_error(id, "invalid_key", format!("unsupported key {key}"));
            }
        };
        let bytes: Vec<u8> = encoded.into_iter().flatten().collect();
        if let Err(err) = runtime.try_send_bytes(Bytes::from(bytes)) {
            return encode_error(id, "agent_send_keys_failed", err.to_string());
        }

        encode_success(id, ResponseResult::Ok {})
    }
}

fn agent_not_ready(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_ready",
        format!("agent {target} is not an active named agent"),
    )
}

fn agent_not_found(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_found",
        format!("agent target {target} not found"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::schema::{AgentStatus, SuccessResponse},
        app::Mode,
        config::Config,
        detect::{Agent, AgentState},
        workspace::Workspace,
    };

    fn app_with_agent() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("agent")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app
    }

    #[tokio::test]
    async fn agent_prompt_sends_text_then_delays_enter() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Working);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 1,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        let bracketed_started = std::time::Instant::now();
        let response = app.handle_agent_prompt(
            "req".into(),
            AgentPromptParams {
                target: public_pane_id,
                text: "A != B".into(),
                wait: None,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentPrompted { agent, .. } = success.result else {
            panic!("expected prompted response");
        };
        assert_eq!(agent.name.as_deref(), Some("reviewer"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap(),
            Bytes::from_static(b"\r")
        );
        assert!(bracketed_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        app.lookup_runtime_sender(0, pane_id)
            .unwrap()
            .test_process_pty_bytes(b"\x1b[?2004l");
        let raw_started = std::time::Instant::now();
        let raw = app.handle_agent_prompt(
            "req-raw".into(),
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let raw: SuccessResponse = serde_json::from_str(&raw).unwrap();
        assert!(matches!(raw.result, ResponseResult::AgentPrompted { .. }));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"A != B"));
        assert!(rx.try_recv().is_err());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap(),
            Bytes::from_static(b"\r")
        );
        assert!(raw_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        let rejected = app.handle_agent_prompt(
            "req-label".into(),
            AgentPromptParams {
                target: "opencode".into(),
                text: "wrong target".into(),
                wait: None,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_not_found");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn role_broadcast_reaches_every_replica_and_role_pane_selects_one() {
        let mut app = app_with_agent();
        let first_pane = app.state.workspaces[0].tabs[0].root_pane;
        let second_pane =
            app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        app.state.ensure_test_terminals();

        let mut receivers = Vec::new();
        for (pane_id, name, suffix) in [
            (first_pane, "reviewer", ""),
            (second_pane, "reviewer-replica-1", "-replica-1"),
        ] {
            let terminal_id = app.state.terminal_id_for_pane(0, pane_id).unwrap();
            let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
            terminal.set_agent_name(name.into());
            terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
            let (runtime, receiver) =
                crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                    80, 24, 0, b"", 2,
                );
            runtime.test_process_pty_bytes(b"\x1b[?2004h");
            app.state.insert_test_runtime(pane_id, runtime);
            let pane = app.public_pane_id(0, pane_id).unwrap();
            app.agent_registry
                .register_or_get("reviewer", std::path::PathBuf::from("/tmp"));
            app.agent_registry
                .roster_register(name, "reviewer", "reviewer", suffix, Some(pane));
            receivers.push(receiver);
        }

        let response = app.handle_agent_broadcast(
            "broadcast".into(),
            AgentBroadcastParams {
                role: "reviewer".into(),
                text: "check this".into(),
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentBroadcast { agents } = success.result else {
            panic!("expected broadcast response");
        };
        assert_eq!(agents.len(), 2);
        for receiver in &mut receivers {
            assert_eq!(
                receiver.try_recv().unwrap(),
                Bytes::from_static(b"\x1b[200~check this\x1b[201~")
            );
        }

        let ambiguous = app.handle_agent_prompt(
            "ambiguous".into(),
            AgentPromptParams {
                target: "reviewer".into(),
                text: "one only".into(),
                wait: None,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&ambiguous).unwrap();
        assert_eq!(error.error.code, "agent_target_ambiguous");

        let second_pane_id = app.public_pane_id(0, second_pane).unwrap();
        let selected = app.handle_agent_prompt(
            "selected".into(),
            AgentPromptParams {
                target: format!("reviewer@{second_pane_id}"),
                text: "one only".into(),
                wait: None,
            },
        );
        let selected: SuccessResponse = serde_json::from_str(&selected).unwrap();
        assert!(matches!(
            selected.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(
            receivers[1].try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~one only\x1b[201~")
        );
    }

    #[tokio::test]
    async fn agent_prompt_rejects_blocked_agent_without_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Blocked);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = app.handle_agent_prompt(
            "req".into(),
            AgentPromptParams {
                target: "reviewer".into(),
                text: "unrelated prompt".into(),
                wait: None,
            },
        );

        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_blocked");
        assert!(
            tokio::time::timeout(
                AGENT_PROMPT_SUBMIT_DELAY + Duration::from_millis(100),
                rx.recv()
            )
            .await
            .is_err(),
            "blocked prompt wrote or scheduled terminal input"
        );
    }

    #[tokio::test]
    async fn agent_prompt_focuses_copilot_before_submitting() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Idle);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 3,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let response = app.handle_agent_prompt(
            "req".into(),
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[I"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap(),
            Bytes::from_static(b"\r")
        );
    }

    #[tokio::test]
    async fn agent_send_keys_validates_every_key_before_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let rejected = app.handle_agent_send_keys(
            "req-invalid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["enter".into(), "not-a-key".into()],
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "invalid_key");
        assert!(rx.try_recv().is_err());

        let sent = app.handle_agent_send_keys(
            "req-valid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["up".into(), "enter".into()],
            },
        );
        let success: SuccessResponse = serde_json::from_str(&sent).unwrap();
        assert!(matches!(success.result, ResponseResult::Ok {}));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[A\r"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_rejects_managed_agent_while_startup_is_pending() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        let now = std::time::Instant::now();
        terminal.begin_managed_agent(
            "reviewer".into(),
            Agent::OpenCode,
            None,
            now,
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(10),
        );
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = app.handle_agent_prompt(
            "req-pending".into(),
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_not_ready");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn agent_focus_marks_already_focused_done_agent_seen() {
        let mut app = app_with_agent();
        app.state.outer_terminal_focus = Some(false);

        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Pi), AgentState::Idle);
        app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&pane_id)
            .unwrap()
            .seen = false;
        app.state.workspaces[0].tabs[0].layout.focus_pane(pane_id);

        let response = app.handle_agent_focus(
            "req".into(),
            AgentTarget {
                target: app.public_pane_id(0, pane_id).unwrap(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentInfo { agent } = success.result else {
            panic!("expected agent info response");
        };
        assert_eq!(agent.agent_status, AgentStatus::Idle);
    }

    #[test]
    fn agent_rename_does_not_replace_the_pane_label() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_manual_label("shell-pane".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let target = app.public_pane_id(0, pane_id).unwrap();

        for name in [Some("reviewer".to_string()), None] {
            let response = app.handle_agent_rename(
                "req".into(),
                AgentRenameParams {
                    target: target.clone(),
                    name,
                },
            );
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            assert!(matches!(success.result, ResponseResult::AgentInfo { .. }));
            assert_eq!(
                app.state.terminals[&terminal_id].manual_label.as_deref(),
                Some("shell-pane")
            );
        }
    }
}
