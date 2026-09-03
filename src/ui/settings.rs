use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Tabs, Wrap},
    Frame,
};

use super::widgets::{
    action_button_row_rects, centered_popup_rect, modal_stack_areas, panel_contrast_fg,
    render_action_button, render_modal_choice_list, render_panel_shell, ActionButtonSpec,
};
use crate::{
    app::{state::Palette, AppState},
    config::{StatusIndicatorStyle, ToastDelivery},
};

pub(crate) const SETTINGS_POPUP_WIDTH: u16 = 84;
pub(crate) const SETTINGS_POPUP_BASE_HEIGHT: u16 = 22;

pub(crate) fn settings_popup_height(app: &AppState) -> u16 {
    match app.settings.section {
        crate::app::state::SettingsSection::Agents => {
            let rows = if app.settings.agent_profile_form.is_some() {
                20
            } else {
                3 + app.saved_agent_profiles.len() as u16
            };
            (14 + rows).max(SETTINGS_POPUP_BASE_HEIGHT)
        }
        crate::app::state::SettingsSection::Integrations => {
            let list_rows = app.integration_recommendations.len().max(1) as u16;
            let footer_rows = integrations_footer_height(app, SETTINGS_POPUP_WIDTH - 2);
            // borders 2 + header 3 + stack gaps 2 + modal footer 2
            // + section title 1 + description 2 + spacers 2
            (14 + list_rows + footer_rows).max(SETTINGS_POPUP_BASE_HEIGHT)
        }
        _ => SETTINGS_POPUP_BASE_HEIGHT,
    }
}

pub(super) fn render_settings_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    use crate::app::state::SettingsSection;

    let p = &app.palette;
    let Some(popup) = centered_popup_rect(area, SETTINGS_POPUP_WIDTH, settings_popup_height(app))
    else {
        return;
    };

    super::dim_background(frame, area);

    let Some(inner) = render_panel_shell(frame, popup, p.accent, p.panel_bg) else {
        return;
    };
    if inner.height < 4 || inner.width < 10 {
        return;
    }

    let stack = modal_stack_areas(inner, 3, 2, 0, 1);
    let header_rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas::<3>(stack.header);

    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            " settings",
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        )])),
        header_rows[0],
    );

    let tab_labels = SettingsSection::ALL.iter().map(|section| {
        if app.settings_section_has_badge(*section) {
            Line::from(vec![
                Span::styled(
                    "● ",
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ),
                Span::raw(section.label()),
            ])
        } else {
            Line::from(section.label())
        }
    });
    let tabs = Tabs::new(tab_labels)
        .select(
            SettingsSection::ALL
                .iter()
                .position(|section| *section == app.settings.section)
                .unwrap_or(0),
        )
        .style(Style::default().fg(p.overlay1))
        .highlight_style(
            Style::default()
                .fg(panel_contrast_fg(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD),
        )
        .divider(" ")
        .padding(" ", " ");
    frame.render_widget(tabs, header_rows[1]);

    let sep = "─".repeat(inner.width as usize);
    frame.render_widget(
        Paragraph::new(Span::styled(&sep, Style::default().fg(p.surface0))),
        header_rows[2],
    );

    let content_area = stack.content;

    match app.settings.section {
        SettingsSection::Theme => {
            render_settings_theme(app, frame, content_area);
        }
        SettingsSection::Indicators => {
            render_modal_choice_list(
                frame,
                content_area,
                "agent status indicators",
                "choose color dots or distinct symbols for each state",
                &[
                    ("color dots  ● ● ● ○ ·", StatusIndicatorStyle::Dots),
                    ("distinct symbols  × ◐ ✓ ○ ·", StatusIndicatorStyle::Symbols),
                ],
                app.status_indicators,
                app.settings.list.selected,
                p,
                1,
            );
        }
        SettingsSection::Sound => {
            render_settings_toggle(
                frame,
                content_area,
                p,
                "sound alerts",
                "play sounds when agents change state in background",
                app.sound_enabled(),
                app.settings.list.selected,
            );
        }
        SettingsSection::Toast => {
            render_modal_choice_list(
                frame,
                content_area,
                "notification popups",
                "choose where background popup notifications should appear",
                &[
                    ("off", ToastDelivery::Off),
                    ("inside herdr", ToastDelivery::Herdr),
                    ("via terminal", ToastDelivery::Terminal),
                    ("via system", ToastDelivery::System),
                ],
                app.toast_delivery(),
                app.settings.list.selected,
                p,
                2,
            );
        }
        SettingsSection::PaneLabels => {
            render_settings_toggle(
                frame,
                content_area,
                p,
                "agent border labels",
                "show detected agent names in split pane borders",
                app.agent_border_labels_enabled(),
                app.settings.list.selected,
            );
        }
        SettingsSection::Agents => render_settings_agents(app, frame, content_area),
        SettingsSection::Integrations => {
            render_settings_integrations(app, frame, content_area);
        }
    }

    if let Some(footer_area) = stack.footer {
        let footer_rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)])
            .areas::<2>(footer_area);
        let primary_label = settings_primary_button_label(app.settings.section);
        let show_primary = settings_show_primary_action(app);
        let (apply_rect, close_rect) =
            settings_button_rects(inner, app.settings.section, show_primary);
        if let Some(apply_rect) = apply_rect {
            render_action_button(
                frame,
                apply_rect,
                Some("↵"),
                primary_label,
                Style::default()
                    .fg(panel_contrast_fg(p))
                    .bg(p.accent)
                    .add_modifier(Modifier::BOLD),
            );
        }
        render_action_button(
            frame,
            close_rect,
            Some("esc"),
            "close",
            Style::default()
                .fg(p.text)
                .bg(p.surface0)
                .add_modifier(Modifier::BOLD),
        );

        let footer_hint = if app
            .settings
            .agent_profile_form
            .as_ref()
            .is_some_and(|form| form.pending_markdown_name.is_some())
        {
            " type .md filename  ↵ creates  esc cancels"
        } else if app
            .settings
            .agent_profile_form
            .as_ref()
            .is_some_and(crate::app::state::AgentProfileForm::instructions_selected)
        {
            " tab selects field  ↵ adds line  ctrl+↵ saves  pgup/pgdn views"
        } else if app.settings.agent_profile_form.is_some() {
            " ↑↓ selects field  ←→ cycles choices  ↵ saves"
        } else {
            " ↑↓ select  tab section"
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                footer_hint,
                Style::default().fg(p.overlay1),
            )])),
            footer_rows[0],
        );
    }
}

