# OhMyHerdr isolated product handoff

## Result

OhMyHerdr is installed as a separately compiled product and is running. It is
not a wrapper around the stable Herdr server. No commit was made and no
existing checkout changes were reset or discarded.

The product now has the PRD's visible workflow in the **Agents sidebar** and
at **Settings → Agents**:

- Each saved profile renders once with its profile name and native working
  directory path (without a `native cwd:` label). Live agent pane entries
  appear beneath the matching profile.
- The Agents section has a plain **new** control at bottom-left, matching the
  Spaces section's control. It opens the native-profile form with Codex
  selected; the same form switches to Pi or Claude.
- Three native creation choices: Codex, Pi, and Claude.
- A visible form for role, native working directory, harness, and initial
  profile-owned `AGENTS.md` content.
- Saved profiles visibly listed; Enter starts the selected profile in the
  active tab. Clicking a saved profile in an active Space presents **Use space
  cwd** and **Use agent native cwd**; both launches retain the profile-owned
  `AGENTS.md`.
- `e` opens editing for a saved profile's native harness and working directory;
  its owned `AGENTS.md` remains attached.
- OhMyHerdr always prompts for a new Space's native working directory. The
  dialog shows `~` initially and uses home when the field is blank; the Space
  name and directory fields are independently editable.
- Replica allocation reuses the lowest number not owned by a live pane, so a
  terminated `reviewer-replica-4` is reused instead of monotonically creating
  `reviewer-replica-5`.
- Success and failure feedback is shown as an in-app toast.

The screen is covered by ratatui render tests, so the actual rendered output
contains all three creation choices, saved profiles, and the `AGENTS.md` form.

## Installed product

```text
PATH shim       /Users/tech01/.local/bin/ohmyherdr
launcher        /Users/tech01/oh-my-herdr/.local/ohmyherdr/bin/ohmyherdr-handoff
product binary  /Users/tech01/oh-my-herdr/.local/ohmyherdr/bin/ohmyherdr-product-e9488b06ec85d173b52e13299b1bb531c120b6e2a20be68e7fd0dbb3b592b876
service label   com.ohmyherdr.isolated
product PID     10837 (PPID 1 at verification)
product config  /Users/tech01/.config/ohmyherdr
product state   /Users/tech01/.local/state/ohmyherdr
API socket      /Users/tech01/.config/ohmyherdr/ohmyherdr.sock
client socket   /Users/tech01/.config/ohmyherdr/ohmyherdr-client.sock
server log      /Users/tech01/.config/ohmyherdr/ohmyherdr-server.log
```

```text
product SHA-256  e9488b06ec85d173b52e13299b1bb531c120b6e2a20be68e7fd0dbb3b592b876
launcher SHA-256 d24a39f6756114063a30e850e4c504f88eac8565ae3958ac6d4cb9a3ce54bd68
PATH shim SHA-256 22e8538531cb99bcdc4ad00316b1db55a3dc71a4a92de08b59395acf461a6b4a
```

The launcher clears inherited `HERDR_SOCKET_PATH`,
`HERDR_CLIENT_SOCKET_PATH`, and `HERDR_SESSION`, then fixes `HOME` to
`/Users/tech01`. It therefore cannot attach to stable Herdr by accident.

`ohmyherdr --version` and `ohmyherdr status server` both report:

```text
ohmyherdr 0.8.2-ohmyherdr.beta.20260902
protocol: 21
compatible: yes
```

## Stable Herdr audit

```text
stable PID       7810 (PPID 7809 at verification)
stable binary    /Users/tech01/.local/bin/herdr
stable SHA-256   37350546b0012555943b92eaf962665de4e264395baeb44227b8015e8ff5b0d6
stable API       /Users/tech01/.config/herdr/herdr.sock
stable client    /Users/tech01/.config/herdr/herdr-client.sock
```

Stable Herdr was neither stopped nor replaced. Its hash remained exactly the
requested value. Use `ohmyherdr server stop` if the isolated product must be
stopped; never use `herdr server stop` for this task.

## Migrated state and native profiles

Only the old isolated debug state was copied from
`/Users/tech01/oh-my-herdr/.local/ohmyherdr/config-home/herdr-dev` to
`/Users/tech01/.config/ohmyherdr`. Stable state was not imported.

The migrated `reviewer` profile is native Claude and has its own durable
instructions:

```text
/Users/tech01/.config/ohmyherdr/agent-context/reviewer/AGENTS.md
SHA-256 a06499c257eadcb218776ecd9e55c5a09fb3a15de12b76fdca15d19e89b5fa45
```

Every newly created native profile gets a session-owned
`agent-context/<role>/AGENTS.md`. The harness mappings are native: Codex uses
`model_instructions_file`, Pi uses `--append-system-prompt`, and Claude uses
`--append-system-prompt-file`.

## Relevant source changes

The visual path is implemented in:

```text
src/app/state.rs
src/app/input/settings.rs
src/app/input/mod.rs
src/app/input/mouse.rs
src/app/input/sidebar.rs
src/ui/settings.rs
src/ui/sidebar.rs
src/ui/tab_surface.rs
```

