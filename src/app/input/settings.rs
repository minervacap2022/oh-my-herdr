use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::{
    api::schema::{AgentProfileCreateParams, AgentProfileSetParams},
    app::{
        state::{AgentProfileForm, AgentProfileMarkdown, AppState, SettingsSection, THEME_NAMES},
        App, Mode,
    },
    config::{StatusIndicatorStyle, ToastDelivery},
};

#[derive(Debug, Clone, PartialEq, Eq)]
// The shared `Save` verb is semantic: these actions persist settings.
#[allow(clippy::enum_variant_names)]
pub(super) enum SettingsAction {
    SaveTheme(String),
    SaveStatusIndicators(StatusIndicatorStyle),
    SaveSound(bool),
    SaveToastDelivery(ToastDelivery),
    SaveAgentBorderLabels(bool),
    InstallRecommendedIntegrations,
    OpenAgentCreate(String),
    OpenAgentEdit(String),
    SaveAgentProfile,
    DeleteAgentProfile(String),
    CreateAgentProfileMarkdown { role: String, name: String },
    DeleteAgentProfileMarkdown { role: String, name: String },
    StartAgentProfile(String),
}

impl App {
    pub(crate) fn handle_settings_key(&mut self, key: KeyEvent) {
        let previous_section = self.state.settings.section;
        if let Some(action) = update_settings_state(&mut self.state, key) {
            match action {
                SettingsAction::SaveTheme(name) => self.save_theme(&name),
                SettingsAction::SaveStatusIndicators(style) => self.save_status_indicators(style),
                SettingsAction::SaveSound(enabled) => self.save_sound(enabled),
                SettingsAction::SaveToastDelivery(delivery) => self.save_toast_delivery(delivery),
                SettingsAction::SaveAgentBorderLabels(enabled) => {
                    self.save_agent_border_labels(enabled)
                }
                SettingsAction::InstallRecommendedIntegrations => {
                    self.install_recommended_integrations()
                }
                SettingsAction::OpenAgentCreate(harness) => {
                    self.open_agent_profile_create_form(harness)
                }
                SettingsAction::OpenAgentEdit(role) => self.open_agent_profile_edit_form(role),
                SettingsAction::SaveAgentProfile => self.save_agent_profile_form(),
                SettingsAction::DeleteAgentProfile(role) => self.delete_agent_profile_via_api(role),
                SettingsAction::CreateAgentProfileMarkdown { role, name } => {
                    self.create_agent_profile_markdown(role, name)
                }
                SettingsAction::DeleteAgentProfileMarkdown { role, name } => {
                    self.delete_agent_profile_markdown(role, name)
                }
                SettingsAction::StartAgentProfile(role) => self.spawn_agent_profile_via_api(role),
            }
        }
        if previous_section != SettingsSection::Integrations
            && self.state.settings.section == SettingsSection::Integrations
        {
            self.refresh_integration_recommendations();
        }
    }

    pub(super) fn open_agent_profile_create_form(&mut self, harness: String) {
        let native_cwd = self
            .state
            .active
            .and_then(|ws_idx| self.focused_pane_cwd_in_workspace(ws_idx))
            .unwrap_or_else(|| std::path::PathBuf::from("/"))
            .display()
            .to_string();
        self.state.settings.agent_profile_form = Some(AgentProfileForm {
            existing_role: None,
            role: String::new(),
            harness,
            native_cwd,
            model: String::new(),
            effort: String::new(),
            apikey_ref: String::new(),
            allowlist: String::new(),
            additional_markdown: Vec::new(),
            linked_markdown: Vec::new(),
            instructions: "# Agent instructions".to_string(),
            instructions_cursor: "# Agent instructions".len(),
            instructions_scroll: 0,
            selected_markdown: None,
            pending_markdown_name: None,
            selected_field: 0,
        });
    }

    pub(super) fn open_sidebar_agent_profile_create_form(&mut self) {
        open_settings_at(&mut self.state, SettingsSection::Agents);
        self.open_agent_profile_create_form("codex".to_string());
    }

    pub(super) fn open_agent_profile_edit_form(&mut self, role: String) {
        match self.profile(&role) {
            Ok(profile) => {
                let instructions =
                    match crate::agent_registry::read_owned_instructions(&profile.role) {
                        Ok(instructions) => instructions,
                        Err(err) => {
                            self.show_agent_profile_feedback(
                                crate::app::state::ToastKind::NeedsAttention,
                                "agent profile failed",
                                format!("could not read this profile's AGENTS.md: {err}"),
                            );
                            return;
                        }
                    };
                let mut additional_markdown = Vec::new();
                let mut linked_markdown = Vec::new();
                for markdown in profile.mds.iter().filter(|md| md.name != "AGENTS.md") {
                    if crate::agent_registry::is_owned_markdown_path(
                        &profile.role,
                        &markdown.name,
                        std::path::Path::new(&markdown.path),
                    ) {
                        match crate::agent_registry::read_owned_markdown(
                            &profile.role,
                            &markdown.name,
                        ) {
                            Ok(content) => additional_markdown.push(AgentProfileMarkdown {
                                name: markdown.name.clone(),
                                path: markdown.path.clone(),
                                cursor: content.len(),
                                content,
                                scroll: 0,
                            }),
                            Err(err) => {
                                self.show_agent_profile_feedback(
                                    crate::app::state::ToastKind::NeedsAttention,
                                    "agent profile failed",
                                    format!(
                                        "could not read this profile's {}: {err}",
                                        markdown.name
                                    ),
                                );
                                return;
                            }
                        }
                    } else {
                        linked_markdown.push(markdown.name.clone());
                    }
                }
                self.state.settings.agent_profile_form = Some(AgentProfileForm {
                    existing_role: Some(profile.role.clone()),
                    role: profile.role,
                    harness: profile.harness,
                    native_cwd: profile.native_cwd,
                    model: profile.model.unwrap_or_default(),
                    effort: profile
                        .effort
                        .map(|effort| match effort {
                            crate::api::schema::AgentProfileEffort::Low => "low",
                            crate::api::schema::AgentProfileEffort::Medium => "medium",
                            crate::api::schema::AgentProfileEffort::High => "high",
                        })
                        .unwrap_or_default()
                        .to_string(),
                    apikey_ref: profile.apikey_ref.unwrap_or_default(),
                    allowlist: profile
                        .allowlist
                        .as_ref()
                        .and_then(|allowlist| serde_json::to_string(allowlist).ok())
                        .unwrap_or_default(),
                    additional_markdown,
                    linked_markdown,
                    instructions_cursor: instructions.len(),
                    instructions_scroll: 0,
                    instructions,
                    selected_markdown: None,
                    pending_markdown_name: None,
                    selected_field: 7,
                });
            }
            Err(err) => self.show_agent_profile_feedback(
                crate::app::state::ToastKind::NeedsAttention,
                "agent profile failed",
                self.agent_profile_error_body(err).message,
            ),
        }
    }

