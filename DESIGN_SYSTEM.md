# OhMyHerdr design system

## 1. Project overview

- **Product:** OhMyHerdr
- **Website type:** terminal-native product UI
- **Surface profile:** App / Product UI · Redesign
- **Platform:** Ratatui terminal application
- **Target users:** people managing persistent coding-agent profiles and shared spaces
- **Main design goal:** make durable agent configuration legible without breaking the fast, mouse-first terminal workflow.

## 2. Brand direction

- **Visual style:** dense, calm, terminal-native.
- **Mood and personality:** precise instrumentation; settings should feel like an inspectable control surface, not a wizard.
- **Concept:** a profile is a durable dossier — concise operational settings appear above its readable `AGENTS.md`, so identity, execution settings, and instructions remain visible together.
- **Adversarial review:** this avoids a generic card dashboard by preserving the existing flat terminal panel language and dedicating the majority of the form to the actual instruction document. The former one-line document field was the weak point and is removed.
- **Reference style:** no web references were used; the implementation follows the project’s existing Catppuccin-derived panel, tab, list, and modal primitives.
- **Voice:** direct, concrete, and action-led: “inspect and change”, “not set”, and explicit save/error feedback.

## 3. Color system

The default Catppuccin Mocha palette is semantic and user-configurable; new UI uses the existing palette roles rather than fixed colors.

| Role | Default hex | Usage |
|---|---:|---|
| Dominant | `#181825` | modal panel background |
| Secondary | `#313244` | selected field and document editor background |
| Accent | `#89b4fa` | panel edge and primary action |
| Primary text | `#cdd6f4` | headings and selected content |
| Supporting text | `#7f849c` | descriptions and helper text |

Primary text on the default panel is 12.14:1; supporting text is 4.75:1. The UI must continue to use `Palette` semantic roles so user-selected light and dark themes remain coherent.

## 4. Typography system

- **Personality:** brutalist-technical.
- **Font family:** the host terminal’s monospace font.
- **Scale:** terminal cell size; bold is reserved for headings, selection, and primary actions.
- **Readability:** instructions preserve line breaks and wrap without trimming. No profile document is flattened or silently truncated by the form.

## 5. Layout system

- Settings use the existing centered 84-column panel and persistent tab placement.
- Agent profile layout is: title → description → identity/settings rows → `AGENTS.md` path → scrollable document viewport → contextual key help.
- `AGENTS.md` gets the flexible height; fixed settings consume only one row each.
- Small terminals may clip the viewport, but Page Up/Page Down remain available to review the document.

## 6. Component system

- **Base:** existing Ratatui modal panel, tabs, list, paragraph, and action-button helpers.
- **Profile rows:** one shared selected/unselected row style.
- **Document editor:** a `Paragraph` with preserved whitespace, shared selected-surface background, and logical-line scrolling.
- **Feedback:** existing toast system reports saved or failed profile changes.

## 7. Surface style

- Flat raised panel over dimmed terminal content; no extra nested cards.
- Selection changes background via `surface0`; it does not rely on decorative borders.
- No gradients or glass effects.

## 8. Icon system

- Terminal glyphs already used by the application (`↵`, `←→`, `▶`, `▸`) remain the only icon language.
- Every glyph is accompanied by a text label or keyboard instruction.

## 9. Image and asset rules

- No image assets are used in this terminal surface.

## 10. Interaction system

- `↑`/`↓` or Tab select profile fields; left/right cycle harness and effort choices.
- In `AGENTS.md`, Enter inserts a line break; Ctrl+Enter or the Save button persists settings; Escape cancels.
- Page Up/Page Down scroll the instruction viewport. Left/right and Home/End move the instruction insertion point.
- All failures use the existing labeled toast; no action fails silently.
- No animation is introduced for this terminal surface.

## 11. Accessibility rules

- The whole form is keyboard-operable.
- Text status never depends on color alone.
- Default palette body/support text pair contrast is recorded in §3; theme customization remains semantic.
- `AGENTS.md` preserves user-authored whitespace and is reviewable without copying it elsewhere.

## 12. Anti-slop rules

- Approved palette: the active `Palette` theme only; no hard-coded blue, gradients, or decorative effects in profile settings.
- Pure black or white: not introduced.
- Concept test: the profile dossier remains specific to OhMyHerdr’s persistent-agent workflow.
- Exception: this is a terminal UI, so terminal glyphs replace a web icon library.

## 13. Future UI instructions

Reuse the existing settings panel, palette roles, rows, action buttons, and contextual key hints. Keep persistent profile facts and its instruction document together; do not add a second disconnected profile settings screen.

## 14. Update policy

Update this file when profile layout, keyboard behavior, palette use, or reusable settings components change.
