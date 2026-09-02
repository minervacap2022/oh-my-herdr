use std::process::Stdio;

use super::manifest::{effective_platforms, ensure_platform_supported};
use super::plugin_manifest_available;
use crate::api::schema::{
    InstalledPluginInfo, PluginCommandLogInfo, PluginCommandStatus, PluginInvocationContext,
};
use crate::app::App;
pub(super) use crate::plugin_command::read_capped_plugin_output;
use crate::plugin_command::{
    current_unix_ms, PluginCommandRunnerResult, PluginCommandRunnerSpec,
    PLUGIN_COMMAND_OUTPUT_MAX_BYTES,
};

pub(super) const MAX_PLUGIN_COMMANDS_IN_FLIGHT: usize = 32;
const PLUGIN_COMMAND_LOG_LIMIT: usize = 200;

impl App {
    pub(super) fn start_plugin_command(
        &mut self,
        plugin: &InstalledPluginInfo,
        action_id: Option<String>,
        event: Option<String>,
        command: Vec<String>,
        context: &PluginInvocationContext,
        event_json: Option<String>,
    ) -> Result<PluginCommandLogInfo, (&'static str, String)> {
        let Some(program) = command.first().cloned() else {
            return Err((
                "invalid_plugin_command",
                "command must not be empty".to_string(),
            ));
        };
        let args = command.iter().skip(1).cloned().collect::<Vec<_>>();
        let context_json = serde_json::to_string(context)
            .map_err(|err| ("invalid_plugin_context", err.to_string()))?;
        super::env::ensure_plugin_user_dirs(plugin)
            .map_err(|err| ("plugin_user_dir_create_failed", err.to_string()))?;
        let log_id = format!("plugin-log-{}", self.state.next_plugin_command_log_id);
        self.state.next_plugin_command_log_id += 1;
        let started_unix_ms = current_unix_ms();
        let mut env = super::env::plugin_path_env(plugin);
        env.extend([
            (
                crate::api::SOCKET_PATH_ENV_VAR.to_string(),
                crate::api::socket_path().display().to_string(),
            ),
            ("HERDR_ENV".to_string(), "1".to_string()),
            ("HERDR_PLUGIN_ID".to_string(), plugin.plugin_id.clone()),
            ("HERDR_PLUGIN_CONTEXT_JSON".to_string(), context_json),
        ]);
        if let Ok(current_exe) = std::env::current_exe() {
            env.push((
                "HERDR_BIN_PATH".to_string(),
                current_exe.display().to_string(),
            ));
        }
        if let Some(action_id) = action_id.as_ref() {
            env.push(("HERDR_PLUGIN_ACTION_ID".to_string(), action_id.clone()));
        }
        if let Some(event) = event.as_ref() {
            env.push(("HERDR_PLUGIN_EVENT".to_string(), event.clone()));
        }
        if let Some(event_json) = event_json {
            env.push(("HERDR_PLUGIN_EVENT_JSON".to_string(), event_json));
        }
        if let Some(workspace_id) = context.workspace_id.as_ref() {
            env.push(("HERDR_WORKSPACE_ID".to_string(), workspace_id.clone()));
        }
        if let Some(tab_id) = context.tab_id.as_ref() {
            env.push(("HERDR_TAB_ID".to_string(), tab_id.clone()));
        }
        if let Some(pane_id) = context.focused_pane_id.as_ref() {
            env.push(("HERDR_PANE_ID".to_string(), pane_id.clone()));
        }
        if let Some(clicked_url) = context.clicked_url.as_ref() {
            env.push(("HERDR_PLUGIN_CLICKED_URL".to_string(), clicked_url.clone()));
        }
        if let Some(link_handler_id) = context.link_handler_id.as_ref() {
            env.push((
                "HERDR_PLUGIN_LINK_HANDLER_ID".to_string(),
                link_handler_id.clone(),
            ));
        }
        let plugin_root =
            crate::worktree::canonical_or_original(std::path::Path::new(&plugin.plugin_root));
        let log = PluginCommandLogInfo {
            log_id: log_id.clone(),
            plugin_id: plugin.plugin_id.clone(),
            action_id,
            event,
            command: command.clone(),
            status: PluginCommandStatus::Running,
            started_unix_ms,
            finished_unix_ms: None,
            exit_code: None,
            stdout: None,
            stderr: None,
            error: None,
        };
        if self.no_session && self.state.plugin_commands_in_flight >= MAX_PLUGIN_COMMANDS_IN_FLIGHT
        {
            return self.reject_plugin_command(
                log,
                "plugin_command_limit_reached",
                format!(
                    "maximum concurrent plugin commands reached ({MAX_PLUGIN_COMMANDS_IN_FLIGHT})"
                ),
            );
        }

        if !self.no_session {
            let lease_id = match crate::persist::plugin_command_leases::acquire(
                plugin_root.clone(),
                MAX_PLUGIN_COMMANDS_IN_FLIGHT,
            ) {
                Ok(lease_id) => lease_id,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    return self.reject_plugin_command(
                        log,
                        "plugin_command_limit_reached",
                        format!(
                            "maximum concurrent plugin commands reached ({MAX_PLUGIN_COMMANDS_IN_FLIGHT})"
                        ),
                    );
                }
                Err(err) => {
                    return self.reject_plugin_command(
                        log,
                        "plugin_command_lease_failed",
                        format!("failed to reserve plugin command lease: {err}"),
                    );
                }
            };
            let runner = crate::plugin_command::spawn_runner(&PluginCommandRunnerSpec {
                lease_id: lease_id.clone(),
                program,
                args,
                cwd: plugin_root,
                env,
            });
            let runner = match runner {
                Ok(runner) => runner,
                Err(err) => {
                    if let Err(release_err) =
                        crate::persist::plugin_command_leases::release(&lease_id)
                    {
                        tracing::error!(
                            lease_id,
                            err = %release_err,
                            "failed to release plugin command lease after runner launch failure"
                        );
                    }
                    return self.reject_plugin_command(
                        log,
                        "plugin_command_start_failed",
                        format!("failed to start plugin command runner: {err}"),
                    );
                }
            };
            self.push_plugin_command_log(log.clone());
            self.state.plugin_commands_in_flight += 1;
            let event_tx = self.event_tx.clone();
            std::thread::spawn(move || {
                let finished = match runner.wait_with_output() {
                    Ok(output) => {
                        match serde_json::from_slice::<PluginCommandRunnerResult>(&output.stdout) {
                            Ok(result) => crate::events::AppEvent::PluginCommandFinished {
                                log_id,
                                finished_unix_ms: result.finished_unix_ms,
                                exit_code: result.exit_code,
                                stdout: result.stdout,
                                stderr: result.stderr,
                                error: result.error,
                            },
                            Err(err) => crate::events::AppEvent::PluginCommandFinished {
                                log_id,
                                finished_unix_ms: current_unix_ms(),
                                exit_code: output.status.code(),
                                stdout: String::new(),
                                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                                error: Some(format!(
                                    "plugin command runner returned an invalid result: {err}"
                                )),
                            },
                        }
                    }
                    Err(err) => crate::events::AppEvent::PluginCommandFinished {
                        log_id,
                        finished_unix_ms: current_unix_ms(),
                        exit_code: None,
                        stdout: String::new(),
                        stderr: String::new(),
                        error: Some(format!("failed to wait for plugin command runner: {err}")),
                    },
                };
                let _ = event_tx.blocking_send(finished);
            });
            return Ok(log);
        }