    pub(super) fn save_agent_profile_form(&mut self) {
        let Some(form) = self.state.settings.agent_profile_form.take() else {
            return;
        };
        let result = match form.existing_role.clone() {
            Some(role) => save_existing_agent_profile(self, &form, role),
            None => self.create_profile(AgentProfileCreateParams {
                role: form.role.clone(),
                harness: form.harness.clone(),
                native_cwd: form.native_cwd.clone(),
                instructions: Some(form.instructions.clone()),
            }),
        };

        match result {
            Ok(profile) => {
                self.state.settings.list.selected = self
                    .state
                    .saved_agent_profiles
                    .iter()
                    .position(|saved| saved.role == profile.role)
                    .map(|idx| idx + 3)
                    .unwrap_or(0);
                self.show_agent_profile_feedback(
                    crate::app::state::ToastKind::UpdateInstalled,
                    "agent profile saved",
                    format!("{} uses {}", profile.role, profile.harness),
                );
            }
            Err(err) => {
                self.state.settings.agent_profile_form = Some(form);
                self.show_agent_profile_feedback(
                    crate::app::state::ToastKind::NeedsAttention,
                    "agent profile failed",
                    self.agent_profile_error_body(err).message,
                );
            }
        }
    }

    pub(super) fn create_agent_profile_markdown(&mut self, role: String, name: String) {
        if let Err(err) = self.profile(&role) {
            self.show_agent_profile_feedback(
                crate::app::state::ToastKind::NeedsAttention,
                "agent profile failed",
                self.agent_profile_error_body(err).message,
            );
            return;
        }
        let path = match crate::agent_registry::create_owned_markdown(&role, &name, "") {
            Ok(path) => path,
            Err(err) => {
                self.show_agent_profile_feedback(
                    crate::app::state::ToastKind::NeedsAttention,
                    "agent profile failed",
                    format!("could not create {name}: {err}"),
                );
                return;
            }
        };
        let profile = match self.set_profile_md(&role, &name, path.to_str()) {
            Ok(profile) => profile,
            Err(err) => {
                let _ = std::fs::remove_file(&path);
                self.show_agent_profile_feedback(
                    crate::app::state::ToastKind::NeedsAttention,
                    "agent profile failed",
                    self.agent_profile_error_body(err).message,
                );
                return;
            }
        };
        let path = profile
            .mds
            .iter()
            .find(|markdown| markdown.name == name)
            .map(|markdown| markdown.path.clone())
            .unwrap_or_else(|| path.display().to_string());
        if let Some(form) = self.state.settings.agent_profile_form.as_mut() {
            if form.existing_role.as_deref() == Some(role.as_str()) {
                form.additional_markdown.push(AgentProfileMarkdown {
                    name: name.clone(),
                    path,
                    content: String::new(),
                    cursor: 0,
                    scroll: 0,
                });
                form.additional_markdown
                    .sort_by(|left, right| left.name.cmp(&right.name));
                form.selected_markdown = form
                    .additional_markdown
                    .iter()
                    .position(|markdown| markdown.name == name);
                form.pending_markdown_name = None;
                form.selected_field = form.instructions_field();
            }
        }
        self.show_agent_profile_feedback(
            crate::app::state::ToastKind::UpdateInstalled,
            "profile document created",
            name,
        );
    }

    pub(super) fn delete_agent_profile_markdown(&mut self, role: String, name: String) {
        if let Err(err) = self.set_profile_md(&role, &name, None) {
            self.show_agent_profile_feedback(
                crate::app::state::ToastKind::NeedsAttention,
                "agent profile failed",
                self.agent_profile_error_body(err).message,
            );
            return;
        }
        if let Err(err) = crate::agent_registry::remove_owned_markdown(&role, &name) {
            self.show_agent_profile_feedback(
                crate::app::state::ToastKind::NeedsAttention,
                "agent profile failed",
                format!("removed {name} from the profile but could not delete its file: {err}"),
            );
            return;
        }
        if let Some(form) = self.state.settings.agent_profile_form.as_mut() {
            if form.existing_role.as_deref() == Some(role.as_str()) {
                form.additional_markdown
                    .retain(|markdown| markdown.name != name);
                form.selected_markdown = None;
                form.selected_field = form.documents_field().unwrap_or(0);
            }
        }
        self.show_agent_profile_feedback(
            crate::app::state::ToastKind::Finished,
            "profile document deleted",
            name,
        );
    }

    fn show_agent_profile_feedback(
        &mut self,
        kind: crate::app::state::ToastKind,
        title: &str,
        context: String,
    ) {
        let previous_toast = self.state.toast.clone();
        self.state.toast = Some(crate::app::state::ToastNotification {
            kind,
            title: title.to_string(),
            context,
            position: None,
            target: None,
        });
        self.sync_toast_deadline(previous_toast);
    }

    pub(super) fn delete_agent_profile_via_api(&mut self, role: String) {
        let response = self.dispatch_runtime_mutation(
            "tui.agent.profile.delete",
            crate::api::schema::Method::AgentProfileDelete(
                crate::api::schema::AgentProfileDeleteParams { role: role.clone() },
            ),
        );
        if let Ok(error) = serde_json::from_str::<crate::api::schema::ErrorResponse>(&response) {
            self.show_agent_profile_feedback(
                crate::app::state::ToastKind::NeedsAttention,
                "agent profile failed",
                error.error.message,
            );
            return;
        }
        self.state.settings.list.selected = self
            .state
            .settings
            .list
            .selected
            .min(2 + self.state.saved_agent_profiles.len());
        self.show_agent_profile_feedback(
            crate::app::state::ToastKind::Finished,
            "agent profile deleted",
            role,
        );
    }
}

fn normalize_theme_name(name: &str) -> String {
    name.to_lowercase().replace([' ', '_'], "-")
}

fn current_theme_index(theme_name: &str) -> usize {
    let normalized = normalize_theme_name(theme_name);
    THEME_NAMES
        .iter()
        .position(|name| normalize_theme_name(name) == normalized)
        .unwrap_or(0)
}

