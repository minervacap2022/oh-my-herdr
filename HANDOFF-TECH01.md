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

## 2026-09-02 follow-up: delete profiles and product-only update routing

The current uncommitted tree adds a complete saved-agent deletion path. It is
intentionally profile deletion, not a hidden way to close arbitrary live panes:

- In the Agents sidebar, click a saved profile and choose **Delete profile**.
  The same menu offers **Use space cwd**, **Use agent native cwd**, and
  **Edit profile**.
- In **Settings → Agents**, select a saved profile. The rendered instruction is
  `enter starts • e edits • d deletes a saved profile`; press `d` to delete it.
- The API is `agent.profile.delete`, and the explicit command-line escape hatch
  is `ohmyherdr agent profile delete <role>`.

Deleting removes the saved profile, its roster records, and its
profile-owned `agent-context/<role>/AGENTS.md`. It does not issue a pane-close
request; use the existing **Close pane** action for a running pane.

A live isolated-product smoke test created the temporary `deleteme` Codex
profile with native cwd `/tmp`, deleted it through the new command, verified
the `agent_profile_deleted` response, and then verified that a subsequent get
returned `agent_profile_not_found`. Its temporary context directory is gone.
No existing profile was used for this test.

The production launcher no longer points at a hash-named build artifact:

```text
launcher        /Users/tech01/oh-my-herdr/.local/ohmyherdr/bin/ohmyherdr-handoff
launcher SHA-256 aa5693907f182e9a56ca933039e767a08d4587bc09cd8b7811b5896fdcbc8b00
next product exe /Users/tech01/oh-my-herdr/.local/ohmyherdr/bin/ohmyherdr
next product SHA-256 5ff6909c59fba5c2652dcc4943b53ff48a7d17ea7ac32f10a6a354882b28d984
PATH shim       /Users/tech01/.local/bin/ohmyherdr
PATH shim SHA-256 22e8538531cb99bcdc4ad00316b1db55a3dc71a4a92de08b59395acf461a6b4a
```

That fixed executable path is the target the native updater atomically
replaces, so future upgrades do not accumulate new hash-named launch targets.

### Important runtime event

I restarted `com.ohmyherdr.isolated` once to put the new deletion UI into the
running product. That was a mistake while live product panes existed: it
terminated their prior processes. The isolated session restored its four
Spaces and launched replacement panes, but prior in-flight agent processes
were interrupted. I did **not** restart it a second time for the OTA routing
change; PID `12425` remains running. The next product restart will load the
on-disk `5ff690…` binary. Stable Herdr was unaffected:

```text
stable PID       7810 (PPID 7809)
stable binary    /Users/tech01/.local/bin/herdr
stable SHA-256   37350546b0012555943b92eaf962665de4e264395baeb44227b8015e8ff5b0d6
stable sockets   /Users/tech01/.config/herdr/herdr.sock
                 /Users/tech01/.config/herdr/herdr-client.sock
product PID      12425 (PPID 1)
product sockets  /Users/tech01/.config/ohmyherdr/ohmyherdr.sock
                 /Users/tech01/.config/ohmyherdr/ohmyherdr-client.sock
```

### Native OTA status

The source now makes the compile-time OhMyHerdr build use only this product
endpoint for both direct update checks and remote binary lookup:

```text
https://github.com/minervacap2022/oh-my-herdr/releases/latest/download/ohmyherdr-latest.json
```

It never falls back to `https://herdr.dev/latest.json`, forces the product to
its stable product channel (so a copied `preview` config cannot select Herdr's
preview feed), and prints `ohmyherdr update` / `ohmyherdr server stop` in its
guidance. The manifest-selection test covers the stable-vs-product separation
under both ordinary and `OHMYHERDR_BUILD=1` compilation. The remote installer
uses the same selector.

There is one real blocker to a downloadable OTA: `minervacap2022/oh-my-herdr`
currently has **no GitHub Release** and therefore no
`ohmyherdr-latest.json` asset. An anonymous request to the exact product URL
returned HTTP 404 on 2026-09-02. A release publisher must publish that manifest
with platform assets and SHA-256 values at the endpoint (and make that endpoint
reachable to intended product users). Until then the code is isolated and safe
from Herdr, but a real network update cannot be completed or end-to-end tested.

