use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use regex::Regex;

use crate::api::schema::{
    ErrorBody, ErrorResponse, EventData, EventEnvelope, EventKind, EventMatch, EventsWaitParams,
    Method, Request, ResponseResult, Subscription, SubscriptionEventData,
    SubscriptionEventEnvelope, SuccessResponse,
};
use crate::api::server::{
    dispatch_to_app_with_timeout, dispatch_to_app_with_timeout_until_connection_stops,
    should_stop_connection, APP_RESPONSE_TIMEOUT, CONNECTION_POLL_INTERVAL,
};
use crate::api::subscriptions::{
    match_output, output_match_read_source, ActiveSubscription, WaitSubscriptionInit,
    WaitSubscriptionPoll,
};
use crate::api::{ApiRequestSender, EventHub};
use crate::ipc::LocalStream;

const AGENT_PROMPT_EFFECT_TIMEOUT_MS: u64 = 5_000;

#[derive(Clone, Copy)]
struct WaitDeadline {
    started: std::time::Instant,
    timeout: Option<std::time::Duration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitBudget {
    Unlimited,
    Expired,
    Remaining(std::time::Duration),
}

impl WaitDeadline {
    fn from_timeout_ms(timeout_ms: Option<u64>) -> Self {
        Self {
            started: std::time::Instant::now(),
            timeout: timeout_ms.map(std::time::Duration::from_millis),
        }
    }

    fn budget_at(self, now: std::time::Instant) -> WaitBudget {
        match self.timeout {
            None => WaitBudget::Unlimited,
            Some(timeout) => match timeout.checked_sub(now.saturating_duration_since(self.started))
            {
                Some(remaining) if !remaining.is_zero() => WaitBudget::Remaining(remaining),
                _ => WaitBudget::Expired,
            },
        }
    }

    fn app_timeout_at(self, now: std::time::Instant) -> Option<std::time::Duration> {
        match self.budget_at(now) {
            WaitBudget::Unlimited => Some(APP_RESPONSE_TIMEOUT),
            WaitBudget::Remaining(remaining) => Some(remaining.min(APP_RESPONSE_TIMEOUT)),
            WaitBudget::Expired => None,
        }
    }

    fn sleep_at(self, now: std::time::Instant) -> Option<std::time::Duration> {
        match self.budget_at(now) {
            WaitBudget::Unlimited => Some(CONNECTION_POLL_INTERVAL),
            WaitBudget::Remaining(remaining) => Some(remaining.min(CONNECTION_POLL_INTERVAL)),
            WaitBudget::Expired => None,
        }
    }

    fn capped_to(self, timeout: std::time::Duration) -> Self {
        Self {
            timeout: Some(self.timeout.map_or(timeout, |current| current.min(timeout))),
            ..self
        }
    }
}

fn output_wait_timeout(request_id: &str, pane_id: &str) -> std::io::Result<Option<String>> {
    crate::logging::api_wait_timed_out(request_id, pane_id);
    Ok(Some(
        serde_json::to_string(&ErrorResponse {
            id: request_id.into(),
            error: ErrorBody {
                code: "timeout".into(),
                message: "timed out waiting for output match".into(),
            },
        })
        .unwrap(),
    ))
}

pub(super) fn wait_for_output(
    request_id: String,
    params: crate::api::schema::PaneWaitForOutputParams,
    stream: &mut LocalStream,
    api_tx: &ApiRequestSender,
    running: &Arc<AtomicBool>,
) -> std::io::Result<Option<String>> {
    crate::logging::api_wait_started(&request_id, &params.pane_id, params.timeout_ms);
    let deadline = WaitDeadline::from_timeout_ms(params.timeout_ms);

    let regex = match &params.r#match {
        crate::api::schema::OutputMatch::Regex { value } => match Regex::new(value) {
            Ok(regex) => Some(regex),
            Err(err) => {
                return Ok(Some(
                    serde_json::to_string(&ErrorResponse {
                        id: request_id,
                        error: ErrorBody {
                            code: "invalid_regex".into(),
                            message: err.to_string(),
                        },
                    })
                    .unwrap(),
                ));
            }
        },
        crate::api::schema::OutputMatch::Substring { .. } => None,
    };