fn status_indicator_index(style: StatusIndicatorStyle) -> usize {
    match style {
        StatusIndicatorStyle::Dots => 0,
        StatusIndicatorStyle::Symbols => 1,
    }
}

fn status_indicator_for_index(idx: usize) -> StatusIndicatorStyle {
    if idx == 0 {
        StatusIndicatorStyle::Dots
    } else {
        StatusIndicatorStyle::Symbols
    }
}

fn toast_delivery_index(delivery: ToastDelivery) -> usize {
    match delivery {
        ToastDelivery::Off => 0,
        ToastDelivery::Herdr => 1,
        ToastDelivery::Terminal => 2,
        ToastDelivery::System => 3,
    }
}

fn toast_delivery_for_index(idx: usize) -> ToastDelivery {
    match idx {
        0 => ToastDelivery::Off,
        1 => ToastDelivery::Herdr,
        2 => ToastDelivery::Terminal,
        _ => ToastDelivery::System,
    }
}

fn preview_selected_theme(state: &mut AppState) {
    use crate::app::state::Palette;

    let name = THEME_NAMES[state.settings.list.selected];
    if let Some(mut palette) = Palette::from_name(name) {
        if let Some(custom) = &state.theme_runtime.custom {
            palette = palette.with_overrides(custom);
        }
        if let Some(accent) = &state.theme_runtime.legacy_accent {
            palette.accent = crate::config::parse_color(accent);
        }
        state.palette = palette;
        state.theme_name = name.to_string();
    }
}

fn cancel_settings(state: &mut AppState) {
    if let Some(palette) = state.settings.original_palette.take() {
        state.palette = palette;
    }
    if let Some(theme_name) = state.settings.original_theme.take() {
        state.theme_name = theme_name;
    }
    state.settings.agent_profile_form = None;
    super::modal::leave_modal(state);
}

fn integrations_need_install(state: &AppState) -> bool {
    state
        .integration_recommendations
        .iter()
        .any(crate::integration::IntegrationRecommendation::needs_install)
}

fn apply_settings(state: &mut AppState) -> Option<SettingsAction> {
    match state.settings.section {
        SettingsSection::Theme => {
            let theme_name = state.theme_name.clone();
            state.settings.original_palette = None;
            state.settings.original_theme = None;
            super::modal::leave_modal(state);
            Some(SettingsAction::SaveTheme(theme_name))
        }
        SettingsSection::Integrations if integrations_need_install(state) => {
            Some(SettingsAction::InstallRecommendedIntegrations)
        }
        SettingsSection::Integrations => None,
        SettingsSection::Agents if state.settings.agent_profile_form.is_some() => {
            return Some(SettingsAction::SaveAgentProfile);
        }
        SettingsSection::Agents => None,
        _ => {
            super::modal::leave_modal(state);
            None
        }
    }
}

pub(super) fn update_settings_state(state: &mut AppState, key: KeyEvent) -> Option<SettingsAction> {
    if state.settings.section == SettingsSection::Agents
        && state.settings.agent_profile_form.is_some()
    {
        return update_agent_profile_form(state, key);
    }

    match state.settings.section {
        SettingsSection::Theme => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                let previous = state.settings.list.selected;
                state.settings.list.move_prev();
                if state.settings.list.selected != previous {
                    preview_selected_theme(state);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let previous = state.settings.list.selected;
                state.settings.list.move_next(THEME_NAMES.len());
                if state.settings.list.selected != previous {
                    preview_selected_theme(state);
                }
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                state.settings.section = SettingsSection::Indicators;
                state.settings.list.selected = status_indicator_index(state.status_indicators);
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                state.settings.section = SettingsSection::Integrations;
                state.settings.list.selected = 0;
            }
            _ => match super::modal::modal_action_from_key(&key, super::modal::SETTINGS_ACTIONS) {
                Some(super::modal::ModalAction::Apply) => return apply_settings(state),
                Some(super::modal::ModalAction::Close) => cancel_settings(state),
                _ => {}
            },
        },
        SettingsSection::Indicators => match key.code {
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                state.settings.list.selected = 1 - state.settings.list.selected.min(1);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let style = status_indicator_for_index(state.settings.list.selected);
                return Some(SettingsAction::SaveStatusIndicators(style));
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                state.settings.section = SettingsSection::Theme;
                state.settings.list.selected = current_theme_index(&state.theme_name);
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                state.settings.section = SettingsSection::Sound;
                state.settings.list.selected = usize::from(!state.sound_enabled());
            }
            _ => {
                if let Some(super::modal::ModalAction::Close) =
                    super::modal::modal_action_from_key(&key, super::modal::SETTINGS_ACTIONS)
                {
                    cancel_settings(state);
                }
            }
        },
        SettingsSection::Sound => match key.code {
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                state.settings.list.selected = 1 - state.settings.list.selected.min(1);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let enabled = state.settings.list.selected == 0;
                return Some(SettingsAction::SaveSound(enabled));
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                state.settings.section = SettingsSection::Toast;
                state.settings.list.selected = toast_delivery_index(state.toast_delivery());
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                state.settings.section = SettingsSection::Indicators;
                state.settings.list.selected = status_indicator_index(state.status_indicators);
            }
            _ => {
                if let Some(super::modal::ModalAction::Close) =
                    super::modal::modal_action_from_key(&key, super::modal::SETTINGS_ACTIONS)
                {
                    cancel_settings(state);
                }
            }
        },
        SettingsSection::Toast => match key.code {
            KeyCode::Up | KeyCode::Char('k') => state.settings.list.move_prev(),
            KeyCode::Down | KeyCode::Char('j') => state.settings.list.move_next(4),
            KeyCode::Enter | KeyCode::Char(' ') => {
                let delivery = toast_delivery_for_index(state.settings.list.selected);
                return Some(SettingsAction::SaveToastDelivery(delivery));
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                state.settings.section = SettingsSection::Sound;
                state.settings.list.selected = usize::from(!state.sound_enabled());
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                state.settings.section = SettingsSection::PaneLabels;
                state.settings.list.selected = usize::from(!state.agent_border_labels_enabled());
            }
            _ => {
                if let Some(super::modal::ModalAction::Close) =
                    super::modal::modal_action_from_key(&key, super::modal::SETTINGS_ACTIONS)
                {
                    cancel_settings(state);
                }
            }
        },
        SettingsSection::PaneLabels => match key.code {
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                state.settings.list.selected = 1 - state.settings.list.selected.min(1);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let enabled = state.settings.list.selected == 0;
                return Some(SettingsAction::SaveAgentBorderLabels(enabled));
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                state.settings.section = SettingsSection::Toast;
                state.settings.list.selected = toast_delivery_index(state.toast_delivery());
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                state.settings.section = SettingsSection::Agents;
                state.settings.list.selected = 0;
            }
            _ => {
                if let Some(super::modal::ModalAction::Close) =
                    super::modal::modal_action_from_key(&key, super::modal::SETTINGS_ACTIONS)
                {
                    cancel_settings(state);
                }
            }
        },
        SettingsSection::Integrations => match key.code {
            KeyCode::Enter | KeyCode::Char(' ') if integrations_need_install(state) => {
                return Some(SettingsAction::InstallRecommendedIntegrations);
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                state.settings.section = SettingsSection::Agents;
                state.settings.list.selected = 0;
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                state.settings.section = SettingsSection::Theme;
                state.settings.list.selected = current_theme_index(&state.theme_name);
            }
            _ => match super::modal::modal_action_from_key(&key, super::modal::SETTINGS_ACTIONS) {
                Some(super::modal::ModalAction::Apply) => return apply_settings(state),
                Some(super::modal::ModalAction::Close) => cancel_settings(state),
                _ => {}
            },
        },
        SettingsSection::Agents => match key.code {
            KeyCode::Up | KeyCode::Char('k') => state.settings.list.move_prev(),
            KeyCode::Down | KeyCode::Char('j') => state
                .settings
                .list
                .move_next(agent_settings_entry_count(state)),
            KeyCode::Enter | KeyCode::Char(' ') => {
                return agent_settings_selected_action(state);
            }
            KeyCode::Char('e') => {
                let selected = state.settings.list.selected;
                if selected >= 3 {
                    if let Some(profile) = state.saved_agent_profiles.get(selected - 3) {
                        return Some(SettingsAction::OpenAgentEdit(profile.role.clone()));
                    }
                }
            }
            KeyCode::Char('d') => {
                let selected = state.settings.list.selected;
                if selected >= 3 {
                    if let Some(profile) = state.saved_agent_profiles.get(selected - 3) {
                        return Some(SettingsAction::DeleteAgentProfile(profile.role.clone()));
                    }
                }
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                state.settings.section = SettingsSection::PaneLabels;
                state.settings.list.selected = usize::from(!state.agent_border_labels_enabled());
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                state.settings.section = SettingsSection::Integrations;
                state.settings.list.selected = 0;
            }
            _ => match super::modal::modal_action_from_key(&key, super::modal::SETTINGS_ACTIONS) {
                Some(super::modal::ModalAction::Close) => cancel_settings(state),
                _ => {}
            },
        },
    }

    None
}