### Follow-up validation

- `cargo fmt --all -- --check`, `cargo check --bin herdr`, and `git diff --check` passed.
- Full serialized binary suite: **3,404 passed, 0 failed, 1 ignored**.
- Product-compiled test (`OHMYHERDR_BUILD=1`): product manifest selection and
  product session command tests passed.
- The branded launcher’s help and `status server` reach only the OhMyHerdr
  product socket; the isolated server reports protocol 21 and compatibility.
- No commit was made. The working tree has 24 modified tracked files; all are
  the deletion, product-command, and product-update isolation work described
  above.

## 2026-09-03 follow-up: readable profile details and multiline AGENTS.md

The saved-agent editor previously had three product defects:

1. Opening an existing profile deliberately loaded `instructions: String::new()`,
   so its profile-owned `AGENTS.md` could not be viewed.
2. Pasted newlines were filtered out, and Enter always saved the whole form.
   Long instructions therefore appeared as one line and could not be edited as
   a document.
3. The edit screen only exposed the harness and native cwd, even though model,
   effort, API-key reference, tool allowlist, and additional injected Markdown
   are all durable profile settings.

The Agents settings flow now opens a profile dossier through the existing **e
edit** action (and the existing sidebar **Edit profile** action): it renders
and edits harness, native cwd, model, effort, API-key reference, and tool
allowlist; lists additional Markdown attachments; displays the owned
`AGENTS.md` path; and renders the real, newline-preserving document in a
scrollable viewport. While that document is selected:

- **Enter** inserts a line break.
- **Ctrl+Enter** saves.
- **Page Up/Page Down** scrolls.
- **Left/Right** and **Home/End** move the insertion point.

Profile instruction writes are atomic replacements of the existing owned file;
NUL input is rejected and cannot partially overwrite the previous document.

Source and test additions:

- `src/app/input/settings.rs` — profile details, all editable saved settings,
  multiline input/navigation/paste behavior.
- `src/ui/settings.rs` — compact settings rows above a flexible, readable
  `AGENTS.md` viewport.
- `src/agent_registry.rs` — owned-instruction path/read/atomic replacement.
- `DESIGN_SYSTEM.md` — the terminal-native profile dossier layout and existing
  palette/component rules for future UI work.

Verification on 2026-09-03:

- Full serialized binary suite: **3,408 passed, 0 failed, 1 ignored**.
- Added deterministic coverage for multiline paste + Enter/Ctrl+Enter, rendered
  profile details + multiline document, and atomic instruction replacement.
- `cargo fmt --all -- --check` and `git diff --check` passed.

### Product artifact state

The verified branded product build was staged atomically at:

```text
/Users/tech01/oh-my-herdr/.local/ohmyherdr/bin/ohmyherdr
SHA-256 4995fe97ee92694d0958a04cb8ce61074f5056a64b8283d0ab709009be0af92f
```

It was built with:

```text
DEVELOPER_DIR=/Library/Developer/CommandLineTools
ZIG=/opt/homebrew/opt/zig@0.15/bin/zig
OHMYHERDR_BUILD=1
HERDR_BUILD_CHANNEL=ohmyherdr
HERDR_BUILD_ID=beta.20260902
```

The previously recorded `/Applications/Xcode-26.6.0.app/Contents/Developer`
path no longer exists, so an initial build attempt stopped before staging. The
Command Line Tools rebuild succeeded.

**The running isolated server was intentionally not restarted or controlled.**
This agent was outside a Herdr-managed pane, and the running product owns live
agent panes. Its PID remains `12425`; it still holds the pre-stage executable
inode (19,848,736 bytes), while the new on-disk file will be used by the next
safe live handoff or normal product restart. The isolated product is healthy:

```text
product PID     12425
product socket  /Users/tech01/.config/ohmyherdr/ohmyherdr.sock
client socket   /Users/tech01/.config/ohmyherdr/ohmyherdr-client.sock
protocol        21 (live handoff supported)
```

Stable Herdr remains separate and unchanged:

```text
stable binary   /Users/tech01/.local/bin/herdr
stable SHA-256  37350546b0012555943b92eaf962665de4e264395baeb44227b8015e8ff5b0d6
stable PID      7810
stable socket   /Users/tech01/.config/herdr/herdr.sock
protocol        17
```

Current uncommitted binary diff SHA-256:

```text
80632b148b7c3c15913a56dbfbf2ac26d41d6bc4fa6d9464310990d9ab664956
```

## 2026-09-03 production OTA release

The user explicitly approved publishing this product. The product repository is
now public at `https://github.com/minervacap2022/oh-my-herdr`; stable Herdr's
repository, binary, server, socket, and update endpoint were not changed.

The first product OTA is version `0.8.3`. The release workflow is deliberately
separate from the upstream `v*` release workflow:

```text
.github/workflows/ohmyherdr-release.yml
tag: ohmyherdr-v0.8.3
manifest asset: ohmyherdr-latest.json
artifact: ohmyherdr-macos-aarch64
```

It builds with `OHMYHERDR_BUILD=1`, publishes the product-only manifest URL
used by the native updater, and includes the artifact SHA-256 in that manifest.
The custom tag cannot trigger the upstream `Release` workflow because it does
not start with `v`.

Local production build verification passed with:

```text
version: ohmyherdr 0.8.3-ohmyherdr.0.8.3
SHA-256: 11e8fcd289f679a7cb0bd9d8bb74716ccbc77d393f1aa4e072757f0c15f4ac5b
target: aarch64-apple-darwin
```

This first product release intentionally advertises only `macos-aarch64`.
Publishing a Windows artifact before making its installer use an independent
OhMyHerdr install path would risk writing to stable Herdr's install directory,
which is prohibited. Linux and Intel macOS are likewise not advertised until
their product-native installation paths are released and verified.

### Publication result

The public production release is now live:

```text
release URL  https://github.com/minervacap2022/oh-my-herdr/releases/tag/ohmyherdr-v0.8.3
tag          ohmyherdr-v0.8.3
target       e48325551b903382ab6760dc4fcf17b50794ddb4
artifact     ohmyherdr-macos-aarch64
SHA-256      11e8fcd289f679a7cb0bd9d8bb74716ccbc77d393f1aa4e072757f0c15f4ac5b
manifest     https://github.com/minervacap2022/oh-my-herdr/releases/latest/download/ohmyherdr-latest.json
```

The manifest was fetched anonymously from its native endpoint, parsed as JSON,
and confirmed to advertise version `0.8.3`, protocol `21`, the pinned asset
URL, and the same SHA-256. The artifact was separately downloaded anonymously;
its checksum matched and it reported `ohmyherdr 0.8.3-ohmyherdr.0.8.3`.

GitHub Actions remains disabled by the `minervacap2022` organization policy,
so the checked-in manual release workflow cannot run until an organization
administrator enables Actions. The first release was published directly from
the verified local product build; native OTA is live despite that automation
policy. The queued workflow run was never allowed to start.

## 2026-09-03 profile document cabinet

The next OhMyHerdr product build is `0.8.4`. Each saved profile now owns a
small editable document cabinet under its existing private state directory:

```text
/Users/tech01/.config/ohmyherdr/agent-context/<profile>/AGENTS.md
/Users/tech01/.config/ohmyherdr/agent-context/<profile>/<user-name>.md
```

`Settings → Agents → e` opens directly on the real `AGENTS.md` contents and
places the insertion point in that editor. It no longer displays the path as a
substitute for the document. The document row lists `AGENTS.md` and each
profile-owned extra file: `a` creates a named `.md`, left/right changes the
active document, and `d` removes the selected extra document (never
`AGENTS.md`). Ctrl+Enter saves the current profile settings and every edited
profile document.

Documents are not copied or symlinked into Spaces. They stay profile-owned and
the profile spawn path injects every registered document for Codex, Pi, and
Claude, regardless of whether the launch uses the Space CWD or the profile's
native CWD. Existing externally attached Markdown remains visible as a linked
attachment but is not rewritten by the profile document editor.

Verification before publication:

```text
cargo test --bin herdr -- --test-threads=1
3410 passed, 1 ignored

product build identity  ohmyherdr 0.8.4-ohmyherdr.0.8.4
product build SHA-256   f6f4516a23aac3142ff8e009730c10a86b48bba2712c54b3bf4ed137c96457fd
```

This change has not stopped, restarted, or modified stable Herdr. The stable
binary and server values above remain the required guardrails when installing
or testing this next product OTA.

## 2026-09-04 OTA live-handoff identity repair

The product updater's manifest used the plain release number (for example,
`0.8.5`) as the expected live-handoff identity. A product binary correctly
reports its build identity as `0.8.5-ohmyherdr.0.8.5`. The server rejects a
handoff when those values differ, so the old server safely rolled back after
installing an update rather than replacing live agent panes.

`src/update.rs` now derives the expected manifest identity from the active
product build: upstream Herdr continues to use a plain version and OhMyHerdr
uses `<version>-ohmyherdr.<version>`. The deterministic unit test covers both
forms.

The isolated named-session E2E proof used the old product `0.8.2` server and
the new `0.8.5` product binary:

```text
before handoff  0.8.2-ohmyherdr.beta.20260902
after handoff   0.8.5-ohmyherdr.0.8.5
protocol        21
socket          /Users/tech01/.config/ohmyherdr/sessions/ota-handoff-084/ohmyherdr.sock
```

The disposable `ota-handoff-084` session was stopped and deleted immediately
after the proof. The default isolated product server stayed on PID `12425`,
and stable Herdr stayed on PID `7810` with the required SHA-256
`37350546b0012555943b92eaf962665de4e264395baeb44227b8015e8ff5b0d6`.

Release-candidate verification:

```text
cargo fmt --all -- --check
cargo test --bin herdr -- --test-threads=1
3411 passed, 0 failed, 1 ignored

product identity  ohmyherdr 0.8.5-ohmyherdr.0.8.5
product SHA-256   9021f7757e50b477043478e44700ba775759c289673a27d1bf42a29b5b36eed0
```

### Publication and default-product handoff result

OhMyHerdr `0.8.5` is public and its native update manifest and binary were
downloaded anonymously, checksum-verified, and version-checked:

```text
release      https://github.com/minervacap2022/oh-my-herdr/releases/tag/ohmyherdr-v0.8.5
manifest     https://github.com/minervacap2022/oh-my-herdr/releases/latest/download/ohmyherdr-latest.json
artifact     ohmyherdr-macos-aarch64
asset SHA    9021f7757e50b477043478e44700ba775759c289673a27d1bf42a29b5b36eed0
identity     ohmyherdr 0.8.5-ohmyherdr.0.8.5
commit       1047aeedf9cab82bde34c070713675e8d575231c
```

The installed `0.8.4` updater safely downloaded and installed `0.8.5` but,
as expected, its pre-fix client still supplied the old plain identity to the
old `0.8.2` server. It rolled back the handoff without affecting panes. The
new installed `0.8.5` client then completed its native `server live-handoff`
with the corrected identity.

The default product is now live and healthy without a restart:

```text
product executable /Users/tech01/oh-my-herdr/.local/ohmyherdr/bin/ohmyherdr
product SHA-256    9021f7757e50b477043478e44700ba775759c289673a27d1bf42a29b5b36eed0
product PID         2711
product version     0.8.5-ohmyherdr.0.8.5
product protocol    21
product sockets     /Users/tech01/.config/ohmyherdr/ohmyherdr.sock
                    /Users/tech01/.config/ohmyherdr/ohmyherdr-client.sock
```

The same product smoke preserved four Spaces and the existing saved profiles
and live roster entries, including the active `testing` Codex process. Stable
Herdr remains separately live at PID `7810`, version `0.7.5`, protocol `17`,
its original sockets under `/Users/tech01/.config/herdr`, and the required
unchanged SHA-256 `37350546b0012555943b92eaf962665de4e264395baeb44227b8015e8ff5b0d6`.

There is no remaining OTA or product-cutover blocker. New product releases
will use `ohmyherdr update --handoff` directly: their updater calculates the
product identity before it requests the server handoff.