    loop {
        if should_stop_connection(stream, running)? {
            crate::logging::api_wait_completed(&request_id, &params.pane_id, "client_disconnected");
            return Ok(None);
        }

        let now = std::time::Instant::now();
        let Some(app_timeout) = deadline.app_timeout_at(now) else {
            return output_wait_timeout(&request_id, &params.pane_id);
        };
        let read_request = Request {
            id: format!("{request_id}:read"),
            method: Method::PaneRead(crate::api::schema::PaneReadParams {
                pane_id: params.pane_id.clone(),
                source: output_match_read_source(&params.source),
                lines: params.lines,
                format: crate::api::schema::ReadFormat::Text,
                strip_ansi: params.strip_ansi,
                intent: crate::api::schema::ReadIntent::Passive,
            }),
        };
        let Some(response) = dispatch_to_app_with_timeout_until_connection_stops(
            read_request,
            api_tx,
            app_timeout,
            stream,
            running,
        )?
        else {
            crate::logging::api_wait_completed(&request_id, &params.pane_id, "client_disconnected");
            return Ok(None);
        };
        if deadline.budget_at(std::time::Instant::now()) == WaitBudget::Expired {
            return output_wait_timeout(&request_id, &params.pane_id);
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&response) else {
            return Ok(Some(response));
        };
        if value.get("error").is_some() {
            let mut value = value;
            value["id"] = serde_json::Value::String(request_id.clone());
            return Ok(Some(serde_json::to_string(&value).unwrap()));
        }

        let read_value = value["result"]["read"].clone();
        let Ok(read) = serde_json::from_value::<crate::api::schema::PaneReadResult>(read_value)
        else {
            return Ok(Some(
                serde_json::to_string(&ErrorResponse {
                    id: request_id,
                    error: ErrorBody {
                        code: "internal_error".into(),
                        message: "failed to decode pane read result".into(),
                    },
                })
                .unwrap(),
            ));
        };

        let matched_line = match_output(&read.text, &params.r#match, regex.as_ref());
        if deadline.budget_at(std::time::Instant::now()) == WaitBudget::Expired {
            return output_wait_timeout(&request_id, &params.pane_id);
        }
        if matched_line.is_some() {
            let revision = read.revision;
            crate::logging::api_wait_completed(&request_id, &params.pane_id, "matched");
            return Ok(Some(
                serde_json::to_string(&SuccessResponse {
                    id: request_id,
                    result: ResponseResult::OutputMatched {
                        pane_id: read.pane_id.clone(),
                        revision,
                        matched_line,
                        read,
                    },
                })
                .unwrap(),
            ));
        }

        let Some(sleep) = deadline.sleep_at(std::time::Instant::now()) else {
            return output_wait_timeout(&request_id, &params.pane_id);
        };
        std::thread::sleep(sleep);
    }
}

pub(super) fn wait_for_agent(
    request_id: String,
    params: crate::api::schema::AgentWaitParams,
    stream: &mut LocalStream,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
) -> std::io::Result<Option<String>> {
    let deadline = WaitDeadline::from_timeout_ms(params.timeout_ms);
    let last_event_sequence = event_hub.current_sequence();
    let Some(app_timeout) = deadline.app_timeout_at(std::time::Instant::now()) else {
        return agent_wait_status_timeout(request_id).map(Some);
    };
    let initial = match agent_get(
        &request_id,
        &params.target,
        api_tx,
        app_timeout,
        stream,
        running,
    )? {
        AgentGetOutcome::Agent(agent) => *agent,
        AgentGetOutcome::ClientDisconnected => return Ok(None),
        AgentGetOutcome::Response(response) => {
            if deadline.budget_at(std::time::Instant::now()) == WaitBudget::Expired {
                return agent_wait_status_timeout(request_id).map(Some);
            }
            return serde_json::to_string(&response)
                .map(Some)
                .map_err(std::io::Error::other);
        }
    };
    if deadline.budget_at(std::time::Instant::now()) == WaitBudget::Expired {
        return agent_wait_timeout(request_id, AgentWaitTimeoutKind::Status, &initial).map(Some);
    }
    let until = agent_wait_statuses(params.until);
    if agent_wait_matches(&initial, &until, None) {
        return agent_wait_success(request_id, initial).map(Some);
    }

    match wait_for_resolved_agent(
        request_id.clone(),
        ResolvedAgentWait {
            target: params.target,
            until,
            deadline,
            initial,
            last_event_sequence,
            after_state_change_seq: None,
            accept_transient_status: true,
            timeout_kind: AgentWaitTimeoutKind::Status,
        },
        stream,
        api_tx,
        event_hub,
        running,
    )? {
        Some(AgentWaitOutcome::Matched(agent)) => agent_wait_success(request_id, *agent).map(Some),
        Some(AgentWaitOutcome::Response(response)) => Ok(Some(response)),
        None => Ok(None),
    }
}

pub(super) fn prompt_agent(
    request_id: String,
    params: crate::api::schema::AgentPromptParams,
    stream: &mut LocalStream,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
) -> std::io::Result<Option<String>> {
    let Some(wait) = params.wait.clone() else {
        return Ok(Some(dispatch_to_app_with_timeout(
            Request {
                id: request_id,
                method: Method::AgentPrompt(params),
            },
            api_tx,
            None,
        )));
    };

    let last_event_sequence = event_hub.current_sequence();
    let before_prompt = match agent_get(
        &request_id,
        &params.target,
        api_tx,
        APP_RESPONSE_TIMEOUT,
        stream,
        running,
    )? {
        AgentGetOutcome::Agent(agent) => *agent,
        AgentGetOutcome::ClientDisconnected => return Ok(None),
        AgentGetOutcome::Response(response) => {
            return serde_json::to_string(&response)
                .map(Some)
                .map_err(std::io::Error::other);
        }
    };
    let target = params.target.clone();
    let prompt_response = dispatch_to_app_with_timeout(
        Request {
            id: request_id.clone(),
            method: Method::AgentPrompt(params),
        },
        api_tx,
        None,
    );
    let Ok(prompted) = agent_from_response(&request_id, &prompt_response) else {
        return Ok(Some(prompt_response));
    };
    if !agent_wait_identity_matches(
        &prompted,
        &before_prompt.terminal_id,
        before_prompt.name.as_deref().filter(|name| *name == target),
        before_prompt.agent.as_deref(),
    ) {
        return agent_wait_not_running(request_id).map(Some);
    }

    let deadline = WaitDeadline::from_timeout_ms(wait.timeout_ms);
    let prompt_state_change_seq = prompted.state_change_seq;
    let until = agent_wait_statuses(wait.until);
    let mut initial = prompted;
    let mut after_state_change_seq = Some(prompt_state_change_seq);

    if initial.agent_status != crate::api::schema::AgentStatus::Working {
        let effect_timeout_ms = wait
            .timeout_ms
            .map_or(AGENT_PROMPT_EFFECT_TIMEOUT_MS, |timeout_ms| {
                timeout_ms.min(AGENT_PROMPT_EFFECT_TIMEOUT_MS)
            });
        let timeout_kind = if wait
            .timeout_ms
            .is_some_and(|timeout_ms| timeout_ms <= AGENT_PROMPT_EFFECT_TIMEOUT_MS)
        {
            AgentWaitTimeoutKind::Status
        } else {
            AgentWaitTimeoutKind::PromptStalled {
                baseline: prompt_state_change_seq,
                timeout_ms: effect_timeout_ms,
            }
        };
        let Some(outcome) = wait_for_resolved_agent(
            request_id.clone(),
            ResolvedAgentWait {
                target: target.clone(),
                until: all_agent_statuses(),
                deadline: deadline.capped_to(std::time::Duration::from_millis(
                    AGENT_PROMPT_EFFECT_TIMEOUT_MS,
                )),
                initial,
                last_event_sequence,
                after_state_change_seq,
                accept_transient_status: false,
                timeout_kind,
            },
            stream,
            api_tx,
            event_hub,
            running,
        )?
        else {
            return Ok(None);
        };
        initial = match outcome {
            AgentWaitOutcome::Matched(agent) => *agent,
            AgentWaitOutcome::Response(response) => return Ok(Some(response)),
        };
        after_state_change_seq = None;
        if agent_wait_matches(&initial, &until, None) {
            return agent_prompt_success(request_id, initial).map(Some);
        }
    }

    let Some(outcome) = wait_for_resolved_agent(
        request_id.clone(),
        ResolvedAgentWait {
            target,
            until,
            deadline,
            initial,
            // Replay from before submission so terminal lifecycle events consumed by
            // the activity gate still terminate this settled-state wait.
            last_event_sequence,
            after_state_change_seq,
            accept_transient_status: false,
            timeout_kind: AgentWaitTimeoutKind::Status,
        },
        stream,
        api_tx,
        event_hub,
        running,
    )?
    else {
        return Ok(None);
    };
    let agent = match outcome {
        AgentWaitOutcome::Matched(agent) => *agent,
        AgentWaitOutcome::Response(response) => return Ok(Some(response)),
    };
    agent_prompt_success(request_id, agent).map(Some)
}

fn agent_prompt_success(
    request_id: String,
    agent: crate::api::schema::AgentInfo,
) -> std::io::Result<String> {
    serde_json::to_string(&SuccessResponse {
        id: request_id,
        result: ResponseResult::AgentPrompted { agent },
    })
    .map_err(std::io::Error::other)
}

struct ResolvedAgentWait {
    target: String,
    until: Vec<crate::api::schema::AgentStatus>,
    deadline: WaitDeadline,
    initial: crate::api::schema::AgentInfo,
    last_event_sequence: u64,
    after_state_change_seq: Option<u64>,
    accept_transient_status: bool,
    timeout_kind: AgentWaitTimeoutKind,
}

#[derive(Clone, Copy)]
enum AgentWaitTimeoutKind {
    Status,
    PromptStalled { baseline: u64, timeout_ms: u64 },
}

enum AgentWaitOutcome {
    Matched(Box<crate::api::schema::AgentInfo>),
    Response(String),
}

fn wait_for_resolved_agent(
    request_id: String,
    wait: ResolvedAgentWait,
    stream: &mut LocalStream,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
) -> std::io::Result<Option<AgentWaitOutcome>> {
    let expected_terminal_id = wait.initial.terminal_id.clone();
    let expected_name = wait
        .initial
        .name
        .as_ref()
        .filter(|name| name.as_str() == wait.target)
        .cloned();
    let expected_agent = wait.initial.agent.clone();
    let pane_id = wait.initial.pane_id.clone();
    let deadline = wait.deadline;
    let mut last_agent = wait.initial.clone();
    let mut last_event_sequence = wait.last_event_sequence;
    let mut poll_for_launch_readiness = wait.initial.launch_pending;

    loop {
        if should_stop_connection(stream, running)? {
            return Ok(None);
        }
        if deadline.budget_at(std::time::Instant::now()) == WaitBudget::Expired {
            return agent_wait_timeout(request_id, wait.timeout_kind, &last_agent)
                .map(AgentWaitOutcome::Response)
                .map(Some);
        }

        let mut should_probe = poll_for_launch_readiness;
        let mut matched_event_status = None;
        for (sequence, event) in event_hub.events_after(last_event_sequence) {
            if deadline.budget_at(std::time::Instant::now()) == WaitBudget::Expired {
                return agent_wait_timeout(request_id, wait.timeout_kind, &last_agent)
                    .map(AgentWaitOutcome::Response)
                    .map(Some);
            }
            last_event_sequence = sequence;
            match event.data {
                EventData::PaneAgentDetected {
                    pane_id: event_pane,
                    agent,
                    released,
                    final_status,
                    ..
                } if event_pane == pane_id => {
                    if released {
                        if let Some(status) = final_status
                            .filter(|status| wait.until.contains(status))
                            .or(matched_event_status)
                        {
                            let mut matched = wait.initial.clone();
                            matched.agent_status = status;
                            return Ok(Some(AgentWaitOutcome::Matched(Box::new(matched))));
                        }
                        return agent_wait_not_running(request_id)
                            .map(AgentWaitOutcome::Response)
                            .map(Some);
                    }
                    if agent.is_some() && expected_agent.is_some() && agent != expected_agent {
                        return agent_wait_not_running(request_id)
                            .map(AgentWaitOutcome::Response)
                            .map(Some);
                    }
                    should_probe = true;
                }
                EventData::PaneAgentStatusChanged {
                    pane_id: event_pane,
                    agent_status,
                    ..
                } if event_pane == pane_id => {
                    if wait.accept_transient_status && wait.until.contains(&agent_status) {
                        matched_event_status = Some(agent_status);
                    }
                    should_probe = true;
                }
                EventData::PaneUpdated { pane } if pane.pane_id == pane_id => should_probe = true,
                EventData::PaneMoved {
                    previous_pane_id, ..
                } if previous_pane_id == pane_id => {
                    return agent_wait_not_running(request_id)
                        .map(AgentWaitOutcome::Response)
                        .map(Some);
                }
                EventData::PaneClosed {
                    pane_id: event_pane,
                    ..
                }
                | EventData::PaneExited {
                    pane_id: event_pane,
                    ..
                } if event_pane == pane_id => {
                    return agent_wait_not_running(request_id)
                        .map(AgentWaitOutcome::Response)
                        .map(Some);
                }
                _ => {}
            }
        }

        if should_probe {
            let Some(app_timeout) = deadline.app_timeout_at(std::time::Instant::now()) else {
                return agent_wait_timeout(request_id, wait.timeout_kind, &last_agent)
                    .map(AgentWaitOutcome::Response)
                    .map(Some);
            };
            let current = match agent_get(
                &request_id,
                &wait.target,
                api_tx,
                app_timeout,
                stream,
                running,
            )? {
                AgentGetOutcome::Agent(agent) => *agent,
                AgentGetOutcome::ClientDisconnected => return Ok(None),
                AgentGetOutcome::Response(response) => {
                    if deadline.budget_at(std::time::Instant::now()) == WaitBudget::Expired {
                        return agent_wait_timeout(request_id, wait.timeout_kind, &last_agent)
                            .map(AgentWaitOutcome::Response)
                            .map(Some);
                    }
                    return agent_wait_probe_error(response)
                        .map(AgentWaitOutcome::Response)
                        .map(Some);
                }
            };
            if deadline.budget_at(std::time::Instant::now()) == WaitBudget::Expired {
                return agent_wait_timeout(request_id, wait.timeout_kind, &current)
                    .map(AgentWaitOutcome::Response)
                    .map(Some);
            }
            if !agent_wait_identity_matches(
                &current,
                &expected_terminal_id,
                expected_name.as_deref(),
                expected_agent.as_deref(),
            ) {
                return agent_wait_not_running(request_id)
                    .map(AgentWaitOutcome::Response)
                    .map(Some);
            }
            poll_for_launch_readiness = current.launch_pending;
            last_agent = current.clone();
            if let Some(status) = matched_event_status.filter(|_| agent_wait_ready(&current)) {
                let mut matched = current;
                matched.agent_status = status;
                return Ok(Some(AgentWaitOutcome::Matched(Box::new(matched))));
            }
            if agent_wait_matches(&current, &wait.until, wait.after_state_change_seq) {
                return Ok(Some(AgentWaitOutcome::Matched(Box::new(current))));
            }
        }

        let Some(sleep) = deadline.sleep_at(std::time::Instant::now()) else {
            return agent_wait_timeout(request_id, wait.timeout_kind, &last_agent)
                .map(AgentWaitOutcome::Response)
                .map(Some);
        };
        std::thread::sleep(sleep);
    }
}

fn all_agent_statuses() -> Vec<crate::api::schema::AgentStatus> {
    // Keep this exhaustive: every status is evidence that the sequence advanced.
    vec![
        crate::api::schema::AgentStatus::Idle,
        crate::api::schema::AgentStatus::Working,
        crate::api::schema::AgentStatus::Blocked,
        crate::api::schema::AgentStatus::Done,
        crate::api::schema::AgentStatus::Unknown,
    ]
}

fn agent_wait_statuses(
    until: Vec<crate::api::schema::AgentStatus>,
) -> Vec<crate::api::schema::AgentStatus> {
    if until.is_empty() {
        vec![
            crate::api::schema::AgentStatus::Idle,
            crate::api::schema::AgentStatus::Done,
            crate::api::schema::AgentStatus::Blocked,
        ]
    } else {
        until
    }
}

fn agent_wait_identity_matches(
    agent: &crate::api::schema::AgentInfo,
    expected_terminal_id: &str,
    expected_name: Option<&str>,
    expected_agent: Option<&str>,
) -> bool {
    agent.terminal_id == expected_terminal_id
        && expected_name.is_none_or(|name| agent.name.as_deref() == Some(name))
        && match (expected_agent, agent.agent.as_deref()) {
            (Some(expected), Some(current)) => expected == current,
            (Some(_), None) => agent.name.is_some(),
            (None, _) => true,
        }
}

fn agent_wait_matches(
    agent: &crate::api::schema::AgentInfo,
    until: &[crate::api::schema::AgentStatus],
    after_state_change_seq: Option<u64>,
) -> bool {
    agent_wait_ready(agent)
        && until.contains(&agent.agent_status)
        && after_state_change_seq.is_none_or(|baseline| agent.state_change_seq > baseline)
}

fn agent_wait_ready(agent: &crate::api::schema::AgentInfo) -> bool {
    !agent.launch_pending || agent.agent_status == crate::api::schema::AgentStatus::Blocked
}

enum AgentGetOutcome {
    Agent(Box<crate::api::schema::AgentInfo>),
    Response(ErrorResponse),
    ClientDisconnected,
}

fn agent_get(
    request_id: &str,
    target: &str,
    api_tx: &ApiRequestSender,
    timeout: std::time::Duration,
    stream: &mut LocalStream,
    running: &Arc<AtomicBool>,
) -> std::io::Result<AgentGetOutcome> {
    let Some(response) = dispatch_to_app_with_timeout_until_connection_stops(
        Request {
            id: format!("{request_id}:agent"),
            method: Method::AgentGet(crate::api::schema::AgentTarget {
                target: target.to_string(),
            }),
        },
        api_tx,
        timeout,
        stream,
        running,
    )?
    else {
        return Ok(AgentGetOutcome::ClientDisconnected);
    };
    Ok(match agent_from_response(request_id, &response) {
        Ok(agent) => AgentGetOutcome::Agent(Box::new(agent)),
        Err(response) => AgentGetOutcome::Response(response),
    })
}

fn agent_from_response(
    request_id: &str,
    response: &str,
) -> Result<crate::api::schema::AgentInfo, ErrorResponse> {
    let value: serde_json::Value = serde_json::from_str(response).map_err(|_| ErrorResponse {
        id: request_id.into(),
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode agent response".into(),
        },
    })?;
    if value.get("error").is_some() {
        let error = serde_json::from_value(value["error"].clone()).map_err(|_| ErrorResponse {
            id: request_id.into(),
            error: ErrorBody {
                code: "internal_error".into(),
                message: "failed to decode agent error".into(),
            },
        })?;
        return Err(ErrorResponse {
            id: request_id.into(),
            error,
        });
    }
    serde_json::from_value(value["result"]["agent"].clone()).map_err(|_| ErrorResponse {
        id: request_id.into(),
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode agent result".into(),
        },
    })
}