fn agent_settings_entry_count(state: &AppState) -> usize {
    3 + state.saved_agent_profiles.len()
}

fn agent_settings_selected_action(state: &AppState) -> Option<SettingsAction> {
    match state.settings.list.selected {
        0 => Some(SettingsAction::OpenAgentCreate("codex".to_string())),
        1 => Some(SettingsAction::OpenAgentCreate("pi".to_string())),
        2 => Some(SettingsAction::OpenAgentCreate("claude".to_string())),
        selected => state
            .saved_agent_profiles
            .get(selected - 3)
            .map(|profile| SettingsAction::StartAgentProfile(profile.role.clone())),
    }
}

fn update_agent_profile_form(state: &mut AppState, key: KeyEvent) -> Option<SettingsAction> {
    let Some(form) = state.settings.agent_profile_form.as_mut() else {
        return None;
    };
    if let Some(name) = form.pending_markdown_name.as_mut() {
        match key.code {
            KeyCode::Esc => form.pending_markdown_name = None,
            KeyCode::Backspace => {
                name.pop();
            }
            KeyCode::Enter => {
                if let Some(role) = form.existing_role.clone() {
                    if !name.trim().is_empty() {
                        return Some(SettingsAction::CreateAgentProfileMarkdown {
                            role,
                            name: name.trim().to_string(),
                        });
                    }
                }
            }
            KeyCode::Char(ch) if !ch.is_control() => name.push(ch),
            _ => {}
        }
        return None;
    }
    match key.code {
        KeyCode::Esc => {
            state.settings.agent_profile_form = None;
        }
        KeyCode::Up | KeyCode::BackTab => {
            form.selected_field = form.selected_field.saturating_sub(1)
        }
        KeyCode::Down | KeyCode::Tab => {
            form.selected_field = (form.selected_field + 1).min(form.field_count() - 1);
        }
        KeyCode::Left => move_agent_profile_instruction_cursor(form, false),
        KeyCode::Right => move_agent_profile_instruction_cursor(form, true),
        KeyCode::Char('a') if form.documents_selected() => {
            form.pending_markdown_name = Some(String::new());
        }
        KeyCode::Char('d') if form.documents_selected() => {
            if let Some(index) = form.selected_markdown_index() {
                if let Some(role) = form.existing_role.clone() {
                    return Some(SettingsAction::DeleteAgentProfileMarkdown {
                        role,
                        name: form.additional_markdown[index].name.clone(),
                    });
                }
            }
        }
        KeyCode::Backspace => {
            if form.instructions_selected() {
                delete_agent_profile_instruction_char(form, true);
            } else if let Some(value) = agent_profile_form_field_mut(form) {
                value.pop();
            }
        }
        KeyCode::Delete if form.instructions_selected() => {
            delete_agent_profile_instruction_char(form, false);
        }
        KeyCode::PageUp if form.instructions_selected() => {
            let (_, _, scroll) = form.active_document_mut();
            *scroll = scroll.saturating_sub(8);
        }
        KeyCode::PageDown if form.instructions_selected() => {
            let (_, _, scroll) = form.active_document_mut();
            *scroll = scroll.saturating_add(8);
        }
        KeyCode::Home if form.instructions_selected() => {
            let (content, cursor, _) = form.active_document_mut();
            *cursor = instruction_line_start(content, *cursor);
        }
        KeyCode::End if form.instructions_selected() => {
            let (content, cursor, _) = form.active_document_mut();
            *cursor = instruction_line_end(content, *cursor);
        }
        KeyCode::Enter
            if form.instructions_selected() && !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            insert_agent_profile_instructions(form, "\n");
        }
        KeyCode::Enter => return Some(SettingsAction::SaveAgentProfile),
        KeyCode::Char(ch) if !ch.is_control() => {
            if form.instructions_selected() {
                insert_agent_profile_instructions(form, &ch.to_string());
            } else if let Some(value) = agent_profile_form_field_mut(form) {
                value.push(ch);
            }
        }
        _ => {}
    }
    None
}

