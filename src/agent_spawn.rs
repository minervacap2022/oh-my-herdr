//! Pure spawn-planning logic for the owlspace-style agent spawn model.
//!
//! In this model the user never splits panes by hand. They pick a tab and click
//! an agent in the Agents sidebar; herdr then ensures the agent lands in its own
//! pane:
//!
//! * The **first** agent in a tab reuses the tab's existing available shell.
//! * Each **subsequent** agent auto-splits a pane (new pane to the right,
//!   uncapped) so N agents => N panes in a row.
//!
//! [`plan_spawn`] turns a tab's current pane state into a [`SpawnPlan`] without
//! touching PTYs, runtimes, or the layout tree. Keeping it pure mirrors herdr's
//! *"render is pure / state is separated from runtime"* spine: the caller
//! (CLI/API layer) reads live pane facts into a [`TabAgentContext`], hands them
//! to [`plan_spawn`], then executes the resulting [`SpawnPlan`] against the real
//! layout and runtimes.
//!
//! See `docs/design/agent-registry-spawn-comms.md`.

use std::path::PathBuf;

use ratatui::layout::Direction;

use crate::layout::PaneId;

/// Direction used when auto-splitting a tab to make room for a new agent.
///
/// Ratatui `Horizontal` splits a rect into columns, so the new pane appears to
/// the right of the split target — i.e. agents grow left-to-right, newest on the
/// right, exactly as the design spec calls for.
pub const AUTO_SPLIT_DIRECTION: Direction = Direction::Horizontal;

/// How the new agent's cwd is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CwdMode {
    /// The agent runs in the **tab's** working directory.
    Tab,
    /// The agent runs in the **profile's** native cwd.
    Agent,
}

/// State of one pane in the target tab, as observed from live state.
///
/// The caller fills terminal state from live runtimes; the planner itself never
/// reads PTYs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneAgentState {
    pub pane_id: PaneId,
    /// Does this pane currently host an interactive agent?
    pub is_agent: bool,
    /// Is this pane an available interactive shell (a valid spawn target)?
    pub is_available: bool,
    /// Is this pane's shell still starting? The first agent in a tab waits for
    /// this pane instead of creating an unnecessary split.
    pub is_shell_starting: bool,
}

/// The panes of the target tab, in layout order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabAgentContext {
    pub panes: Vec<PaneAgentState>,
}

impl TabAgentContext {
    /// A tab with a single (available) root pane — the empty-tab case.
    pub fn empty(root_pane: PaneId) -> Self {
        Self {
            panes: vec![PaneAgentState {
                pane_id: root_pane,
                is_agent: false,
                is_available: true,
                is_shell_starting: false,
            }],
        }
    }

    /// The first available shell, if any.
    pub fn first_available(&self) -> Option<&PaneAgentState> {
        self.panes.iter().find(|p| p.is_available)
    }

    /// The first shell which is still starting, if any.
    pub fn first_starting_shell(&self) -> Option<&PaneAgentState> {
        self.panes.iter().find(|pane| pane.is_shell_starting)
    }

    /// Whether a tab already hosts an interactive agent.
    pub fn has_agent(&self) -> bool {
        self.panes.iter().any(|pane| pane.is_agent)
    }

    /// The pane to split when no shell is available.
    ///
    /// We always split the **last** pane in layout order so the new pane lands to
    /// its right and agents accumulate left-to-right regardless of focus. This
    /// keeps the decision deterministic and independent of which pane is focused.
    pub fn split_target(&self) -> Option<&PaneAgentState> {
        self.panes.last()
    }
}

/// What to do with the layout to make room for the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnAction {
    /// Start in an existing available pane (no layout change).
    UseExisting { pane_id: PaneId },
    /// Split `from_pane` in [`AUTO_SPLIT_DIRECTION`] to create the pane.
    Split {
        from_pane: PaneId,
        direction: Direction,
    },
}