        self.push_plugin_command_log(log.clone());
        self.state.plugin_commands_in_flight += 1;
        self.acquire_plugin_command_root_lease(log_id.clone(), plugin_root.clone());
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let child = crate::plugin_command::command_for_plugin_argv_in_dir(
                &program,
                &args,
                &plugin_root,
                &env,
            )
            .envs(env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
            let finished = match child {
                Ok(mut child) => {
                    let stdout = child.stdout.take();
                    let stderr = child.stderr.take();
                    let stdout_reader = stdout.map(|stdout| {
                        std::thread::spawn(move || {
                            read_capped_plugin_output(stdout, PLUGIN_COMMAND_OUTPUT_MAX_BYTES)
                        })
                    });
                    let stderr_reader = stderr.map(|stderr| {
                        std::thread::spawn(move || {
                            read_capped_plugin_output(stderr, PLUGIN_COMMAND_OUTPUT_MAX_BYTES)
                        })
                    });
                    match child.wait() {
                        Ok(status) => crate::events::AppEvent::PluginCommandFinished {
                            log_id,
                            finished_unix_ms: current_unix_ms(),
                            exit_code: status.code(),
                            stdout: stdout_reader
                                .and_then(|reader| reader.join().ok())
                                .unwrap_or_default(),
                            stderr: stderr_reader
                                .and_then(|reader| reader.join().ok())
                                .unwrap_or_default(),
                            error: None,
                        },
                        Err(err) => crate::events::AppEvent::PluginCommandFinished {
                            log_id,
                            finished_unix_ms: current_unix_ms(),
                            exit_code: None,
                            stdout: stdout_reader
                                .and_then(|reader| reader.join().ok())
                                .unwrap_or_default(),
                            stderr: stderr_reader
                                .and_then(|reader| reader.join().ok())
                                .unwrap_or_default(),
                            error: Some(err.to_string()),
                        },
                    }
                }
                Err(err) => crate::events::AppEvent::PluginCommandFinished {
                    log_id,
                    finished_unix_ms: current_unix_ms(),
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(err.to_string()),
                },
            };
            let _ = event_tx.blocking_send(finished);
        });
        Ok(log)
    }

    fn acquire_plugin_command_root_lease(
        &mut self,
        log_id: String,
        plugin_root: std::path::PathBuf,
    ) {
        let count = self
            .state
            .plugin_command_root_leases
            .entry(plugin_root.clone())
            .or_default();
        *count = count.saturating_add(1);
        self.state
            .plugin_command_lease_roots
            .insert(log_id, plugin_root);
    }

    pub(crate) fn release_plugin_command_root_lease(&mut self, log_id: &str) {
        let Some(plugin_root) = self.state.plugin_command_lease_roots.remove(log_id) else {
            return;
        };
        let Some(count) = self.state.plugin_command_root_leases.get_mut(&plugin_root) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.state.plugin_command_root_leases.remove(&plugin_root);
        }
    }

    fn reject_plugin_command(
        &mut self,
        mut log: PluginCommandLogInfo,
        code: &'static str,
        message: String,
    ) -> Result<PluginCommandLogInfo, (&'static str, String)> {
        log.status = PluginCommandStatus::Failed;
        log.finished_unix_ms = Some(log.started_unix_ms);
        log.stdout = Some(String::new());
        log.stderr = Some(String::new());
        log.error = Some(message.clone());
        self.push_plugin_command_log(log);
        Err((code, message))
    }

    pub(crate) fn leased_plugin_root_within(
        &self,
        checkout_path: &std::path::Path,
    ) -> std::io::Result<Option<std::path::PathBuf>> {
        if !self.no_session {
            return crate::persist::plugin_command_leases::active_root_within(checkout_path);
        }
        let checkout_path = crate::worktree::canonical_or_original(checkout_path);
        Ok(self
            .state
            .plugin_command_root_leases
            .iter()
            .find_map(|(plugin_root, count)| {
                (*count > 0 && plugin_root.starts_with(&checkout_path)).then(|| plugin_root.clone())
            }))
    }

    pub(crate) fn run_plugin_startup_hooks(&mut self) {
        let mut context = self.current_plugin_context("plugin.startup");
        context.invocation_source = Some("startup".to_string());
        let mut plugins = self
            .state
            .installed_plugins
            .values()
            .filter(|plugin| {
                plugin.enabled && plugin_manifest_available(plugin) && !plugin.startup.is_empty()
            })
            .cloned()
            .collect::<Vec<_>>();
        plugins.sort_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
        for plugin in plugins {
            for startup in plugin.startup.clone() {
                if ensure_platform_supported(
                    &effective_platforms(&startup.platforms, &plugin.platforms).clone(),
                    "startup",
                )
                .is_err()
                {
                    continue;
                }
                let _ = self.start_plugin_command(
                    &plugin,
                    None,
                    Some("startup".to_string()),
                    startup.command,
                    &context,
                    None,
                );
            }
        }
    }

    pub(crate) fn run_plugin_event_hooks(&mut self, event: &crate::api::schema::EventEnvelope) {
        let event_name = event.event.dot_name();
        if !crate::api::schema::PLUGIN_HOOK_EVENT_KINDS.contains(&event.event) {
            return;
        }
        if let Err(err) = self.refresh_installed_plugins() {
            tracing::warn!(err = %err, "failed to refresh plugin registry before event hooks");
            return;
        }
        let plugins = self
            .state
            .installed_plugins
            .values()
            .filter(|plugin| {
                plugin.enabled
                    && plugin_manifest_available(plugin)
                    && plugin.events.iter().any(|hook| hook.on == event_name)
            })
            .cloned()
            .collect::<Vec<_>>();
        if plugins.is_empty() {
            return;
        }
        let event_json = serde_json::to_string(event).ok();
        let context = self.plugin_context_for_event(event, event_name);
        for plugin in plugins {
            for hook in plugin.events.clone() {
                if hook.on != event_name {
                    continue;
                }
                if ensure_platform_supported(
                    &effective_platforms(&hook.platforms, &plugin.platforms).clone(),
                    event_name,
                )
                .is_err()
                {
                    continue;
                }
                let _ = self.start_plugin_command(
                    &plugin,
                    None,
                    Some(event_name.to_string()),
                    hook.command.clone(),
                    &context,
                    event_json.clone(),
                );
            }
        }
    }

    fn push_plugin_command_log(&mut self, log: PluginCommandLogInfo) {
        self.state.plugin_command_logs.push(log);
        if self.state.plugin_command_logs.len() > PLUGIN_COMMAND_LOG_LIMIT {
            let extra = self.state.plugin_command_logs.len() - PLUGIN_COMMAND_LOG_LIMIT;
            self.state.plugin_command_logs.drain(0..extra);
        }
    }
}