fn agent_profile_form_field_mut(form: &mut AgentProfileForm) -> Option<&mut String> {
    match (form.is_new(), form.selected_field) {
        (true, 0) => Some(&mut form.role),
        (true, 2) | (false, 1) => Some(&mut form.native_cwd),
        (false, 2) => Some(&mut form.model),
        (false, 4) => Some(&mut form.apikey_ref),
        (false, 5) => Some(&mut form.allowlist),
        _ => None,
    }
}

fn move_agent_profile_instruction_cursor(form: &mut AgentProfileForm, forward: bool) {
    if form.instructions_selected() {
        let (content, cursor, _) = form.active_document_mut();
        let current = (*cursor).min(content.len());
        *cursor = if forward {
            next_instruction_boundary(content, current)
        } else {
            previous_instruction_boundary(content, current)
        };
        return;
    }

    if form.documents_selected() {
        form.cycle_document(forward);
        return;
    }

    let harness_field = if form.is_new() { 1 } else { 0 };
    if form.selected_field == harness_field {
        const HARNESSES: [&str; 3] = ["codex", "pi", "claude"];
        let current = HARNESSES
            .iter()
            .position(|harness| *harness == form.harness)
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % HARNESSES.len()
        } else {
            (current + HARNESSES.len() - 1) % HARNESSES.len()
        };
        form.harness = HARNESSES[next].to_string();
        return;
    }

    if !form.is_new() && form.selected_field == 3 {
        const EFFORTS: [&str; 4] = ["", "low", "medium", "high"];
        let current = EFFORTS
            .iter()
            .position(|effort| *effort == form.effort)
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % EFFORTS.len()
        } else {
            (current + EFFORTS.len() - 1) % EFFORTS.len()
        };
        form.effort = EFFORTS[next].to_string();
    }
}

pub(super) fn insert_agent_profile_form_text(state: &mut AppState, text: &str) -> bool {
    let Some(form) = state.settings.agent_profile_form.as_mut() else {
        return false;
    };
    if let Some(name) = form.pending_markdown_name.as_mut() {
        name.extend(text.chars().filter(|ch| !ch.is_control()));
        return true;
    }
    if form.instructions_selected() {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let text: String = text
            .chars()
            .filter(|ch| *ch == '\n' || !ch.is_control())
            .collect();
        insert_agent_profile_instructions(form, &text);
        return true;
    }
    let Some(value) = agent_profile_form_field_mut(form) else {
        return false;
    };
    value.extend(text.chars().filter(|ch| !ch.is_control()));
    true
}

fn insert_agent_profile_instructions(form: &mut AgentProfileForm, text: &str) {
    let (content, cursor, _) = form.active_document_mut();
    let current = (*cursor).min(content.len());
    content.insert_str(current, text);
    *cursor = current + text.len();
}

fn previous_instruction_boundary(value: &str, cursor: usize) -> usize {
    value[..cursor.min(value.len())]
        .char_indices()
        .next_back()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn next_instruction_boundary(value: &str, cursor: usize) -> usize {
    let cursor = cursor.min(value.len());
    value[cursor..]
        .char_indices()
        .nth(1)
        .map(|(index, _)| cursor + index)
        .unwrap_or(value.len())
}

fn instruction_line_start(value: &str, cursor: usize) -> usize {
    value[..cursor.min(value.len())]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0)
}

fn instruction_line_end(value: &str, cursor: usize) -> usize {
    let cursor = cursor.min(value.len());
    value[cursor..]
        .find('\n')
        .map(|index| cursor + index)
        .unwrap_or(value.len())
}

fn delete_agent_profile_instruction_char(form: &mut AgentProfileForm, backwards: bool) {
    let (content, cursor, _) = form.active_document_mut();
    let current = (*cursor).min(content.len());
    let (start, end) = if backwards {
        (previous_instruction_boundary(content, current), current)
    } else {
        (current, next_instruction_boundary(content, current))
    };
    if start == end {
        return;
    }
    content.replace_range(start..end, "");
    *cursor = start;
}

fn save_existing_agent_profile(
    app: &mut App,
    form: &AgentProfileForm,
    role: String,
) -> Result<crate::api::schema::AgentProfileInfo, crate::app::agents::AgentProfileError> {
    let allowlist = if form.allowlist.trim().is_empty() {
        None
    } else {
        Some(
            serde_json::from_str(&form.allowlist)
                .map_err(|_| crate::app::agents::AgentProfileError::InvalidAllowlist)?,
        )
    };
    let effort = match form.effort.as_str() {
        "" => None,
        "low" => Some(crate::api::schema::AgentProfileEffort::Low),
        "medium" => Some(crate::api::schema::AgentProfileEffort::Medium),
        "high" => Some(crate::api::schema::AgentProfileEffort::High),
        _ => return Err(crate::app::agents::AgentProfileError::InvalidPatch),
    };
    let profile = app.set_profile(AgentProfileSetParams {
        role: role.clone(),
        harness: Some(form.harness.clone()),
        native_cwd: Some(form.native_cwd.clone()),
        model: (!form.model.trim().is_empty()).then(|| form.model.clone()),
        effort,
        apikey_ref: (!form.apikey_ref.trim().is_empty()).then(|| form.apikey_ref.clone()),
        allowlist,
        clear_model: form.model.trim().is_empty(),
        clear_effort: form.effort.is_empty(),
        clear_apikey_ref: form.apikey_ref.trim().is_empty(),
        clear_allowlist: form.allowlist.trim().is_empty(),
    })?;
    crate::agent_registry::replace_owned_instructions(&role, &form.instructions)
        .map_err(|err| crate::app::agents::AgentProfileError::PersistFailed(err.to_string()))?;
    for markdown in &form.additional_markdown {
        crate::agent_registry::replace_owned_markdown(&role, &markdown.name, &markdown.content)
            .map_err(|err| crate::app::agents::AgentProfileError::PersistFailed(err.to_string()))?;
    }
    Ok(profile)
}

pub(crate) fn open_settings(state: &mut AppState) {
    open_settings_at(state, SettingsSection::Theme);
}