fn agent_wait_success(
    request_id: String,
    agent: crate::api::schema::AgentInfo,
) -> std::io::Result<String> {
    serde_json::to_string(&SuccessResponse {
        id: request_id,
        result: ResponseResult::AgentInfo { agent },
    })
    .map_err(std::io::Error::other)
}

fn agent_wait_timeout(
    request_id: String,
    kind: AgentWaitTimeoutKind,
    current: &crate::api::schema::AgentInfo,
) -> std::io::Result<String> {
    let (code, message) = match kind {
        AgentWaitTimeoutKind::Status => {
            ("timeout", "timed out waiting for agent status".to_string())
        }
        AgentWaitTimeoutKind::PromptStalled {
            baseline,
            timeout_ms,
        } => {
            let status = format!("{:?}", current.agent_status).to_ascii_lowercase();
            (
                "agent_prompt_stalled",
                format!(
                    "agent prompt produced no observed state change within {timeout_ms} ms; status is {status} and state_change_seq remained {baseline}"
                ),
            )
        }
    };
    serde_json::to_string(&ErrorResponse {
        id: request_id,
        error: ErrorBody {
            code: code.into(),
            message,
        },
    })
    .map_err(std::io::Error::other)
}

fn agent_wait_status_timeout(request_id: String) -> std::io::Result<String> {
    serde_json::to_string(&ErrorResponse {
        id: request_id,
        error: ErrorBody {
            code: "timeout".into(),
            message: "timed out waiting for agent status".into(),
        },
    })
    .map_err(std::io::Error::other)
}