pub(crate) fn settings_primary_button_label(
    section: crate::app::state::SettingsSection,
) -> &'static str {
    match section {
        crate::app::state::SettingsSection::Integrations => "install",
        crate::app::state::SettingsSection::Agents => "save",
        _ => "apply",
    }
}

pub(crate) fn settings_show_primary_action(app: &AppState) -> bool {
    match app.settings.section {
        crate::app::state::SettingsSection::Integrations => app
            .integration_recommendations
            .iter()
            .any(crate::integration::IntegrationRecommendation::needs_install),
        crate::app::state::SettingsSection::Agents => app.settings.agent_profile_form.is_some(),
        _ => true,
    }
}

pub(crate) fn settings_button_rects(
    inner: Rect,
    section: crate::app::state::SettingsSection,
    show_primary: bool,
) -> (Option<Rect>, Rect) {
    if !show_primary {
        let rects = action_button_row_rects(
            inner,
            &[ActionButtonSpec {
                hint: Some("esc"),
                label: "close",
            }],
            2,
            inner.height.saturating_sub(1),
        );
        return (None, rects[0]);
    }

    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: settings_primary_button_label(section),
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "close",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (Some(rects[0]), rects[1])
}

fn integrations_footer_paragraph(app: &AppState) -> Paragraph<'static> {
    let p = &app.palette;
    let mut footer_lines = Vec::new();
    if !app.integration_install_messages.is_empty() {
        for message in &app.integration_install_messages {
            footer_lines.push(Line::from(Span::styled(
                format!(" {message}"),
                Style::default().fg(p.overlay1),
            )));
        }
    } else {
        let found_any = app.integration_recommendations.iter().any(|item| {
            item.available || item.state != crate::integration::IntegrationStatusKind::NotInstalled
        });
        let hint = if app
            .integration_recommendations
            .iter()
            .any(crate::integration::IntegrationRecommendation::needs_install)
        {
            " press install to add available or outdated integrations"
        } else if found_any {
            " all detected integrations are installed"
        } else {
            " no supported agent CLIs found on PATH"
        };
        footer_lines.push(Line::from(Span::styled(
            hint.to_string(),
            Style::default().fg(p.overlay1),
        )));
    }
    Paragraph::new(footer_lines).wrap(ratatui::widgets::Wrap { trim: false })
}

fn integrations_footer_height(app: &AppState, width: u16) -> u16 {
    (integrations_footer_paragraph(app).line_count(width) as u16).min(6)
}