pub(crate) fn open_settings_at(state: &mut AppState, section: SettingsSection) {
    state.integration_install_messages.clear();
    state.settings.original_palette = Some(state.palette.clone());
    state.settings.original_theme = Some(state.theme_name.clone());
    state.settings.section = section;
    state.settings.list.selected = match section {
        SettingsSection::Theme => current_theme_index(&state.theme_name),
        SettingsSection::Indicators => status_indicator_index(state.status_indicators),
        SettingsSection::Sound => usize::from(!state.sound_enabled()),
        SettingsSection::Toast => toast_delivery_index(state.toast_delivery()),
        SettingsSection::PaneLabels => usize::from(!state.agent_border_labels_enabled()),
        SettingsSection::Agents => 0,
        SettingsSection::Integrations => 0,
    };
    state.mode = Mode::Settings;
}

impl AppState {
    fn settings_popup_rect(&self) -> Rect {
        crate::ui::centered_popup_rect(
            self.screen_rect(),
            crate::ui::SETTINGS_POPUP_WIDTH,
            crate::ui::settings_popup_height(self),
        )
        .unwrap_or_default()
    }

    fn settings_inner_rect(&self) -> Rect {
        let popup = self.settings_popup_rect();
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    }

    fn settings_tab_at(&self, col: u16, row: u16) -> Option<SettingsSection> {
        let inner = self.settings_inner_rect();
        let tab_y = inner.y + 1;
        if row != tab_y {
            return None;
        }
        let mut x = inner.x;
        for section in SettingsSection::ALL {
            let badge_width = if self.settings_section_has_badge(*section) {
                2
            } else {
                0
            };
            let width = section.label().len() as u16 + 2 + badge_width;
            if col >= x && col < x + width {
                return Some(*section);
            }
            x += width + 1;
        }
        None
    }

    pub(crate) fn settings_content_rect(&self) -> Rect {
        let inner = self.settings_inner_rect();
        crate::ui::modal_stack_areas(inner, 3, 2, 0, 1).content
    }

    fn settings_list_index_at(&self, col: u16, row: u16) -> Option<usize> {
        let area = self.settings_content_rect();
        if row < area.y || row >= area.y + area.height || col < area.x || col >= area.x + area.width
        {
            return None;
        }

        match self.settings.section {
            SettingsSection::Theme => {
                let max_visible = area.height as usize;
                let scroll = if self.settings.list.selected >= max_visible {
                    self.settings.list.selected - max_visible + 1
                } else {
                    0
                };
                let idx = scroll + (row - area.y) as usize;
                (idx < THEME_NAMES.len()).then_some(idx)
            }
            SettingsSection::Indicators | SettingsSection::Sound => {
                let list_y = area.y + 3;
                if row >= list_y && row < list_y + 2 {
                    Some((row - list_y) as usize)
                } else {
                    None
                }
            }
            SettingsSection::Toast => {
                let list_y = area.y + 3;
                if row >= list_y && row < list_y + 8 {
                    Some(((row - list_y) / 2) as usize)
                } else {
                    None
                }
            }
            SettingsSection::PaneLabels => {
                let list_y = area.y + 3;
                if row >= list_y && row < list_y + 2 {
                    Some((row - list_y) as usize)
                } else {
                    None
                }
            }
            SettingsSection::Agents => {
                if self.settings.agent_profile_form.is_some() {
                    return None;
                }
                let list_y = area.y + 3;
                let count = agent_settings_entry_count(self) as u16;
                if row >= list_y && row < list_y.saturating_add(count) {
                    Some((row - list_y) as usize)
                } else {
                    None
                }
            }
            SettingsSection::Integrations => None,
        }
    }