fn agent_wait_not_running(request_id: String) -> std::io::Result<String> {
    serde_json::to_string(&ErrorResponse {
        id: request_id,
        error: ErrorBody {
            code: "agent_not_running".into(),
            message: "agent is no longer running in the target pane".into(),
        },
    })
    .map_err(std::io::Error::other)
}

fn agent_wait_probe_error(response: ErrorResponse) -> std::io::Result<String> {
    if response.error.code == "agent_not_found" {
        return agent_wait_not_running(response.id);
    }
    serde_json::to_string(&response).map_err(std::io::Error::other)
}

pub(super) fn wait_for_event(
    request_id: String,
    params: EventsWaitParams,
    stream: &mut LocalStream,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
) -> std::io::Result<Option<String>> {
    let deadline = WaitDeadline::from_timeout_ms(params.timeout_ms);

    let subscription = match event_match_subscription(&request_id, params.match_event) {
        Ok(subscription) => subscription,
        Err(response) => return Ok(Some(serde_json::to_string(&response).unwrap())),
    };
    let Some(app_timeout) = deadline.app_timeout_at(std::time::Instant::now()) else {
        return event_wait_timeout(request_id).map(Some);
    };
    let mut active = match ActiveSubscription::new_for_event_wait(
        subscription,
        &request_id,
        0,
        api_tx,
        event_hub.current_sequence(),
        app_timeout,
        stream,
        running,
    )? {
        WaitSubscriptionInit::Active(active) => active,
        WaitSubscriptionInit::ClientDisconnected => return Ok(None),
        WaitSubscriptionInit::Error(response) => {
            if deadline.budget_at(std::time::Instant::now()) == WaitBudget::Expired {
                return event_wait_timeout(request_id).map(Some);
            }
            return Ok(Some(serde_json::to_string(&response).unwrap()));
        }
    };
    if deadline.budget_at(std::time::Instant::now()) == WaitBudget::Expired {
        return event_wait_timeout(request_id).map(Some);
    }

    loop {
        if should_stop_connection(stream, running)? {
            return Ok(None);
        }

        let Some(app_timeout) = deadline.app_timeout_at(std::time::Instant::now()) else {
            return event_wait_timeout(request_id).map(Some);
        };
        let poll = active.poll_for_event_wait(api_tx, event_hub, app_timeout, stream, running)?;
        if deadline.budget_at(std::time::Instant::now()) == WaitBudget::Expired {
            return event_wait_timeout(request_id).map(Some);
        }
        match poll {
            WaitSubscriptionPoll::Event(Some(event))
                if deadline.budget_at(std::time::Instant::now()) != WaitBudget::Expired =>
            {
                return Ok(Some(wait_matched_response(&request_id, event)));
            }
            WaitSubscriptionPoll::Event(Some(_)) => {
                return event_wait_timeout(request_id).map(Some)
            }
            WaitSubscriptionPoll::Event(None) => {}
            WaitSubscriptionPoll::ClientDisconnected => return Ok(None),
            WaitSubscriptionPoll::Error(mut response)
                if response.error.code == "pane_not_found" =>
            {
                response.id = request_id;
                return serde_json::to_string(&response)
                    .map(Some)
                    .map_err(std::io::Error::other);
            }
            WaitSubscriptionPoll::Error(_) => {}
        }

        let Some(sleep) = deadline.sleep_at(std::time::Instant::now()) else {
            return event_wait_timeout(request_id).map(Some);
        };
        std::thread::sleep(sleep);
    }
}