fn render_settings_integrations(app: &AppState, frame: &mut Frame, area: Rect) {
    let p = &app.palette;

    let footer = integrations_footer_paragraph(app);
    let footer_height = integrations_footer_height(app, area.width);

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(footer_height),
    ])
    .areas::<6>(area);

    frame.render_widget(
        Paragraph::new("agent integrations")
            .style(Style::default().fg(p.text).add_modifier(Modifier::BOLD)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(
            "let agents report state directly instead of relying only on process detection",
        )
        .style(Style::default().fg(p.overlay1))
        .wrap(ratatui::widgets::Wrap { trim: false }),
        rows[1],
    );

    let mut lines = Vec::new();
    for item in &app.integration_recommendations {
        let marker = match item.state {
            crate::integration::IntegrationStatusKind::Current => "✓",
            crate::integration::IntegrationStatusKind::Outdated => "↻",
            crate::integration::IntegrationStatusKind::NotInstalled if item.available => "+",
            crate::integration::IntegrationStatusKind::NotInstalled => "–",
        };
        let marker_style = match item.state {
            crate::integration::IntegrationStatusKind::Current => Style::default().fg(p.green),
            crate::integration::IntegrationStatusKind::Outdated => Style::default().fg(p.yellow),
            crate::integration::IntegrationStatusKind::NotInstalled if item.available => {
                Style::default().fg(p.accent)
            }
            crate::integration::IntegrationStatusKind::NotInstalled => {
                Style::default().fg(p.overlay0)
            }
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} "), marker_style),
            Span::styled(
                format!("{:<9}", item.label),
                Style::default().fg(p.subtext0),
            ),
            Span::styled(item.status_label(), Style::default().fg(p.overlay1)),
        ]));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            " no integration targets available",
            Style::default().fg(p.overlay1),
        )));
    }

    frame.render_widget(Paragraph::new(lines), rows[3]);
    frame.render_widget(footer, rows[5]);
}

fn render_settings_agents(app: &AppState, frame: &mut Frame, area: Rect) {
    let p = &app.palette;
    if let Some(form) = &app.settings.agent_profile_form {
        render_agent_profile_form(form, frame, area, p);
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<4>(area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "saved agents",
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        ))),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "enter starts • e edits • d deletes a saved profile",
            Style::default().fg(p.overlay1),
        ))),
        rows[1],
    );

    let mut entries = vec![
        ListItem::new(Line::from(Span::styled(
            " + create Codex agent",
            Style::default().fg(p.accent),
        ))),
        ListItem::new(Line::from(Span::styled(
            " + create Pi agent",
            Style::default().fg(p.accent),
        ))),
        ListItem::new(Line::from(Span::styled(
            " + create Claude agent",
            Style::default().fg(p.accent),
        ))),
    ];
    entries.extend(app.saved_agent_profiles.iter().map(|profile| {
        ListItem::new(Line::from(vec![
            Span::styled(" ▶ ", Style::default().fg(p.green)),
            Span::styled(&profile.role, Style::default().fg(p.text)),
            Span::styled(
                format!("  {}  {}", profile.harness, profile.native_cwd),
                Style::default().fg(p.overlay1),
            ),
        ]))
    }));

    let list = List::new(entries)
        .highlight_style(
            Style::default()
                .bg(p.surface0)
                .fg(p.text)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" ▸ ");
    let mut state = ListState::default().with_selected(Some(app.settings.list.selected));
    frame.render_stateful_widget(list, rows[3], &mut state);
}