    pub(super) fn handle_settings_mouse(&mut self, mouse: MouseEvent) -> Option<SettingsAction> {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(section) = self.settings_tab_at(mouse.column, mouse.row) {
                    if section != SettingsSection::Agents {
                        self.settings.agent_profile_form = None;
                    }
                    self.settings.section = section;
                    self.settings.list.select(match section {
                        SettingsSection::Theme => current_theme_index(&self.theme_name),
                        SettingsSection::Indicators => {
                            status_indicator_index(self.status_indicators)
                        }
                        SettingsSection::Sound => usize::from(!self.sound_enabled()),
                        SettingsSection::Toast => toast_delivery_index(self.toast_delivery()),
                        SettingsSection::PaneLabels => {
                            usize::from(!self.agent_border_labels_enabled())
                        }
                        SettingsSection::Agents => 0,
                        SettingsSection::Integrations => 0,
                    });
                    return None;
                }
                if self.settings.section == SettingsSection::Agents
                    && self.settings.agent_profile_form.is_some()
                {
                    if let Some(field) = self.agent_profile_form_field_at(mouse.row) {
                        if let Some(form) = self.settings.agent_profile_form.as_mut() {
                            form.selected_field = field;
                        }
                        return None;
                    }
                }
                if let Some(idx) = self.settings_list_index_at(mouse.column, mouse.row) {
                    self.settings.list.select(idx);
                    return match self.settings.section {
                        SettingsSection::Theme => {
                            preview_selected_theme(self);
                            None
                        }
                        SettingsSection::Indicators => Some(SettingsAction::SaveStatusIndicators(
                            status_indicator_for_index(idx),
                        )),
                        SettingsSection::Sound => {
                            let enabled = idx == 0;
                            Some(SettingsAction::SaveSound(enabled))
                        }
                        SettingsSection::Toast => {
                            let delivery = toast_delivery_for_index(idx);
                            Some(SettingsAction::SaveToastDelivery(delivery))
                        }
                        SettingsSection::PaneLabels => {
                            let enabled = idx == 0;
                            Some(SettingsAction::SaveAgentBorderLabels(enabled))
                        }
                        SettingsSection::Agents => agent_settings_selected_action(self),
                        SettingsSection::Integrations => None,
                    };
                }

                let inner = self.settings_inner_rect();
                let show_primary = crate::ui::settings_show_primary_action(self);
                let (apply, close) =
                    crate::ui::settings_button_rects(inner, self.settings.section, show_primary);
                let mut buttons = vec![(close, super::modal::ModalAction::Close)];
                if let Some(apply) = apply {
                    buttons.insert(0, (apply, super::modal::ModalAction::Apply));
                }
                match super::modal::modal_action_from_buttons(mouse.column, mouse.row, &buttons) {
                    Some(super::modal::ModalAction::Apply) => apply_settings(self),
                    Some(super::modal::ModalAction::Close) => {
                        cancel_settings(self);
                        None
                    }
                    _ => {
                        cancel_settings(self);
                        None
                    }
                }
            }
            _ => None,
        }
    }

    fn agent_profile_form_field_at(&self, row: u16) -> Option<usize> {
        let form = self.settings.agent_profile_form.as_ref()?;
        let area = self.settings_content_rect();
        let (first_field_y, text_field_count, documents_y, document_y) = if form.is_new() {
            (area.y + 3, 3, None, area.y + 6)
        } else {
            (
                area.y + 4,
                6,
                Some(area.y + 10),
                area.y + 11 + form.linked_markdown.len() as u16,
            )
        };
        let offset = row.checked_sub(first_field_y)? as usize;
        if offset < text_field_count {
            Some(offset)
        } else if documents_y == Some(row) {
            form.documents_field()
        } else if row >= document_y && row < area.y.saturating_add(area.height) {
            Some(form.instructions_field())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};

    use super::super::{app_for_mouse_test, mouse, state_with_workspaces};
    use super::*;

    #[test]
    fn settings_cancel_restores_previewed_theme_from_other_sections() {
        let mut state = state_with_workspaces(&["test"]);
        let original_palette = state.palette.clone();
        let original_theme = state.theme_name.clone();

        open_settings(&mut state);
        update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
        );
        assert_ne!(state.theme_name, original_theme);

        update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
        );
        assert_eq!(
            state.settings.section,
            crate::app::state::SettingsSection::Indicators
        );

        update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
        );

        assert_eq!(state.mode, Mode::Terminal);
        assert_eq!(state.theme_name, original_theme);
        assert_eq!(state.palette.accent, original_palette.accent);
        assert_eq!(state.palette.panel_bg, original_palette.panel_bg);
    }

    #[test]
    fn agents_settings_creates_with_each_native_harness_and_starts_saved_profiles() {
        let mut state = state_with_workspaces(&["test"]);
        state.saved_agent_profiles = vec![crate::app::state::SavedAgentProfile {
            role: "reviewer".to_string(),
            native_cwd: "/tmp/reviewer".to_string(),
            harness: "claude".to_string(),
            replicas_assigned: 0,
        }];
        open_settings_at(&mut state, SettingsSection::Agents);

        assert_eq!(
            update_settings_state(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            ),
            Some(SettingsAction::OpenAgentCreate("codex".to_string()))
        );

        update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
        );
        assert_eq!(
            update_settings_state(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            ),
            Some(SettingsAction::OpenAgentCreate("pi".to_string()))
        );

        update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
        );
        assert_eq!(
            update_settings_state(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            ),
            Some(SettingsAction::OpenAgentCreate("claude".to_string()))
        );

        update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
        );
        assert_eq!(
            update_settings_state(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            ),
            Some(SettingsAction::StartAgentProfile("reviewer".to_string()))
        );
        assert_eq!(
            update_settings_state(
                &mut state,
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::empty()),
            ),
            Some(SettingsAction::OpenAgentEdit("reviewer".to_string()))
        );
        assert_eq!(
            update_settings_state(
                &mut state,
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()),
            ),
            Some(SettingsAction::DeleteAgentProfile("reviewer".to_string()))
        );
    }

    #[test]
    fn agent_profile_form_edits_its_selected_field_and_cycles_harness() {
        let mut state = state_with_workspaces(&["test"]);
        open_settings_at(&mut state, SettingsSection::Agents);
        state.settings.agent_profile_form = Some(AgentProfileForm {
            existing_role: None,
            role: String::new(),
            harness: "codex".to_string(),
            native_cwd: "/tmp".to_string(),
            model: String::new(),
            effort: String::new(),
            apikey_ref: String::new(),
            allowlist: String::new(),
            additional_markdown: Vec::new(),
            linked_markdown: Vec::new(),
            instructions: String::new(),
            instructions_cursor: 0,
            instructions_scroll: 0,
            selected_markdown: None,
            pending_markdown_name: None,
            selected_field: 0,
        });

        update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::empty()),
        );
        assert_eq!(
            state.settings.agent_profile_form.as_ref().unwrap().role,
            "r"
        );
        assert!(insert_agent_profile_form_text(&mut state, "eviewer"));
        assert_eq!(
            state.settings.agent_profile_form.as_ref().unwrap().role,
            "reviewer"
        );

        update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
        );
        update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::Right, KeyModifiers::empty()),
        );
        assert_eq!(
            state.settings.agent_profile_form.as_ref().unwrap().harness,
            "pi"
        );
        assert_eq!(
            update_settings_state(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            ),
            Some(SettingsAction::SaveAgentProfile)
        );
    }

    #[test]
    fn agent_profile_instructions_keep_lines_from_typing_and_paste() {
        let mut state = state_with_workspaces(&["test"]);
        open_settings_at(&mut state, SettingsSection::Agents);
        state.settings.agent_profile_form = Some(AgentProfileForm {
            existing_role: None,
            role: "reviewer".to_string(),
            harness: "codex".to_string(),
            native_cwd: "/tmp".to_string(),
            model: String::new(),
            effort: String::new(),
            apikey_ref: String::new(),
            allowlist: String::new(),
            additional_markdown: Vec::new(),
            linked_markdown: Vec::new(),
            instructions: "first line".to_string(),
            instructions_cursor: "first line".len(),
            instructions_scroll: 0,
            selected_markdown: None,
            pending_markdown_name: None,
            selected_field: 3,
        });

        assert!(insert_agent_profile_form_text(
            &mut state,
            "\r\nsecond line\nthird line"
        ));
        update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );
        assert_eq!(
            state
                .settings
                .agent_profile_form
                .as_ref()
                .unwrap()
                .instructions,
            "first line\nsecond line\nthird line\n"
        );
        assert_eq!(
            update_settings_state(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            ),
            Some(SettingsAction::SaveAgentProfile)
        );
    }

    #[test]
    fn agent_profile_documents_are_named_selected_and_edited_in_the_form() {
        let mut state = state_with_workspaces(&["test"]);
        open_settings_at(&mut state, SettingsSection::Agents);
        state.settings.agent_profile_form = Some(AgentProfileForm {
            existing_role: Some("reviewer".to_string()),
            role: "reviewer".to_string(),
            harness: "codex".to_string(),
            native_cwd: "/tmp".to_string(),
            model: String::new(),
            effort: String::new(),
            apikey_ref: String::new(),
            allowlist: String::new(),
            additional_markdown: Vec::new(),
            linked_markdown: Vec::new(),
            instructions: "base instructions".to_string(),
            instructions_cursor: 0,
            instructions_scroll: 0,
            selected_markdown: None,
            pending_markdown_name: None,
            selected_field: 6,
        });

        update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty()),
        );
        assert!(insert_agent_profile_form_text(&mut state, "review.md"));
        assert_eq!(
            update_settings_state(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            ),
            Some(SettingsAction::CreateAgentProfileMarkdown {
                role: "reviewer".to_string(),
                name: "review.md".to_string(),
            })
        );

        let form = state.settings.agent_profile_form.as_mut().unwrap();
        form.additional_markdown.push(AgentProfileMarkdown {
            name: "review.md".to_string(),
            path: "/state/agent-context/reviewer/review.md".to_string(),
            content: String::new(),
            cursor: 0,
            scroll: 0,
        });
        form.selected_markdown = Some(0);
        form.pending_markdown_name = None;
        form.selected_field = form.instructions_field();

        assert!(insert_agent_profile_form_text(&mut state, "Read the diff."));
        let form = state.settings.agent_profile_form.as_ref().unwrap();
        assert_eq!(form.instructions, "base instructions");
        assert_eq!(form.additional_markdown[0].content, "Read the diff.");

        state
            .settings
            .agent_profile_form
            .as_mut()
            .unwrap()
            .selected_field = 6;
        assert_eq!(
            update_settings_state(
                &mut state,
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()),
            ),
            Some(SettingsAction::DeleteAgentProfileMarkdown {
                role: "reviewer".to_string(),
                name: "review.md".to_string(),
            })
        );
    }

    #[test]
    fn settings_indicator_choice_returns_save_action() {
        let mut state = state_with_workspaces(&["test"]);
        open_settings_at(&mut state, SettingsSection::Indicators);
        state.settings.list.selected = 1;

        let action = update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );

        assert_eq!(
            action,
            Some(SettingsAction::SaveStatusIndicators(
                StatusIndicatorStyle::Symbols
            ))
        );
        assert_eq!(state.status_indicators, StatusIndicatorStyle::Dots);
        assert_eq!(state.mode, Mode::Settings);
    }

    #[test]
    fn settings_sound_toggle_returns_save_action() {
        let mut state = state_with_workspaces(&["test"]);
        open_settings(&mut state);
        state.settings.section = crate::app::state::SettingsSection::Sound;
        state.settings.list.selected = 0;

        let action = update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );

        assert_eq!(action, Some(SettingsAction::SaveSound(true)));
        assert!(!state.sound.enabled);
        assert_eq!(state.mode, Mode::Settings);
    }

    #[test]
    fn settings_tab_cycle_includes_agents_before_integrations() {
        let mut state = state_with_workspaces(&["test"]);
        open_settings_at(&mut state, SettingsSection::PaneLabels);

        update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
        );
        assert_eq!(state.settings.section, SettingsSection::Agents);

        update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
        );
        assert_eq!(state.settings.section, SettingsSection::Integrations);

        update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::empty()),
        );
        assert_eq!(state.settings.section, SettingsSection::Agents);

        update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::empty()),
        );
        assert_eq!(state.settings.section, SettingsSection::PaneLabels);

        update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::empty()),
        );
        assert_eq!(state.settings.section, SettingsSection::Toast);
    }

    #[test]
    fn integrations_enter_does_nothing_when_nothing_needs_install() {
        let mut state = state_with_workspaces(&["test"]);
        open_settings_at(&mut state, SettingsSection::Integrations);

        let enter_action = update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );
        assert_eq!(enter_action, None);

        let space_action = update_settings_state(
            &mut state,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
        );
        assert_eq!(space_action, None);
    }

    #[test]
    fn settings_hover_does_not_change_selection() {
        let mut app = app_for_mouse_test();
        open_settings(&mut app.state);
        app.state.settings.list.select(0);

        let area = app.state.settings_content_rect();
        app.handle_mouse(mouse(MouseEventKind::Moved, area.x + 2, area.y + 2));

        assert_eq!(app.state.settings.list.selected, 0);
    }

    #[test]
    fn integration_update_badge_only_tracks_outdated_recommendations() {
        let mut state = state_with_workspaces(&["test"]);
        state.integration_recommendations = vec![integration_recommendation(
            crate::integration::IntegrationStatusKind::NotInstalled,
            true,
        )];
        assert!(!state.integration_updates_available());

        state.integration_recommendations = vec![integration_recommendation(
            crate::integration::IntegrationStatusKind::NotInstalled,
            false,
        )];
        assert!(!state.integration_updates_available());

        state.integration_recommendations = vec![integration_recommendation(
            crate::integration::IntegrationStatusKind::Current,
            true,
        )];
        assert!(!state.integration_updates_available());

        state.integration_recommendations = vec![integration_recommendation(
            crate::integration::IntegrationStatusKind::Outdated,
            true,
        )];
        assert!(state.integration_updates_available());
    }

    #[test]
    fn settings_tab_hit_area_includes_integration_update_badge() {
        let mut state = state_with_workspaces(&["test"]);
        state.integration_recommendations = vec![integration_recommendation(
            crate::integration::IntegrationStatusKind::Outdated,
            true,
        )];
        open_settings(&mut state);

        let inner = state.settings_inner_rect();
        let tab_y = inner.y + 1;
        let integrations_idx = SettingsSection::ALL
            .iter()
            .position(|section| *section == SettingsSection::Integrations)
            .expect("integrations section should be present");
        let integrations_x = inner.x
            + SettingsSection::ALL[..integrations_idx]
                .iter()
                .map(|section| {
                    let badge_width = if state.settings_section_has_badge(*section) {
                        2
                    } else {
                        0
                    };
                    section.label().len() as u16 + 3 + badge_width
                })
                .sum::<u16>();
        let dotted_width = SettingsSection::Integrations.label().len() as u16 + 4;

        assert_eq!(
            state.settings_tab_at(integrations_x + dotted_width - 1, tab_y),
            Some(SettingsSection::Integrations)
        );
    }

    fn integration_recommendation(
        state: crate::integration::IntegrationStatusKind,
        available: bool,
    ) -> crate::integration::IntegrationRecommendation {
        crate::integration::IntegrationRecommendation {
            target: crate::api::schema::IntegrationTarget::Claude,
            label: "claude",
            command: "claude",
            available,
            path: std::path::PathBuf::from("/tmp/herdr-test-integration"),
            state,
        }
    }
}
