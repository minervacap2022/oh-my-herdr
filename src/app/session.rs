use std::time::{Duration, Instant};

use super::{App, SESSION_SAVE_DEBOUNCE, SESSION_SAVE_RETRY_MAX_DELAY};

enum SessionSaveJob {
    Clear,
    Save {
        snapshot: crate::persist::SessionSnapshot,
        history: Option<crate::persist::SessionHistorySnapshot>,
    },
}

impl App {
    pub(super) fn sync_saved_agent_profiles(&mut self) {
        self.state.saved_agent_profiles = self
            .agent_registry
            .profiles
            .values()
            .map(|profile| crate::app::state::SavedAgentProfile {
                role: profile.role.clone(),
                native_cwd: profile.native_cwd.display().to_string(),
                harness: profile.harness.clone(),
                replicas_assigned: profile.replicas_assigned,
            })
            .collect();
    }

    pub(super) fn schedule_session_save(&mut self) {
        if !self.no_session {
            let deadline = Instant::now() + SESSION_SAVE_DEBOUNCE;
            if self.session_save_retry_attempt > 0
                && self
                    .session_save_deadline
                    .is_some_and(|retry_deadline| retry_deadline > deadline)
            {
                return;
            }
            self.session_save_deadline = Some(deadline);
        }
    }

    fn schedule_session_save_retry(&mut self) {
        if self.no_session {
            return;
        }

        let exponent = u32::from(self.session_save_retry_attempt.min(4));
        let retry_delay = SESSION_SAVE_DEBOUNCE
            .checked_mul(1_u32 << exponent)
            .unwrap_or(SESSION_SAVE_RETRY_MAX_DELAY)
            .min(SESSION_SAVE_RETRY_MAX_DELAY);
        self.session_save_retry_attempt = self.session_save_retry_attempt.saturating_add(1);
        self.session_save_deadline = Some(Instant::now() + retry_delay);
    }

    pub(crate) fn sync_session_save_schedule(&mut self) {
        if self.reap_finished_session_save() {
            self.state.mark_session_dirty();
            self.schedule_session_save_retry();
        }
        if self.state.session_dirty {
            self.state.session_dirty = false;
            if self.session_save_deadline.is_none() {
                self.schedule_session_save();
            }
        }
    }

    fn reap_finished_session_save(&mut self) -> bool {
        if self
            .session_save_thread
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            if let Some(thread) = self.session_save_thread.take() {
                return match thread.join() {
                    Ok(Ok(())) => {
                        self.session_save_retry_attempt = 0;
                        false
                    }
                    Ok(Err(err)) => {
                        tracing::warn!(err = %err, "background session save failed; scheduling retry");
                        true
                    }
                    Err(_) => {
                        tracing::warn!("background session save thread panicked; scheduling retry");
                        true
                    }
                };
            }
        }
        false
    }

    fn capture_session_save_job(&self) -> SessionSaveJob {
        if self.state.workspaces.is_empty() {
            SessionSaveJob::Clear
        } else {
            let snapshot = crate::persist::capture(
                &self.state.workspaces,
                &self.state.terminals,
                &self.terminal_runtimes,
                self.state.active,
                self.state.selected,
                self.state.sidebar_width,
                self.state.sidebar_section_split,
                self.state.collapsed_space_keys.clone(),
            );
            let history = self.persist_pane_history.then(|| {
                crate::persist::capture_history(&self.state.workspaces, &self.terminal_runtimes)
            });
            SessionSaveJob::Save { snapshot, history }
        }
    }

    pub(crate) fn start_background_session_save(&mut self) {
        if self.no_session {
            self.session_save_deadline = None;
            self.session_save_retry_attempt = 0;
            return;
        }

        if self.reap_finished_session_save() {
            self.state.mark_session_dirty();
            self.schedule_session_save_retry();
            return;
        }
        if self.session_save_thread.is_some() {
            self.session_save_deadline = Some(Instant::now() + Duration::from_millis(250));
            return;
        }

        let job = self.capture_session_save_job();
        self.session_save_deadline = None;
        match std::thread::Builder::new()
            .name("herdr-session-save".into())
            .spawn(move || run_session_save_job(job))
        {
            Ok(thread) => self.session_save_thread = Some(thread),
            Err(err) => {
                tracing::warn!(err = %err, "failed to spawn session save thread; saving inline");
                if run_session_save_job(self.capture_session_save_job()).is_err() {
                    self.state.mark_session_dirty();
                    self.schedule_session_save_retry();
                } else {
                    self.session_save_retry_attempt = 0;
                }
            }
        }
    }

    pub(crate) fn save_session_now(&mut self) {
        if let Some(thread) = self.session_save_thread.take() {
            match thread.join() {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::warn!(err = %err, "background session save failed before forced save");
                }
                Err(_) => {
                    tracing::warn!("background session save thread panicked before forced save");
                }
            }
        }

        if self.no_session {
            self.session_save_deadline = None;
            self.session_save_retry_attempt = 0;
            return;
        }

        match run_session_save_job(self.capture_session_save_job()) {
            Ok(()) => {
                self.session_save_retry_attempt = 0;
                self.session_save_deadline = None;
            }
            Err(err) => {
                tracing::warn!(err = %err, "forced session save failed; scheduling retry");
                self.state.mark_session_dirty();
                self.schedule_session_save_retry();
            }
        }
    }

    pub(super) fn update_agent_registry<T>(
        &mut self,
        mutation: impl FnOnce(&mut crate::agent_registry::AgentRegistry) -> T,
    ) -> std::io::Result<T> {
        let result = if self.no_session {
            mutation(&mut self.agent_registry)
        } else {
            if self.agent_registry.version > crate::agent_registry::REGISTRY_VERSION {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    format!(
                        "agent registry version {} is newer than supported version {}",
                        self.agent_registry.version,
                        crate::agent_registry::REGISTRY_VERSION
                    ),
                ));
            }
            let (result, registry) = crate::agent_registry::update(mutation)?;
            self.agent_registry = registry;
            result
        };
        self.sync_saved_agent_profiles();
        self.state.mark_session_dirty();
        self.schedule_session_save();
        Ok(result)
    }

    pub(super) fn refresh_agent_registry_for_read(&mut self) -> std::io::Result<()> {
        if !self.no_session {
            self.agent_registry = crate::agent_registry::load_for_read()?;
        }
        self.sync_saved_agent_profiles();
        Ok(())
    }
}

fn run_session_save_job(job: SessionSaveJob) -> std::io::Result<()> {
    match job {
        // NB: the agent registry is intentionally NOT cleared here. Saved agent
        // profiles are persistent across the session lifecycle (they survive
        // empty-workspaces and restart), so the session clear only touches the
        // session + history files; profiles are dropped only by an explicit
        // user action wired in phase 2.
        SessionSaveJob::Clear => crate::persist::clear(),
        SessionSaveJob::Save { snapshot, history } => {
            crate::persist::save(&snapshot, history.as_ref())
        }
    }
}