The English, Japanese, and Simplified Chinese agent-automation docs now
document the **Agents sidebar new** fast path, profile/path/live-pane layout,
Space native-cwd prompt, click-spawn cwd choice, replica-number reuse, and the
**Settings → Agents** create/edit/start flow.

## Verification

- Product release build used `OHMYHERDR_BUILD=1`,
  `HERDR_BUILD_CHANNEL=ohmyherdr`, `HERDR_BUILD_ID=beta.20260902`,
  `DEVELOPER_DIR=/Library/Developer/CommandLineTools`, and Zig 0.15.
- Product identity and server smoke passed on the private OhMyHerdr socket:
  `ohmyherdr --version` and `ohmyherdr status server` report version
  `0.8.2-ohmyherdr.beta.20260902`, protocol 21, and `compatible: yes`.
- `ohmyherdr agent profiles`, `ohmyherdr agent roster`, and
  `ohmyherdr workspace list` all reached only the private product socket after
  the restart.
- Focused Settings input/render tests: 20 passed; sidebar render tests: 101
  passed; agent-registry tests: 32 passed; the sidebar mouse-path test passed.
- Full serialized binary suite after all final edits: **3,402 passed, 0 failed,
  1 ignored** (41.31s).
- `cargo fmt --all -- --check`, `cargo check --bin herdr`, `git diff --check`,
  and `node website/scripts/docs-preview.mjs check` passed.
- The preview documentation snapshot check validated
  `b5c4a0176e9183924df552eb8aecb94ed5f9e732`.

## Source snapshot

```text
HEAD                  7b675f42af35508eab66ac42fe1598628597a893
original diff SHA-256  2c59c864746c6382d80d2abd45bbc21a540f45eb811db613a1fed802b68e2f85
current diff SHA-256   42741ec41cd58a699646bdc8a28d6c95135148c2b7ca224e846ca053d1dd1486
untracked              src/persist/plugin_command_leases.rs
                       tests/windows_agent_registry.rs
                       HANDOFF-TECH01.md
```

## Remaining blocker

No known blocker remains for the isolated product cutover. The only validation
not captured in an interactive human terminal is a manual click-through; the
rendered UI and its key/mouse state transitions are covered by automated tests,
and the final branded product binary is running on its isolated service. A
fresh interactive zsh resolves `ohmyherdr` at
`/Users/tech01/.local/bin/ohmyherdr`.

## Latest repair: saved-profile spawn menu input deadlock

The first click of a saved profile followed by **Use space cwd** or **Use agent
native cwd** did create the agent, but it left the server-owned TUI in
`ContextMenu` mode after removing the menu. Every following key or click was
therefore routed to an absent context menu instead of the focused pane. This
made the entire UI appear frozen, including Codex's trust prompt.

`src/app/input/modal.rs` now exits the modal after either spawn choice. The
deterministic regression test
`app::input::modal::tests::agent_profile_spawn_menu_action_leaves_modal`
covers the terminal-mode transition.

The isolated product was rebuilt and only `com.ohmyherdr.isolated` was
restarted. The restart intentionally terminated the blocked `ccc` pane along
with the former isolated server; its durable profile and `hiii` Space remain.
The user must start a fresh `ccc` instance after reopening the UI.

```text
product binary  /Users/tech01/oh-my-herdr/.local/ohmyherdr/bin/ohmyherdr-product-e9488b06ec85d173b52e13299b1bb531c120b6e2a20be68e7fd0dbb3b592b876
product SHA-256 e9488b06ec85d173b52e13299b1bb531c120b6e2a20be68e7fd0dbb3b592b876
launcher SHA-256 d24a39f6756114063a30e850e4c504f88eac8565ae3958ac6d4cb9a3ce54bd68
product PID     10837 (PPID 1 at verification)
product sockets /Users/tech01/.config/ohmyherdr/ohmyherdr.sock
                /Users/tech01/.config/ohmyherdr/ohmyherdr-client.sock
stable PID      7810
stable SHA-256  37350546b0012555943b92eaf962665de4e264395baeb44227b8015e8ff5b0d6
```

Validation after the repair:

- `cargo fmt --all -- --check` and `git diff --check` passed.
- `DEVELOPER_DIR=/Library/Developer/CommandLineTools ZIG=/opt/homebrew/opt/zig@0.15/bin/zig cargo test --bin herdr -- --test-threads=1` passed: **3,402 passed, 0 failed, 1 ignored**.
- `ohmyherdr --version`, `ohmyherdr status server`, and `ohmyherdr workspace list` reached only the private OhMyHerdr socket; `hiii` (`w9`) was restored and focused.

The former TUI client disconnected as part of the isolated server restart. In
that terminal, run `ohmyherdr` again to attach to the repaired product. No
stable Herdr process, socket, binary, or state was changed. The current tracked
diff SHA-256 is `42741ec41cd58a699646bdc8a28d6c95135148c2b7ca224e846ca053d1dd1486`.