fn render_agent_profile_form(
    form: &crate::app::state::AgentProfileForm,
    frame: &mut Frame,
    area: Rect,
    p: &Palette,
) {
    let title = if form.is_new() {
        "new agent profile"
    } else {
        "edit agent profile"
    };
    let description = if form.is_new() {
        "choose a role, working directory, and profile-owned AGENTS.md"
    } else {
        "inspect and change saved native settings and profile documents"
    };
    let mut details = vec![
        Line::from(Span::styled(
            title,
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(description, Style::default().fg(p.overlay1))),
        Line::from(""),
    ];

    if form.is_new() {
        details.push(agent_profile_form_line(
            "role",
            &form.role,
            form.selected_field == 0,
            p,
        ));
        details.push(agent_profile_form_line(
            "native harness",
            &format!("{}  (←→ to change)", form.harness),
            form.selected_field == 1,
            p,
        ));
        details.push(agent_profile_form_line(
            "native cwd",
            &form.native_cwd,
            form.selected_field == 2,
            p,
        ));
    } else {
        details.push(Line::from(vec![
            Span::styled(" role: ", Style::default().fg(p.overlay1)),
            Span::styled(&form.role, Style::default().fg(p.text)),
        ]));
        details.push(agent_profile_form_line(
            "native harness",
            &format!("{}  (←→ to change)", form.harness),
            form.selected_field == 0,
            p,
        ));
        details.push(agent_profile_form_line(
            "native cwd",
            &form.native_cwd,
            form.selected_field == 1,
            p,
        ));
        details.push(agent_profile_form_line(
            "model",
            display_agent_profile_value(&form.model, "harness default"),
            form.selected_field == 2,
            p,
        ));
        details.push(agent_profile_form_line(
            "effort",
            display_agent_profile_value(&form.effort, "harness default (←→ to change)"),
            form.selected_field == 3,
            p,
        ));
        details.push(agent_profile_form_line(
            "API key ref",
            display_agent_profile_value(&form.apikey_ref, "not set"),
            form.selected_field == 4,
            p,
        ));
        details.push(agent_profile_form_line(
            "tool allowlist",
            display_agent_profile_value(&form.allowlist, "not set"),
            form.selected_field == 5,
            p,
        ));
        if let Some(name) = &form.pending_markdown_name {
            details.push(agent_profile_form_line(
                "new .md",
                if name.is_empty() {
                    "type a filename ending in .md"
                } else {
                    name
                },
                form.documents_selected(),
                p,
            ));
        } else {
            let documents = std::iter::once("AGENTS.md")
                .chain(
                    form.additional_markdown
                        .iter()
                        .map(|markdown| markdown.name.as_str()),
                )
                .collect::<Vec<_>>()
                .join("  ");
            details.push(agent_profile_form_line(
                "documents",
                &format!("{documents}  (a add • ←→ select • d delete)"),
                form.documents_selected(),
                p,
            ));
        }
        if !form.linked_markdown.is_empty() {
            details.push(Line::from(Span::styled(
                format!(" linked Markdown: {}", form.linked_markdown.join(", ")),
                Style::default().fg(p.overlay1),
            )));
        }
    }

    let details_height = details.len() as u16;
    let rows = Layout::vertical([
        Constraint::Length(details_height),
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .areas::<4>(area);
    frame.render_widget(Paragraph::new(details), rows[0]);

    let document_title = format!(" {}", form.active_document_name());
    let document_style = if form.instructions_selected() {
        Style::default().fg(p.text).bg(p.surface0)
    } else {
        Style::default().fg(p.subtext0)
    };
    frame.render_widget(
        Paragraph::new(document_title).style(document_style),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(form.active_document_content())
            .style(document_style)
            .wrap(Wrap { trim: false })
            .scroll((
                form.active_document_scroll().min(u16::MAX as usize) as u16,
                0,
            )),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(if form.pending_markdown_name.is_some() {
            " enter creates document • esc cancels"
        } else if form.instructions_selected() {
            " ←→ moves cursor • home/end line • enter line break • ctrl+enter saves"
        } else if form.documents_selected() {
            " a adds a document • ←→ selects • d deletes selected document"
        } else {
            " enter saves • esc cancels"
        })
        .style(Style::default().fg(p.overlay0)),
        rows[3],
    );
}

fn agent_profile_form_line(label: &str, value: &str, selected: bool, p: &Palette) -> Line<'static> {
    let style = if selected {
        Style::default().fg(p.text).bg(p.surface0)
    } else {
        Style::default().fg(p.subtext0)
    };
    Line::from(vec![
        Span::styled(format!(" {}: ", label), style),
        Span::styled(value.to_string(), style),
    ])
}

fn display_agent_profile_value<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() {
        fallback
    } else {
        value
    }
}

fn render_settings_theme(app: &AppState, frame: &mut Frame, area: Rect) {
    use crate::app::state::THEME_NAMES;

    let p = &app.palette;
    let items: Vec<ListItem> = THEME_NAMES
        .iter()
        .map(|name| {
            let is_current = name.to_lowercase().replace([' ', '_'], "-")
                == app.theme_name.to_lowercase().replace([' ', '_'], "-");
            let marker = if is_current { " ✓" } else { "" };
            ListItem::new(Line::from(vec![
                Span::styled(*name, Style::default().fg(p.subtext0)),
                Span::styled(marker, Style::default().fg(p.green)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(p.surface0)
                .fg(p.text)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" ▸ ")
        .style(Style::default().fg(p.subtext0));

    let mut state = ListState::default().with_selected(Some(app.settings.list.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_settings_toggle(
    frame: &mut Frame,
    area: Rect,
    p: &Palette,
    title: &str,
    description: &str,
    current_value: bool,
    selected_idx: usize,
) {
    render_modal_choice_list(
        frame,
        area,
        title,
        description,
        &[("on", true), ("off", false)],
        current_value,
        selected_idx,
        p,
        1,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, layout::Rect, Terminal};

    use super::render_settings_overlay;
    use crate::app::{
        state::{
            AgentProfileForm, AgentProfileMarkdown, AppState, SavedAgentProfile, SettingsSection,
        },
        Mode,
    };

    fn rendered_text(app: &AppState) -> String {
        let area = Rect::new(0, 0, 120, 36);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|frame| render_settings_overlay(app, frame, area))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn agents_settings_visibly_offers_native_harnesses_and_saved_profiles() {
        let mut app = AppState::test_new();
        app.mode = Mode::Settings;
        app.settings.section = SettingsSection::Agents;
        app.saved_agent_profiles = vec![SavedAgentProfile {
            role: "reviewer".to_string(),
            native_cwd: "/tmp/reviewer".to_string(),
            harness: "claude".to_string(),
            replicas_assigned: 2,
        }];

        let rendered = rendered_text(&app);
        for expected in [
            "create Codex agent",
            "create Pi agent",
            "create Claude agent",
            "reviewer",
            "enter starts • e edits • d deletes a saved profile",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}: {rendered:?}"
            );
        }
    }

    #[test]
    fn agents_settings_visibly_renders_the_create_form_and_agents_file() {
        let mut app = AppState::test_new();
        app.mode = Mode::Settings;
        app.settings.section = SettingsSection::Agents;
        app.settings.agent_profile_form = Some(AgentProfileForm {
            existing_role: None,
            role: "reviewer".to_string(),
            harness: "pi".to_string(),
            native_cwd: "/workspace".to_string(),
            model: String::new(),
            effort: String::new(),
            apikey_ref: String::new(),
            allowlist: String::new(),
            additional_markdown: Vec::new(),
            linked_markdown: Vec::new(),
            instructions: "review this change".to_string(),
            instructions_cursor: "review this change".len(),
            instructions_scroll: 0,
            selected_markdown: None,
            pending_markdown_name: None,
            selected_field: 0,
        });

        let rendered = rendered_text(&app);
        for expected in [
            "new agent profile",
            "reviewer",
            "pi",
            "/workspace",
            "AGENTS.md",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}: {rendered:?}"
            );
        }
    }

    #[test]
    fn agents_settings_visibly_renders_profile_details_and_multiline_instructions() {
        let mut app = AppState::test_new();
        app.mode = Mode::Settings;
        app.settings.section = SettingsSection::Agents;
        app.settings.agent_profile_form = Some(AgentProfileForm {
            existing_role: Some("reviewer".to_string()),
            role: "reviewer".to_string(),
            harness: "claude".to_string(),
            native_cwd: "/workspace/reviewer".to_string(),
            model: "sonnet".to_string(),
            effort: "high".to_string(),
            apikey_ref: "env:REVIEWER_API_KEY".to_string(),
            allowlist: r#"{"tools":["Read"]}"#.to_string(),
            additional_markdown: vec![AgentProfileMarkdown {
                name: "review-guide.md".to_string(),
                path: "/state/agent-context/reviewer/review-guide.md".to_string(),
                content: "Read the diff before replying.".to_string(),
                cursor: 0,
                scroll: 0,
            }],
            linked_markdown: Vec::new(),
            instructions: "# Reviewer\n\nCheck every changed file.".to_string(),
            instructions_cursor: 0,
            instructions_scroll: 0,
            selected_markdown: None,
            pending_markdown_name: None,
            selected_field: 7,
        });

        let rendered = rendered_text(&app);
        for expected in [
            "edit agent profile",
            "claude",
            "/workspace/reviewer",
            "sonnet",
            "high",
            "env:REVIEWER_API_KEY",
            "documents",
            "review-guide.md",
            "# Reviewer",
            "Check every changed file.",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}: {rendered:?}"
            );
        }

        let form = app.settings.agent_profile_form.as_mut().unwrap();
        form.selected_markdown = Some(0);
        form.selected_field = form.instructions_field();
        let rendered = rendered_text(&app);
        for expected in ["review-guide.md", "Read the diff before replying."] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}: {rendered:?}"
            );
        }
    }
}
