# OhMyHerdr

<p align="center">
  <img src="assets/logo.png" alt="OhMyHerdr" width="100" />
</p>

<p align="center">
  Native agent profiles, persistent agent instructions, and shared coding spaces.
</p>

<p align="center">
  <a href="#install">install</a> · <a href="#native-agent-profiles">agent profiles</a> · <a href="https://github.com/minervacap2022/oh-my-herdr/releases">releases</a>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-666666?labelColor=333333" alt="Apache 2.0 license" /></a>
  <a href="https://github.com/minervacap2022/oh-my-herdr/releases/latest"><img src="https://img.shields.io/github/v/release/minervacap2022/oh-my-herdr?label=release&labelColor=333333&color=666666" alt="latest release" /></a>
  <a href="https://github.com/minervacap2022/oh-my-herdr/stargazers"><img src="https://img.shields.io/github/stars/minervacap2022/oh-my-herdr?labelColor=333333&color=666666&logo=github" alt="GitHub stars" /></a>
</p>

---

**One runtime for agent collaboration, with every agent's native identity kept
intact.**

- **Native profiles** — save Codex, Pi, and Claude Code profiles with each
  harness's settings, model choices, tool permissions, and native working
  directory.
- **An instruction document per profile** — every saved agent owns a durable,
  editable `AGENTS.md`. Open a profile dossier to read and edit the complete
  multiline document without losing its formatting.
- **Spaces that coordinate without forcing one directory** — a Space has a
  default working directory, but a spawned agent can use either the Space
  directory or its own native directory. Its profile instructions persist in
  both cases.
- **A visual workflow** — create profiles from the Agents sidebar, inspect and
  edit them in Settings, and spawn them into a Space without hardcoded roles or
  duplicate profile rows.
- **Separate product runtime** — OhMyHerdr uses its own binary, server,
  sockets, configuration, sessions, and OTA manifest. It neither shares nor
  interrupts a stable Herdr installation.

## install

The first production release supports Apple Silicon macOS. Install the product
binary separately from any stable Herdr binary:

```bash
install_dir="${OHMYHERDR_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$install_dir"
curl -fsSL \
  https://github.com/minervacap2022/oh-my-herdr/releases/latest/download/ohmyherdr-macos-aarch64 \
  -o "$install_dir/ohmyherdr"
chmod +x "$install_dir/ohmyherdr"
```

Ensure `$HOME/.local/bin` is on `PATH`, then start OhMyHerdr where the work
lives:

```bash
ohmyherdr
```

To install a later product release, run:

```bash
ohmyherdr update
```

The updater reads only OhMyHerdr's public release manifest and verifies the
downloaded binary's SHA-256 before replacing the product executable.

## native agent profiles

1. In the **Agents** sidebar, select **new**.
2. Choose Codex, Pi, or Claude Code and set the profile's native working
   directory.
3. Write the profile's `AGENTS.md` in its dossier. Enter adds a line;
   Ctrl+Enter saves.
4. Open a Space and choose whether the new pane starts in the Space directory
   or the agent's native directory.

Closing a replica frees its number for reuse. Deleting a profile removes its
saved settings and owned instruction document; it does not delete live panes.

## development

```bash
git clone https://github.com/minervacap2022/oh-my-herdr
cd oh-my-herdr
cargo build --release

just test
just check
```

## license

OhMyHerdr is licensed under the [Apache License 2.0](LICENSE).