/// The full, layout-agnostic result of planning a spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnPlan {
    /// Live agent name to register (`role` for the first instance,
    /// `role-replica-N` for subsequent ones).
    pub agent_name: String,
    /// Interactive agent kind (e.g. `codex`, `claude`).
    pub kind: String,
    /// Resolved working directory for the new pane.
    pub cwd: PathBuf,
    /// How the cwd was resolved (recorded for tests / diagnostics).
    pub cwd_mode: CwdMode,
    /// Layout action to take before starting the agent.
    pub action: SpawnAction,
}

impl SpawnPlan {
    /// The pane the agent will occupy once the plan is executed.
    ///
    /// Part of the [`SpawnPlan`] accessor surface, exercised by the unit tests;
    /// kept even though non-test builds do not reference it yet (phase 2.5 UI
    /// will read it directly instead of matching on [`SpawnAction`] by hand).
    #[allow(dead_code)]
    pub fn target_pane(&self) -> PaneId {
        match &self.action {
            SpawnAction::UseExisting { pane_id } => *pane_id,
            // The split does not yield a new PaneId until the layout mutates, so we
            // surface the split *source* here; the caller produces the new id.
            SpawnAction::Split { from_pane, .. } => *from_pane,
        }
    }
}

/// Plan a spawn.
///
/// * `kind` is the interactive agent kind (validated by the caller before use).
/// * `agent_name` is the live name to register — already computed by the caller
///   from the lowest currently available replica index.
/// * `tab_cwd` / `native_cwd` are the two cwd candidates.
pub fn plan_spawn(
    ctx: &TabAgentContext,
    kind: &str,
    agent_name: &str,
    cwd_mode: CwdMode,
    tab_cwd: PathBuf,
    native_cwd: PathBuf,
) -> SpawnPlan {
    let cwd = match cwd_mode {
        CwdMode::Tab => tab_cwd,
        CwdMode::Agent => native_cwd,
    };
    let reusable_pane = ctx.first_available().or_else(|| {
        if !ctx.has_agent() {
            ctx.first_starting_shell()
        } else {
            None
        }
    });
    let action = match reusable_pane {
        Some(available) => SpawnAction::UseExisting {
            pane_id: available.pane_id,
        },
        None => match ctx.split_target() {
            Some(target) => SpawnAction::Split {
                from_pane: target.pane_id,
                direction: AUTO_SPLIT_DIRECTION,
            },
            None => SpawnAction::UseExisting {
                pane_id: ctx
                    .panes
                    .first()
                    .map(|p| p.pane_id)
                    .unwrap_or_else(PaneId::alloc),
            },
        },
    };
    SpawnPlan {
        agent_name: agent_name.to_string(),
        kind: kind.to_string(),
        cwd,
        cwd_mode,
        action,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: u32) -> PaneAgentState {
        PaneAgentState {
            pane_id: PaneId::from_raw(id),
            is_agent: false,
            is_available: true,
            is_shell_starting: false,
        }
    }

    fn agent_pane(id: u32) -> PaneAgentState {
        let mut p = pane(id);
        p.is_agent = true;
        p.is_available = false;
        p.is_shell_starting = false;
        p
    }

    const TAB_CWD: &str = "/repo/tab";
    const NATIVE_CWD: &str = "/home/me/native";

    fn cwd() -> PathBuf {
        PathBuf::from(TAB_CWD)
    }

    fn native() -> PathBuf {
        PathBuf::from(NATIVE_CWD)
    }

    #[test]
    fn empty_tab_reuses_root_pane_without_splitting() {
        let ctx = TabAgentContext::empty(PaneId::from_raw(1));
        let plan = plan_spawn(&ctx, "codex", "planner", CwdMode::Tab, cwd(), native());
        assert_eq!(
            plan.action,
            SpawnAction::UseExisting {
                pane_id: PaneId::from_raw(1)
            }
        );
        assert_eq!(plan.cwd, cwd());
        assert_eq!(plan.agent_name, "planner");
    }

    #[test]
    fn available_shell_is_reused_even_if_an_agent_pane_exists() {
        // A tab with one agent pane + one free shell: the new agent takes the
        // free shell instead of splitting.
        let ctx = TabAgentContext {
            panes: vec![agent_pane(1), pane(2)],
        };
        let plan = plan_spawn(
            &ctx,
            "codex",
            "planner-replica-1",
            CwdMode::Agent,
            cwd(),
            native(),
        );
        assert_eq!(
            plan.action,
            SpawnAction::UseExisting {
                pane_id: PaneId::from_raw(2)
            }
        );
        assert_eq!(plan.cwd, native());
    }

    #[test]
    fn second_agent_splits_the_last_pane_to_the_right() {
        // One agent already occupies the only pane -> must split.
        let ctx = TabAgentContext {
            panes: vec![agent_pane(1)],
        };
        let plan = plan_spawn(
            &ctx,
            "codex",
            "planner-replica-1",
            CwdMode::Tab,
            cwd(),
            native(),
        );
        assert_eq!(
            plan.action,
            SpawnAction::Split {
                from_pane: PaneId::from_raw(1),
                direction: Direction::Horizontal,
            }
        );
        assert_eq!(plan.target_pane(), PaneId::from_raw(1));
    }

    #[test]
    fn split_target_is_always_the_last_pane_regardless_of_focus() {
        // Even if the focused-looking pane is first, we split the last so the
        // new pane lands on the right.
        let ctx = TabAgentContext {
            panes: vec![agent_pane(1), agent_pane(2), agent_pane(3)],
        };
        let plan = plan_spawn(
            &ctx,
            "codex",
            "planner-replica-3",
            CwdMode::Tab,
            cwd(),
            native(),
        );
        assert_eq!(
            plan.action,
            SpawnAction::Split {
                from_pane: PaneId::from_raw(3),
                direction: Direction::Horizontal,
            }
        );
    }

    #[test]
    fn cwd_mode_selects_the_candidate() {
        let ctx = TabAgentContext::empty(PaneId::from_raw(1));
        let tab = plan_spawn(&ctx, "codex", "a", CwdMode::Tab, cwd(), native());
        let agent = plan_spawn(&ctx, "codex", "a", CwdMode::Agent, cwd(), native());
        assert_eq!(tab.cwd_mode, CwdMode::Tab);
        assert_eq!(tab.cwd, cwd());
        assert_eq!(agent.cwd_mode, CwdMode::Agent);
        assert_eq!(agent.cwd, native());
    }

    #[test]
    fn replica_name_flows_through_untouched() {
        let ctx = TabAgentContext::empty(PaneId::from_raw(1));
        let plan = plan_spawn(
            &ctx,
            "claude",
            "reviewer-replica-2",
            CwdMode::Tab,
            cwd(),
            native(),
        );
        assert_eq!(plan.agent_name, "reviewer-replica-2");
        assert_eq!(plan.kind, "claude");
    }

    #[test]
    fn single_pane_tab_with_no_agent_uses_it() {
        let ctx = TabAgentContext {
            panes: vec![pane(7)],
        };
        let plan = plan_spawn(&ctx, "codex", "sneak", CwdMode::Tab, cwd(), native());
        assert_eq!(
            plan.action,
            SpawnAction::UseExisting {
                pane_id: PaneId::from_raw(7)
            }
        );
    }

    #[test]
    fn first_agent_waits_for_a_starting_root_shell_without_splitting() {
        let ctx = TabAgentContext {
            panes: vec![PaneAgentState {
                pane_id: PaneId::from_raw(7),
                is_agent: false,
                is_available: false,
                is_shell_starting: true,
            }],
        };
        let plan = plan_spawn(&ctx, "codex", "reviewer", CwdMode::Tab, cwd(), native());
        assert_eq!(
            plan.action,
            SpawnAction::UseExisting {
                pane_id: PaneId::from_raw(7)
            }
        );
    }
}