fn event_wait_timeout(request_id: String) -> std::io::Result<String> {
    serde_json::to_string(&ErrorResponse {
        id: request_id,
        error: ErrorBody {
            code: "timeout".into(),
            message: "timed out waiting for event match".into(),
        },
    })
    .map_err(std::io::Error::other)
}

fn event_match_subscription(
    request_id: &str,
    match_event: EventMatch,
) -> Result<Subscription, ErrorResponse> {
    match match_event {
        EventMatch::PaneAgentStatusChanged {
            pane_id,
            agent_status,
        } => Ok(Subscription::PaneAgentStatusChanged {
            pane_id,
            agent_status: Some(agent_status),
        }),
        _ => Err(ErrorResponse {
            id: request_id.into(),
            error: ErrorBody {
                code: "unsupported_event_wait_match".into(),
                message: "events.wait currently supports pane agent status matches".into(),
            },
        }),
    }
}

fn wait_matched_response(request_id: &str, event: serde_json::Value) -> String {
    let Ok(event) = serde_json::from_value::<SubscriptionEventEnvelope>(event) else {
        return serde_json::to_string(&ErrorResponse {
            id: request_id.into(),
            error: ErrorBody {
                code: "internal_error".into(),
                message: "failed to decode matched event".into(),
            },
        })
        .unwrap();
    };

    let SubscriptionEventData::PaneAgentStatusChanged(data) = event.data else {
        return serde_json::to_string(&ErrorResponse {
            id: request_id.into(),
            error: ErrorBody {
                code: "unsupported_event_wait_match".into(),
                message: "events.wait currently supports pane agent status matches".into(),
            },
        })
        .unwrap();
    };

    serde_json::to_string(&SuccessResponse {
        id: request_id.into(),
        result: ResponseResult::WaitMatched {
            event: EventEnvelope {
                event: EventKind::PaneAgentStatusChanged,
                data: EventData::PaneAgentStatusChanged {
                    pane_id: data.pane_id,
                    workspace_id: data.workspace_id,
                    agent_status: data.agent_status,
                    agent: data.agent,
                    title: data.title,
                    display_agent: data.display_agent,
                    state_labels: data.state_labels,
                },
            },
        },
    })
    .unwrap()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::*;

    fn agent_info(
        status: crate::api::schema::AgentStatus,
        launch_pending: bool,
    ) -> crate::api::schema::AgentInfo {
        crate::api::schema::AgentInfo {
            terminal_id: "t1".into(),
            name: Some("reviewer".into()),
            agent: Some("claude".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            display_agent: None,
            agent_status: status,
            screen_detection_skipped: false,
            state_labels: HashMap::new(),
            tokens: HashMap::new(),
            agent_session: None,
            workspace_id: "w1".into(),
            tab_id: "t1".into(),
            pane_id: "w1:p1".into(),
            focused: true,
            launch_pending,
            interactive_ready: !launch_pending,
            state_change_seq: 1,
            cwd: None,
            foreground_cwd: None,
            revision: 1,
        }
    }

    #[test]
    fn agent_wait_requires_managed_launch_to_settle_before_matching_status() {
        let idle = crate::api::schema::AgentStatus::Idle;

        assert!(!agent_wait_matches(&agent_info(idle, true), &[idle], None));
        assert!(agent_wait_matches(&agent_info(idle, false), &[idle], None));
    }

    #[test]
    fn wait_deadline_rejects_zero_timeout_before_dispatch() {
        let now = std::time::Instant::now();
        let deadline = WaitDeadline {
            started: now,
            timeout: Some(Duration::ZERO),
        };

        assert_eq!(deadline.budget_at(now), WaitBudget::Expired);
        assert_eq!(deadline.app_timeout_at(now), None);
        assert_eq!(deadline.sleep_at(now), None);
    }

    #[test]
    fn wait_deadline_caps_remaining_app_and_sleep_budgets() {
        let now = std::time::Instant::now();
        let deadline = WaitDeadline {
            started: now,
            timeout: Some(Duration::from_millis(25)),
        };

        assert_eq!(
            deadline.app_timeout_at(now),
            Some(Duration::from_millis(25))
        );
        assert_eq!(deadline.sleep_at(now), Some(Duration::from_millis(25)));
        assert_eq!(
            deadline
                .capped_to(Duration::from_millis(10))
                .app_timeout_at(now),
            Some(Duration::from_millis(10))
        );
    }

    #[test]
    fn wait_deadline_handles_maximum_timeout_without_instant_addition() {
        let now = std::time::Instant::now();
        let deadline = WaitDeadline::from_timeout_ms(Some(u64::MAX));

        assert!(matches!(deadline.budget_at(now), WaitBudget::Remaining(_)));
    }

    #[test]
    fn agent_wait_allows_a_pending_launch_to_report_blocked() {
        let blocked = crate::api::schema::AgentStatus::Blocked;

        assert!(agent_wait_matches(
            &agent_info(blocked, true),
            &[blocked],
            None,
        ));
    }

    #[test]
    fn agent_wait_probe_only_translates_agent_disappearance() {
        let disappeared = agent_wait_probe_error(ErrorResponse {
            id: "wait".into(),
            error: ErrorBody {
                code: "agent_not_found".into(),
                message: "missing".into(),
            },
        })
        .unwrap();
        let disappeared: ErrorResponse = serde_json::from_str(&disappeared).unwrap();
        assert_eq!(disappeared.id, "wait");
        assert_eq!(disappeared.error.code, "agent_not_running");

        let unavailable = agent_wait_probe_error(ErrorResponse {
            id: "wait".into(),
            error: ErrorBody {
                code: "server_unavailable".into(),
                message: "timed out waiting for app response".into(),
            },
        })
        .unwrap();
        let unavailable: ErrorResponse = serde_json::from_str(&unavailable).unwrap();
        assert_eq!(unavailable.id, "wait");
        assert_eq!(unavailable.error.code, "server_unavailable");
    }
}
