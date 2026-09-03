//! Persistent, per-user registry of saved agent profiles plus a roster of every
//! agent that has ever existed in the current Herdr session.
//!
//! This is the foundation for the owlspace-style "saved agent profiles" model:
//! agents are first-class, persistent entities (not just ephemeral pane
//! occupants). It holds two related facts:
//!
//! * [`AgentProfile`] — a saved profile the user clicks to spawn. Its
//!   `{ role, native_cwd, mds }` subset is *persistent context that survives
//!   harness/model/effort/apikey swaps*; `{ harness, model, effort, apikey_ref,
//!   allowlist }` are *switchable settings*.
//! * [`AgentRosterEntry`] — a durable record of an agent instance that existed,
//!   keyed by a stable `profile_id` so `revive`/addressing can target *the exact
//!   agent* even after its pane/terminal is gone.
//!
//! Storage mirrors the session layer: a single `agents.json` in the current
//! session data directory, written atomically (temp + rename) separately from
//! the session snapshot so profile edits never require a full workspace
//! snapshot. Pure data + IO, no PTYs, unit-testable in isolation.
//!
//! See `docs/design/agent-registry-spawn-comms.md` for the full design.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tracing::warn;

/// Format version — bumped for structurally incompatible changes. Newer files
/// are preserved without loading so an older binary cannot overwrite them.
pub const REGISTRY_VERSION: u32 = 2;
const REGISTRY_FILENAME: &str = "agents.json";
const REGISTRY_LOCK_FILENAME: &str = ".agents.lock";
const PROFILE_CONTEXT_DIRECTORY: &str = "agent-context";
const PROFILE_INSTRUCTIONS_FILENAME: &str = "AGENTS.md";
const MAX_NAME_LEN: usize = 32;
static NEXT_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize)]
struct RegistryVersion {
    #[serde(default)]
    version: u32,
}

/// Milliseconds since the Unix epoch.
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// One `.md` the agent always has injected, regardless of pane/cwd. The user
/// authorizes any filename; `path` is where it lives on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMd {
    /// Arbitrary filename the user chooses, e.g. `context.md`, `todo.md`.
    pub name: String,
    /// Where the file lives on disk (user-authored, not managed by herdr).
    pub path: PathBuf,
}

/// Model effort tier. Switchable setting, part of persistent context only in
/// that it is remembered on the profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffortLevel {
    Low,
    #[default]
    Medium,
    High,
}

/// A saved agent profile — the thing shown in the Agents sidebar and clicked to
/// spawn. `{ role, native_cwd, mds }` is persistent context; the rest are
/// switchable settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    /// Unique handle, `[a-z][a-z0-9_-]{0,31}`. Also the profile key.
    pub role: String,
    /// Persistent context: default cwd when spawned with the agent-cwd mode.
    pub native_cwd: PathBuf,
    /// Whether `native_cwd` is an explicit profile value instead of the
    /// temporary cwd used while a profile is created by pre-spawn setup.
    #[serde(default = "default_native_cwd_seeded")]
    pub native_cwd_seeded: bool,
    /// Persistent context: `.md`s always injected at spawn (prompt/flags).
    #[serde(default)]
    pub mds: Vec<AgentMd>,
    /// Switchable setting: which harness runs this agent (codex/claude/...).
    pub harness: String,
    /// Switchable setting.
    pub model: Option<String>,
    /// Switchable setting.
    pub effort: Option<EffortLevel>,
    /// Switchable setting: reference to the secret, never the secret itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apikey_ref: Option<String>,
    /// Switchable setting: tool allowlist. Kept as opaque JSON for now; a typed
    /// schema is phase 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowlist: Option<serde_json::Value>,
    /// Highest replica index ever assigned. Kept for persistence and API
    /// compatibility; new live allocations scan the roster for the lowest free
    /// index so terminated names can be reused.
    pub replicas_assigned: u32,
    /// Created timestamp (ms since epoch).
    pub created_at: i64,
    /// Last spawn timestamp (ms since epoch), if any.
    pub last_spawned_at: Option<i64>,
}

#[allow(dead_code)]
/// Phase 2/3 API: consumed once spawn + comms are wired in.
impl AgentProfile {
    /// Create a fresh profile with defaults. `native_cwd` defaults to `fallback`
    /// when it does not exist.
    pub fn new(role: impl Into<String>, native_cwd: PathBuf, fallback: &Path) -> Self {
        let native_cwd = if native_cwd.exists() {
            native_cwd
        } else {
            fallback.to_path_buf()
        };
        Self {
            role: role.into(),
            native_cwd,
            native_cwd_seeded: true,
            mds: Vec::new(),
            harness: "codex".to_string(),
            model: None,
            effort: None,
            apikey_ref: None,
            allowlist: None,
            replicas_assigned: 0,
            created_at: now_millis(),
            last_spawned_at: None,
        }
    }

    /// Add (or replace) an injected `.md` by its on-disk path, keyed by filename.
    pub fn set_md(&mut self, name: impl Into<String>, path: PathBuf) {
        let name = name.into();
        self.mds.retain(|md| md.name != name);
        self.mds.push(AgentMd { name, path });
        self.mds.sort_by(|a, b| a.name.cmp(&b.name));
    }

    /// Remove an injected `.md` by filename. Returns whether it existed.
    pub fn remove_md(&mut self, name: &str) -> bool {
        let before = self.mds.len();
        self.mds.retain(|md| md.name != name);
        self.mds.len() != before
    }

    /// Stable, unique-looking live agent name using the profile's next
    /// high-water index. Runtime spawn allocation uses the registry-wide lowest
    /// available index instead.
    pub fn next_replica_name(&mut self) -> Option<String> {
        // 1-based: first replica is `role-replica-1`.
        let index = self.replicas_assigned.checked_add(1)?;
        self.replicas_assigned = index;
        Some(format_replica_name(&self.role, index))
    }

    /// Record a replica index that has started without allowing an earlier
    /// deferred completion to move the persisted high-water mark backward.
    pub fn record_replica_assignment(&mut self, index: u32) {
        self.replicas_assigned = self.replicas_assigned.max(index);
    }

    pub fn record_spawn(&mut self, native_cwd: PathBuf) {
        if !self.native_cwd_seeded {
            self.native_cwd = native_cwd;
            self.native_cwd_seeded = true;
        }
        self.last_spawned_at = Some(now_millis());
    }
}

fn default_native_cwd_seeded() -> bool {
    true
}

/// Lifecycle status of a live agent instance tracked in the roster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    #[default]
    Active,
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
    /// Process gone but the entry is kept so history/replica numbering survive.
    Terminated,
}

fn replica_index_from_suffix(replica_suffix: &str) -> Option<u32> {
    let index = replica_suffix.strip_prefix("-replica-")?.parse().ok()?;
    (index > 0).then_some(index)
}

/// A durable record of an agent instance that existed. `instance_id` remains
/// unique even when a primary or replica display name is reused later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRosterEntry {
    /// Stable per-instance identity. It is deliberately separate from the
    /// ephemeral live display name so terminated entries remain archival.
    #[serde(default)]
    pub instance_id: String,
    /// Stable identity == the owning profile's `role`.
    pub profile_id: String,
    /// Role this instance represents.
    pub role: String,
    /// Replica suffix: `""` for the first instance, `"-replica-2"` for the rest.
    pub replica_suffix: String,
    /// The live agent name (`role` + suffix).
    pub display_name: String,
    pub status: AgentStatus,
    /// Last known pane address as a `w:t:p` string, if any.
    pub last_pane: Option<String>,
    /// Last time this instance was observed alive (ms since epoch).
    pub last_seen_at: Option<i64>,
}

/// The full registry: saved profiles (keyed by role) + the roster (keyed by
/// stable instance id). Ordered maps keep the JSON stable for tests and diffs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRegistry {
    /// Format version.
    #[serde(default)]
    pub version: u32,
    /// Saved profiles, keyed by `role`.
    #[serde(default)]
    pub profiles: BTreeMap<String, AgentProfile>,
    /// Roster, keyed by `AgentRosterEntry::instance_id`.
    #[serde(default)]
    pub roster: BTreeMap<String, AgentRosterEntry>,
    /// Monotonic per-registry roster instance counter.
    #[serde(default)]
    next_roster_instance: u64,
}

#[allow(dead_code)]
/// Phase 2/3 API: consumed once spawn + comms are wired in.
impl AgentRegistry {
    /// An empty registry (never persisted with zero entries — `save` writes it,
    /// `clear` removes it).
    pub fn new() -> Self {
        Self {
            version: REGISTRY_VERSION,
            ..Self::default()
        }
    }

    // ---- Profiles -------------------------------------------------------

    pub fn get(&self, role: &str) -> Option<&AgentProfile> {
        self.profiles.get(role)
    }

    pub fn get_mut(&mut self, role: &str) -> Option<&mut AgentProfile> {
        self.profiles.get_mut(role)
    }

    /// Return the lowest replica index that is not currently alive or locally
    /// reserved. Terminated roster history does not consume a live name.
    pub fn next_available_replica_index(
        &self,
        role: &str,
        unavailable_names: &[String],
    ) -> Option<u32> {
        let mut index = 1u32;
        loop {
            let candidate = format_replica_name(role, index);
            if !unavailable_names.iter().any(|name| name == &candidate)
                && self.alive_instance(&candidate).is_none()
            {
                return Some(index);
            }
            index = index.checked_add(1)?;
        }
    }

    /// Return the existing profile, or create one from defaults and insert it.
    /// Returns a mutable reference to whichever it is.
    pub fn register_or_get(
        &mut self,
        role: impl Into<String>,
        native_cwd: PathBuf,
    ) -> &mut AgentProfile {
        let role = role.into();
        self.profiles
            .entry(role.clone())
            .or_insert_with(|| AgentProfile::new(role, native_cwd, Path::new("/")))
    }

    /// Delete a saved profile and any roster entries that belong to it.
    /// Returns the removed profile, if any.
    pub fn remove_profile(&mut self, role: &str) -> Option<AgentProfile> {
        let removed = self.profiles.remove(role)?;
        self.roster.retain(|_, entry| entry.profile_id != role);
        Some(removed)
    }

    // ---- Roster ---------------------------------------------------------

    /// Register (or update) a live instance. A reused display name after an
    /// earlier termination creates a new durable record.
    pub fn roster_register(
        &mut self,
        display_name: &str,
        profile_id: &str,
        role: &str,
        replica_suffix: &str,
        last_pane: Option<String>,
    ) -> Option<&mut AgentRosterEntry> {
        let active_instance_id = self.roster.iter().find_map(|(instance_id, entry)| {
            (entry.display_name == display_name && entry.status != AgentStatus::Terminated)
                .then(|| instance_id.clone())
        });
        let instance_id = active_instance_id.unwrap_or_else(|| {
            let instance_id = self.allocate_instance_id(profile_id);
            self.roster.insert(
                instance_id.clone(),
                AgentRosterEntry {
                    instance_id: instance_id.clone(),
                    profile_id: profile_id.to_string(),
                    role: role.to_string(),
                    replica_suffix: replica_suffix.to_string(),
                    display_name: display_name.to_string(),
                    status: AgentStatus::Active,
                    last_pane: None,
                    last_seen_at: None,
                },
            );
            instance_id
        });
        let entry = self.roster.get_mut(&instance_id)?;
        entry.status = AgentStatus::Active;
        entry.last_seen_at = Some(now_millis());
        if let Some(pane) = last_pane {
            entry.last_pane = Some(pane);
        }
        Some(entry)
    }

    /// Mark an instance terminated but keep its roster entry as history.
    pub fn roster_terminate(&mut self, display_name: &str) -> bool {
        let instance_id = self.roster.iter().find_map(|(instance_id, entry)| {
            (entry.display_name == display_name && entry.status != AgentStatus::Terminated)
                .then(|| instance_id.clone())
        });
        if let Some(entry) = instance_id.and_then(|instance_id| self.roster.get_mut(&instance_id)) {
            entry.status = AgentStatus::Terminated;
            entry.last_seen_at = Some(now_millis());
            true
        } else {
            false
        }
    }

    /// Mark one durable instance terminated without resolving through a
    /// process-local pane or mutable display name.
    pub fn roster_terminate_instance(&mut self, instance_id: &str) -> bool {
        let Some(entry) = self.roster.get_mut(instance_id) else {
            return false;
        };
        if entry.status == AgentStatus::Terminated {
            return false;
        }
        entry.status = AgentStatus::Terminated;
        entry.last_seen_at = Some(now_millis());
        true
    }

    /// Reserve an archived instance for an exact revival. The stable roster
    /// identity and display name are retained; only its lifecycle state and
    /// stale pane address are reset until spawn completion binds a new pane.
    pub fn roster_reserve_revival(&mut self, instance_id: &str) -> Option<AgentRosterEntry> {
        let entry = self.roster.get_mut(instance_id)?;
        if entry.status != AgentStatus::Terminated {
            return None;
        }
        entry.status = AgentStatus::Active;
        entry.last_pane = None;
        entry.last_seen_at = Some(now_millis());
        Some(entry.clone())
    }

    /// Return an unsuccessful exact revival reservation to the archive.
    pub fn roster_cancel_revival(&mut self, instance_id: &str) -> bool {
        let Some(entry) = self.roster.get_mut(instance_id) else {
            return false;
        };
        if entry.status != AgentStatus::Active || entry.last_pane.is_some() {
            return false;
        }
        entry.status = AgentStatus::Terminated;
        entry.last_seen_at = Some(now_millis());
        true
    }

    /// Drop an unstarted spawn reservation and restore the profile's replica
    /// high-water mark from the remaining durable roster entries. Reservations
    /// that have already bound a pane become historical instances instead.
    pub fn roster_release_reservation(&mut self, instance_id: &str) -> bool {
        let Some(entry) = self.roster.get(instance_id) else {
            return false;
        };
        if entry.status != AgentStatus::Active || entry.last_pane.is_some() {
            return false;
        }
        let profile_id = entry.profile_id.clone();
        self.roster.remove(instance_id);

        let replicas_assigned = self
            .roster
            .values()
            .filter(|entry| entry.profile_id == profile_id && entry.last_pane.is_some())
            .filter_map(|entry| replica_index_from_suffix(&entry.replica_suffix))
            .max()
            .unwrap_or_default();
        if let Some(profile) = self.get_mut(&profile_id) {
            profile.replicas_assigned = replicas_assigned;
        }
        true
    }

    /// Mark a reserved durable instance active and bind its current pane.
    pub fn roster_activate_instance(&mut self, instance_id: &str, pane_id: String) -> bool {
        let Some(entry) = self.roster.get_mut(instance_id) else {
            return false;
        };
        let changed =
            entry.status != AgentStatus::Active || entry.last_pane.as_deref() != Some(&pane_id);
        entry.status = AgentStatus::Active;
        entry.last_pane = Some(pane_id);
        entry.last_seen_at = Some(now_millis());
        changed
    }

    /// Update the lifecycle status for one durable instance.
    pub fn roster_update_status_instance(
        &mut self,
        instance_id: &str,
        status: AgentStatus,
    ) -> bool {
        let Some(entry) = self.roster.get_mut(instance_id) else {
            return false;
        };
        if entry.status == AgentStatus::Terminated || entry.status == status {
            return false;
        }
        entry.status = status;
        entry.last_seen_at = Some(now_millis());
        true
    }

    /// Whether an exact durable instance has a different live lifecycle state.
    pub fn roster_status_differs_instance(&self, instance_id: &str, status: AgentStatus) -> bool {
        self.roster
            .get(instance_id)
            .is_some_and(|entry| entry.status != AgentStatus::Terminated && entry.status != status)
    }

    /// Mark the live instance last seen in `pane_id` terminated. This keeps
    /// roster lifecycle correct when a user renames or clears the live alias.
    pub fn roster_terminate_for_pane(&mut self, pane_id: &str) -> bool {
        let Some(instance_id) = self.roster.iter().find_map(|(instance_id, entry)| {
            (entry.status != AgentStatus::Terminated && entry.last_pane.as_deref() == Some(pane_id))
                .then(|| instance_id.clone())
        }) else {
            return false;
        };
        let Some(entry) = self.roster.get_mut(&instance_id) else {
            return false;
        };
        entry.status = AgentStatus::Terminated;
        entry.last_seen_at = Some(now_millis());
        true
    }

    /// Update the lifecycle state for the live instance occupying `pane_id`.
    /// Returns whether the durable record changed.
    pub fn roster_update_status_for_pane(&mut self, pane_id: &str, status: AgentStatus) -> bool {
        let Some(instance_id) = self.roster.iter().find_map(|(instance_id, entry)| {
            (entry.status != AgentStatus::Terminated && entry.last_pane.as_deref() == Some(pane_id))
                .then(|| instance_id.clone())
        }) else {
            return false;
        };
        let Some(entry) = self.roster.get_mut(&instance_id) else {
            return false;
        };
        if entry.status == status {
            return false;
        }
        entry.status = status;
        entry.last_seen_at = Some(now_millis());
        true
    }

    /// Whether updating the active instance in `pane_id` would change its
    /// persisted lifecycle state.
    pub fn roster_status_differs_for_pane(&self, pane_id: &str, status: AgentStatus) -> bool {
        self.roster.values().any(|entry| {
            entry.status != AgentStatus::Terminated
                && entry.last_pane.as_deref() == Some(pane_id)
                && entry.status != status
        })
    }

    /// Archive records that are not represented by any live terminal after a
    /// cold session restore. Live handoff imports skip this reconciliation
    /// because they transfer the original PTYs and identities intact.
    pub fn roster_terminate_missing_live_instances(
        &mut self,
        live_instance_ids: &[String],
        legacy_live_names: &[String],
    ) -> bool {
        let live_instance_ids = live_instance_ids
            .iter()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>();
        let legacy_live_names = legacy_live_names
            .iter()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>();
        let mut live_entries_by_name = BTreeMap::<String, usize>::new();
        for entry in self
            .roster
            .values()
            .filter(|entry| entry.status != AgentStatus::Terminated)
        {
            *live_entries_by_name
                .entry(entry.display_name.clone())
                .or_default() += 1;
        }

        let mut changed = false;
        for entry in self.roster.values_mut() {
            let legacy_name_is_unique = legacy_live_names.contains(entry.display_name.as_str())
                && live_entries_by_name
                    .get(entry.display_name.as_str())
                    .is_some_and(|count| *count == 1);
            let legacy_name_is_ambiguous =
                legacy_live_names.contains(entry.display_name.as_str()) && !legacy_name_is_unique;
            if entry.status != AgentStatus::Terminated
                && !live_instance_ids.contains(entry.instance_id.as_str())
                && !legacy_name_is_unique
                && !legacy_name_is_ambiguous
            {
                entry.status = AgentStatus::Terminated;
                entry.last_seen_at = Some(now_millis());
                changed = true;
            }
        }
        changed
    }

    /// All currently-alive roster entries for a role (first instance plus any
    /// live replicas). This is how phase 3 addresses "the agent(s) of role R".
    pub fn alive_by_role(&self, role: &str) -> Vec<&AgentRosterEntry> {
        self.roster
            .values()
            .filter(|e| e.role == role && e.status != AgentStatus::Terminated)
            .collect()
    }

    /// Is any live instance of this role currently rostered?
    pub fn is_role_alive(&self, role: &str) -> bool {
        self.roster
            .values()
            .any(|e| e.role == role && e.status != AgentStatus::Terminated)
    }

    /// The roster entry for an exact live agent name, if present and alive.
    pub fn alive_instance(&self, display_name: &str) -> Option<&AgentRosterEntry> {
        self.roster.values().find(|entry| {
            entry.display_name == display_name && entry.status != AgentStatus::Terminated
        })
    }

    fn allocate_instance_id(&mut self, profile_id: &str) -> String {
        loop {
            self.next_roster_instance = self.next_roster_instance.saturating_add(1);
            let instance_id = format!("{profile_id}:{}", self.next_roster_instance);
            if !self.roster.contains_key(&instance_id) {
                return instance_id;
            }
        }
    }
}

#[allow(dead_code)]
/// Phase 2/3 API: consumed once replica-aware spawn is wired in.
/// Build a `role-replica-N` name (N is 1-based), preserving a stable role
/// disambiguator when the role would otherwise be truncated.
pub fn format_replica_name(role: &str, index: u32) -> String {
    let candidate = format!("{role}-replica-{index}");
    if candidate.len() <= MAX_NAME_LEN {
        return candidate;
    }

    let mut hash = 2_166_136_261_u32;
    for byte in role.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    let tail = format!("-{hash:08x}-replica-{index}");
    let budget = MAX_NAME_LEN.saturating_sub(tail.len());
    let trimmed: String = role.chars().take(budget).collect();
    let trimmed = if trimmed.is_empty() {
        "agent"
    } else {
        trimmed.as_str()
    };
    format!("{trimmed}{tail}")
}

// ---- IO ---------------------------------------------------------------

fn registry_path() -> PathBuf {
    crate::session::data_dir().join(REGISTRY_FILENAME)
}

fn legacy_registry_path() -> PathBuf {
    crate::config::config_dir().join(REGISTRY_FILENAME)
}

fn owned_instructions_path_at(registry: &Path, role: &str) -> PathBuf {
    registry
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(PROFILE_CONTEXT_DIRECTORY)
        .join(role)
        .join(PROFILE_INSTRUCTIONS_FILENAME)
}

/// Return the durable path for a profile-owned `AGENTS.md` file.
pub fn owned_instructions_path(role: &str) -> PathBuf {
    let registry = registry_path();
    let target = resolve_write_target(&registry).unwrap_or(registry);
    owned_instructions_path_at(&target, role)
}

/// Read a profile-owned `AGENTS.md` without touching user-supplied Markdown
/// attachments.
pub fn read_owned_instructions(role: &str) -> std::io::Result<String> {
    std::fs::read_to_string(owned_instructions_path(role))
}

/// Materialize a profile-owned instruction file without changing registry
/// state. Callers that persist a profile should add this path as `AGENTS.md`.
pub fn write_owned_instructions(path: &Path, instructions: &str) -> std::io::Result<()> {
    if instructions.contains('\0') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "agent instructions must not contain NUL bytes",
        ));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(instructions.as_bytes())?;
    file.sync_all()
}

/// Atomically replace a profile-owned `AGENTS.md`. The target must already
/// exist; this prevents an edit from creating an unregistered context file.
pub fn replace_owned_instructions(role: &str, instructions: &str) -> std::io::Result<()> {
    let target = owned_instructions_path(role);
    replace_owned_instructions_at_path(&target, instructions)
}

fn replace_owned_instructions_at_path(target: &Path, instructions: &str) -> std::io::Result<()> {
    if instructions.contains('\0') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "agent instructions must not contain NUL bytes",
        ));
    }
    if !target.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("profile instructions {} do not exist", target.display()),
        ));
    }
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let temp_id = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(PROFILE_INSTRUCTIONS_FILENAME),
        std::process::id(),
        temp_id,
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    if let Err(err) = file
        .write_all(instructions.as_bytes())
        .and_then(|_| file.sync_all())
    {
        let _ = std::fs::remove_file(&temporary);
        return Err(err);
    }
    drop(file);
    if let Err(err) = crate::platform::replace_file(&temporary, target) {
        let _ = std::fs::remove_file(&temporary);
        return Err(err);
    }
    if let Err(err) = sync_parent_directory(target) {
        warn!(
            path = %target.display(),
            err = %err,
            "profile instruction replacement committed but parent directory sync failed"
        );
    }
    Ok(())
}

fn with_registry_lock<T>(
    operation: impl FnOnce(&Path) -> std::io::Result<T>,
) -> std::io::Result<T> {
    let target = resolve_write_target(&registry_path())?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = target.with_file_name(REGISTRY_LOCK_FILENAME);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock.lock()?;
    operation(&target)
}

/// Follow a (possibly dangling) symlink so the write lands on the target,
/// mirroring `persist::io::resolve_write_target`.
fn resolve_write_target(path: &Path) -> std::io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..16 {
        let meta = match std::fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(_) => return Ok(current),
        };
        if !meta.file_type().is_symlink() {
            return Ok(current);
        }
        let link = std::fs::read_link(&current)?;
        current = if link.is_absolute() {
            link
        } else {
            current
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(link)
        };
    }
    Ok(current)
}

/// Apply one registry mutation while holding the session-scoped registry lock.
///
/// The registry is reloaded inside the critical section, so independent server
/// processes sharing a session directory cannot overwrite each other's changes
/// with stale in-memory snapshots. The committed registry is returned for the
/// caller to replace its cache after the atomic write succeeds.
pub fn update<T>(
    mutation: impl FnOnce(&mut AgentRegistry) -> T,
) -> std::io::Result<(T, AgentRegistry)> {
    with_registry_lock(|target| {
        let mut registry = load_for_update(target)?;
        let result = mutation(&mut registry);
        save_to_path(target, &registry)?;
        Ok((result, registry))
    })
}

/// Create a new profile and its session-owned instruction file as one locked
/// registry operation. The instructions file is intentionally next to the
/// registry rather than in the profile's cwd, so switching the harness or cwd
/// cannot replace the profile's context.
pub fn create_with_owned_instructions(
    mut profile: AgentProfile,
    instructions: &str,
) -> std::io::Result<(AgentProfile, AgentRegistry)> {
    with_registry_lock(|target| {
        create_with_owned_instructions_at_path(target, &mut profile, instructions)
    })
}

/// Remove the session-owned instruction directory for a deleted profile.
/// User-supplied Markdown files referenced by the profile are never touched.
pub fn remove_owned_instructions(role: &str) -> std::io::Result<()> {
    let directory = registry_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(PROFILE_CONTEXT_DIRECTORY)
        .join(role);
    match std::fs::remove_dir_all(directory) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Add a session-owned `AGENTS.md` to profiles that predate owned
/// instructions. Existing named Markdown remains untouched. This is the
/// one-time shape migration for registries created before profile creation
/// owned the instruction file.
pub fn ensure_owned_instructions() -> std::io::Result<AgentRegistry> {
    with_registry_lock(ensure_owned_instructions_at_path)
}

fn ensure_owned_instructions_at_path(target: &Path) -> std::io::Result<AgentRegistry> {
    let mut registry = load_for_update(target)?;
    let mut created = Vec::new();
    let mut changed = false;

    for profile in registry.profiles.values_mut() {
        if profile
            .mds
            .iter()
            .any(|md| md.name == PROFILE_INSTRUCTIONS_FILENAME)
        {
            continue;
        }

        let path = owned_instructions_path_at(target, &profile.role);
        match write_owned_instructions(&path, &format!("# {} agent\n", profile.role)) {
            Ok(()) => created.push(path.clone()),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => {
                for created_path in created {
                    let _ = std::fs::remove_file(created_path);
                }
                return Err(err);
            }
        }
        profile.set_md(PROFILE_INSTRUCTIONS_FILENAME, path);
        changed = true;
    }

    if changed {
        if let Err(err) = save_to_path(target, &registry) {
            for created_path in created {
                let _ = std::fs::remove_file(created_path);
            }
            return Err(err);
        }
    }
    Ok(registry)
}

fn create_with_owned_instructions_at_path(
    target: &Path,
    profile: &mut AgentProfile,
    instructions: &str,
) -> std::io::Result<(AgentProfile, AgentRegistry)> {
    let mut registry = load_for_update(target)?;
    if registry.get(&profile.role).is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("agent profile {} already exists", profile.role),
        ));
    }

    let context_path = owned_instructions_path_at(target, &profile.role);
    write_owned_instructions(&context_path, instructions)?;

    profile.set_md(PROFILE_INSTRUCTIONS_FILENAME, context_path.clone());
    registry
        .profiles
        .insert(profile.role.clone(), profile.clone());
    if let Err(err) = save_to_path(target, &registry) {
        let _ = std::fs::remove_file(&context_path);
        return Err(err);
    }
    Ok((profile.clone(), registry))
}

#[cfg(test)]
fn save(registry: &AgentRegistry) -> std::io::Result<()> {
    if registry.version > REGISTRY_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "agent registry version {} is newer than supported version {REGISTRY_VERSION}",
                registry.version
            ),
        ));
    }
    save_to_path(&registry_path(), registry)
}

fn save_to_path(path: &Path, registry: &AgentRegistry) -> std::io::Result<()> {
    save_to_path_with_replace(path, registry, crate::platform::replace_file)
}

fn save_to_path_with_replace(
    path: &Path,
    registry: &AgentRegistry,
    replace_file: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let target = resolve_write_target(path)?;
    validate_existing_registry(&target)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(registry)?;
    let temp_id = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let tmp_path = target.with_extension(format!("json.{}.{}.tmp", std::process::id(), temp_id));
    let mut tmp = std::fs::File::create(&tmp_path)?;
    tmp.write_all(json.as_bytes())?;
    tmp.sync_all()?;
    drop(tmp);
    if let Err(err) = replace_file(&tmp_path, &target) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    if let Err(err) = sync_parent_directory(&target) {
        warn!(
            path = %target.display(),
            err = %err,
            "agent registry rename committed but parent directory sync failed"
        );
    }
    Ok(())
}

/// Do not replace an existing registry that this build cannot safely read.
/// Startup remains best-effort so a damaged registry cannot block a server,
/// but a later profile mutation must preserve the original bytes for recovery.
fn validate_existing_registry(path: &Path) -> std::io::Result<()> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    parse_strict(&content).map(|_| ())
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[allow(dead_code)]
/// Phase 2 API: remove the registry file (e.g. on explicit profile deletion or
/// logout). Missing file is not an error.
pub fn clear() {
    let path = registry_path();
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => warn!(path = %path.display(), err = %err, "failed to clear agent registry"),
    }
}

/// Load the registry from disk. A missing, empty, or malformed file yields an
/// empty registry so startup stays available. Mutations use a strict pre-write
/// read and therefore preserve malformed or newer-version files for recovery.
pub fn load() -> AgentRegistry {
    let path = registry_path();
    if path.exists() || crate::session::active_name().is_none() {
        return load_from_path(&path);
    }

    let legacy_path = legacy_registry_path();
    if legacy_path.exists() {
        warn!(
            path = %legacy_path.display(),
            target = %path.display(),
            "loading legacy shared agent registry into named session; it will be isolated on the next write"
        );
        load_from_path(&legacy_path)
    } else {
        AgentRegistry::new()
    }
}

/// Strictly reload the current session registry for a shared-session read.
///
/// Unlike [`load`], this propagates malformed, inaccessible, and newer-version
/// errors so an API read never silently replaces a server's cache with an empty
/// registry after another server has updated the shared file.
pub fn load_for_read() -> std::io::Result<AgentRegistry> {
    let path = registry_path();
    if path.exists() || crate::session::active_name().is_none() {
        return load_for_update(&path);
    }

    let legacy_path = legacy_registry_path();
    if legacy_path.exists() {
        load_for_update(&legacy_path)
    } else {
        Ok(AgentRegistry::new())
    }
}

fn load_from_path(path: &Path) -> AgentRegistry {
    let content = match std::fs::read_to_string(path) {
        Ok(content) if !content.trim().is_empty() => content,
        Ok(_) => return AgentRegistry::new(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return AgentRegistry::new(),
        Err(err) => {
            warn!(path = %path.display(), err = %err, "failed to read agent registry");
            return AgentRegistry::new();
        }
    };
    parse(&content)
}

fn load_for_update(path: &Path) -> std::io::Result<AgentRegistry> {
    match std::fs::read_to_string(path) {
        Ok(content) => parse_compatible_registry(&content),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let legacy_path = legacy_registry_path();
            if crate::session::active_name().is_some() && legacy_path.exists() {
                let content = std::fs::read_to_string(legacy_path)?;
                parse_compatible_registry(&content)
            } else {
                Ok(AgentRegistry::new())
            }
        }
        Err(err) => Err(err),
    }
}

fn parse_compatible_registry(content: &str) -> std::io::Result<AgentRegistry> {
    let mut registry = parse_strict(content)?;
    if registry.version < REGISTRY_VERSION {
        registry.version = REGISTRY_VERSION;
    }
    normalize_roster_entries(&mut registry);
    Ok(registry)
}

#[allow(dead_code)]
/// Phase 2/3 API: the save path re-versions an older on-disk file through this.
/// Parse a registry from raw JSON. Exposed for tests and for the save path to
/// detect-and-migrate an older on-disk version.
pub fn parse(content: &str) -> AgentRegistry {
    match parse_strict(content) {
        Ok(mut registry) => {
            if registry.version < REGISTRY_VERSION {
                registry.version = REGISTRY_VERSION;
            }
            normalize_roster_entries(&mut registry);
            registry
        }
        Err(err) if err.kind() == std::io::ErrorKind::Unsupported => {
            let version = serde_json::from_str::<RegistryVersion>(content)
                .map(|version| version.version)
                .unwrap_or(REGISTRY_VERSION + 1);
            warn!(
                version,
                supported = REGISTRY_VERSION,
                "agent registry version is newer than this build; preserving without loading"
            );
            AgentRegistry {
                version,
                ..AgentRegistry::default()
            }
        }
        Err(err) => {
            warn!(err = %err, "failed to parse agent registry, ignoring");
            AgentRegistry::new()
        }
    }
}

fn parse_strict(content: &str) -> std::io::Result<AgentRegistry> {
    let version = serde_json::from_str::<RegistryVersion>(content)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    if version.version > REGISTRY_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "agent registry version {} is newer than supported version {REGISTRY_VERSION}",
                version.version
            ),
        ));
    }
    serde_json::from_str(content)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}

fn normalize_roster_entries(registry: &mut AgentRegistry) {
    let roster = std::mem::take(&mut registry.roster);
    for (legacy_key, mut entry) in roster {
        let mut instance_id = if entry.instance_id.is_empty() {
            legacy_key
        } else {
            entry.instance_id.clone()
        };
        while registry.roster.contains_key(&instance_id) {
            registry.next_roster_instance = registry.next_roster_instance.saturating_add(1);
            instance_id = format!("{}:{}", entry.profile_id, registry.next_roster_instance);
        }
        entry.instance_id = instance_id.clone();
        registry.roster.insert(instance_id, entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(role: &str) -> AgentProfile {
        AgentProfile::new(role, PathBuf::from("/tmp"), Path::new("/tmp"))
    }

    #[test]
    fn empty_registry_defaults() {
        let reg = AgentRegistry::new();
        assert!(reg.profiles.is_empty());
        assert!(reg.roster.is_empty());
        assert!(!reg.is_role_alive("reviewer"));
    }

    #[test]
    fn register_or_get_is_idempotent_and_keeps_defaults() {
        let mut reg = AgentRegistry::new();
        let p = reg.register_or_get("reviewer", PathBuf::from("/repo"));
        assert_eq!(p.role, "reviewer");
        assert_eq!(p.harness, "codex");
        let created_cwd = p.native_cwd.clone();

        // Re-registering the same role returns the existing profile untouched.
        let again = reg.register_or_get("reviewer", PathBuf::from("/other"));
        assert_eq!(again.role, "reviewer");
        assert_eq!(again.native_cwd, created_cwd);
        assert_eq!(reg.profiles.len(), 1);
    }

    #[test]
    fn replica_names_are_sequential_and_persisted() {
        let mut reg = AgentRegistry::new();
        let p = reg.register_or_get("planner", PathBuf::from("/tmp"));
        let a = p.next_replica_name().unwrap();
        let b = p.next_replica_name().unwrap();
        assert_eq!(a, "planner-replica-1");
        assert_eq!(b, "planner-replica-2");
        // The index lives on the profile, so a fresh registry loaded from the
        // same JSON continues numbering instead of restarting at 1.
        let json = serde_json::to_string(&reg).unwrap();
        let mut reloaded = parse(&json);
        let p2 = reloaded.get_mut("planner").unwrap();
        assert_eq!(p2.next_replica_name().as_deref(), Some("planner-replica-3"));
    }

    #[test]
    fn replica_assignment_remains_monotonic_when_completions_arrive_out_of_order() {
        let mut profile = profile("reviewer");

        profile.record_replica_assignment(2);
        profile.record_replica_assignment(1);

        assert_eq!(profile.replicas_assigned, 2);
        assert_eq!(
            profile.next_replica_name().as_deref(),
            Some("reviewer-replica-3")
        );
    }

    #[test]
    fn next_available_replica_index_reuses_terminated_names() {
        let mut registry = AgentRegistry::new();
        registry.register_or_get("reviewer", PathBuf::from("/tmp"));
        registry.roster_register(
            "reviewer",
            "reviewer",
            "reviewer",
            "",
            Some("w1:t1:p1".into()),
        );
        registry.roster_register(
            "reviewer-replica-1",
            "reviewer",
            "reviewer",
            "-replica-1",
            Some("w1:t1:p2".into()),
        );
        registry.roster_register(
            "reviewer-replica-2",
            "reviewer",
            "reviewer",
            "-replica-2",
            Some("w1:t1:p3".into()),
        );
        assert!(registry.roster_terminate("reviewer-replica-1"));

        assert_eq!(
            registry.next_available_replica_index("reviewer", &[]),
            Some(1)
        );
        assert_eq!(
            registry.next_available_replica_index("reviewer", &["reviewer-replica-1".into()]),
            Some(3)
        );
    }

    #[test]
    fn first_spawn_seeds_native_cwd_once() {
        let mut profile =
            AgentProfile::new("reviewer", PathBuf::from("/placeholder"), Path::new("/"));
        profile.native_cwd_seeded = false;

        profile.record_spawn(PathBuf::from("/first-tab"));
        profile.record_spawn(PathBuf::from("/later-tab"));

        assert_eq!(profile.native_cwd, PathBuf::from("/first-tab"));
        assert!(profile.native_cwd_seeded);
        assert!(profile.last_spawned_at.is_some());
    }

    #[test]
    fn replica_name_clamps_to_agent_name_limit() {
        // "a-replica-" is 10 chars; cap at 32 leaves 22 for the role.
        let long_role = "a".repeat(40);
        let name = format_replica_name(&long_role, 1);
        assert!(name.len() <= MAX_NAME_LEN);
        assert!(name.ends_with("-replica-1"));
    }

    #[test]
    fn long_roles_keep_distinct_replica_names() {
        let first = format_replica_name("reviewer-abcdefghijklmnop", 1);
        let second = format_replica_name("reviewer-abcdefghijklnoq", 1);
        assert_ne!(first, second);
        assert!(first.len() <= MAX_NAME_LEN);
        assert!(second.len() <= MAX_NAME_LEN);
    }

    #[test]
    fn md_add_replace_remove_are_named() {
        let mut p = profile("r");
        p.set_md("context.md", PathBuf::from("/a/context.md"));
        p.set_md("todo.md", PathBuf::from("/a/todo.md"));
        assert_eq!(p.mds.len(), 2);
        // Re-adding the same name replaces rather than duplicates.
        p.set_md("context.md", PathBuf::from("/b/context.md"));
        assert_eq!(p.mds.len(), 2);
        assert_eq!(p.mds[0].path, PathBuf::from("/b/context.md"));
        assert!(p.remove_md("context.md"));
        assert_eq!(p.mds.len(), 1);
        assert!(!p.remove_md("context.md"));
    }

    #[test]
    fn owned_instructions_are_created_with_the_profile_and_persisted() {
        let root = std::env::temp_dir().join(format!(
            "herdr-agent-profile-context-{}-{}",
            std::process::id(),
            now_millis()
        ));
        let registry_path = root.join(REGISTRY_FILENAME);
        let mut profile = profile("reviewer");
        profile.harness = "claude".into();

        let (created, registry) = create_with_owned_instructions_at_path(
            &registry_path,
            &mut profile,
            "# Reviewer\n\nReview the current change.\n",
        )
        .unwrap();

        let instructions = root
            .join(PROFILE_CONTEXT_DIRECTORY)
            .join("reviewer")
            .join(PROFILE_INSTRUCTIONS_FILENAME);
        assert_eq!(
            created.mds,
            vec![AgentMd {
                name: PROFILE_INSTRUCTIONS_FILENAME.into(),
                path: instructions.clone(),
            }]
        );
        assert_eq!(
            std::fs::read_to_string(&instructions).unwrap(),
            "# Reviewer\n\nReview the current change.\n"
        );
        assert_eq!(registry.get("reviewer"), Some(&created));
        assert_eq!(
            load_from_path(&registry_path).get("reviewer"),
            Some(&created)
        );

        let err = create_with_owned_instructions_at_path(&registry_path, &mut profile, "other")
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(&instructions).unwrap(),
            "# Reviewer\n\nReview the current change.\n"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn owned_instructions_are_replaced_atomically_with_multiline_content() {
        let root = std::env::temp_dir().join(format!(
            "herdr-agent-profile-instructions-replace-{}-{}",
            std::process::id(),
            now_millis()
        ));
        let path = root.join(PROFILE_INSTRUCTIONS_FILENAME);
        std::fs::create_dir_all(&root).unwrap();
        write_owned_instructions(&path, "before").unwrap();

        replace_owned_instructions_at_path(&path, "first line\nsecond line\n").unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "first line\nsecond line\n"
        );
        assert!(replace_owned_instructions_at_path(&path, "bad\0input").is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "first line\nsecond line\n"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn existing_profiles_gain_owned_instructions_without_replacing_other_markdown() {
        let root = std::env::temp_dir().join(format!(
            "herdr-agent-profile-migration-{}-{}",
            std::process::id(),
            now_millis()
        ));
        let registry_path = root.join(REGISTRY_FILENAME);
        let extra_path = root.join("review.md");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&extra_path, "Review all changes.\n").unwrap();

        let mut registry = AgentRegistry::new();
        let mut profile = profile("reviewer");
        profile.set_md("review.md", extra_path.clone());
        registry.profiles.insert(profile.role.clone(), profile);
        save_to_path(&registry_path, &registry).unwrap();

        let migrated = ensure_owned_instructions_at_path(&registry_path).unwrap();
        let profile = migrated.get("reviewer").unwrap();
        let instructions = root
            .join(PROFILE_CONTEXT_DIRECTORY)
            .join("reviewer")
            .join(PROFILE_INSTRUCTIONS_FILENAME);
        assert_eq!(
            profile.mds,
            vec![
                AgentMd {
                    name: PROFILE_INSTRUCTIONS_FILENAME.into(),
                    path: instructions.clone(),
                },
                AgentMd {
                    name: "review.md".into(),
                    path: extra_path,
                },
            ]
        );
        assert_eq!(
            std::fs::read_to_string(instructions).unwrap(),
            "# reviewer agent\n"
        );
        assert_eq!(load_from_path(&registry_path), migrated);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn roster_tracks_alive_instances_by_role() {
        let mut reg = AgentRegistry::new();
        let base = reg.register_or_get("reviewer", PathBuf::from("/tmp"));
        let primary = base.role.clone();
        let replica = base.next_replica_name().unwrap();

        reg.roster_register(&primary, &primary, &primary, "", Some("w1:t1:p1".into()));
        reg.roster_register(
            &replica,
            &primary,
            &primary,
            "-replica-1",
            Some("w1:t1:p2".into()),
        );

        assert!(reg.is_role_alive("reviewer"));
        let alive = reg.alive_by_role("reviewer");
        assert_eq!(alive.len(), 2);

        // Terminating the primary leaves the replica alive.
        reg.roster_terminate(&primary);
        assert!(reg.alive_instance(&primary).is_none());
        assert_eq!(reg.alive_by_role("reviewer").len(), 1);
        assert_eq!(reg.alive_by_role("reviewer")[0].display_name, replica);
    }

    #[test]
    fn roster_termination_by_pane_survives_live_name_changes() {
        let mut reg = AgentRegistry::new();
        reg.roster_register("reviewer", "reviewer", "reviewer", "", Some("w1:p1".into()));
        assert!(reg.roster_terminate_for_pane("w1:p1"));
        assert!(!reg.roster_terminate_for_pane("w1:p1"));
        assert!(reg.roster.values().any(
            |entry| entry.display_name == "reviewer" && entry.status == AgentStatus::Terminated
        ));
    }

    #[test]
    fn archived_instance_can_be_reserved_and_cancelled_for_exact_revival() {
        let mut registry = AgentRegistry::new();
        registry.register_or_get("reviewer", PathBuf::from("/tmp"));
        let instance_id = registry
            .roster_register(
                "reviewer-replica-1",
                "reviewer",
                "reviewer",
                "-replica-1",
                Some("w1:t1:p2".into()),
            )
            .unwrap()
            .instance_id
            .clone();
        assert!(registry.roster_terminate_instance(&instance_id));

        let reserved = registry.roster_reserve_revival(&instance_id).unwrap();
        assert_eq!(reserved.display_name, "reviewer-replica-1");
        assert_eq!(registry.roster[&instance_id].last_pane, None);
        assert!(registry.roster_cancel_revival(&instance_id));
        assert_eq!(
            registry.roster[&instance_id].status,
            AgentStatus::Terminated
        );
    }

    #[test]
    fn roster_keeps_archived_instances_when_a_display_name_is_reused() {
        let mut reg = AgentRegistry::new();
        reg.register_or_get("reviewer", PathBuf::from("/tmp"));
        reg.roster_register("reviewer", "reviewer", "reviewer", "", Some("w1:p1".into()));
        assert!(reg.roster_terminate("reviewer"));
        reg.roster_register("reviewer", "reviewer", "reviewer", "", Some("w1:p2".into()));

        let reviewer_entries: Vec<_> = reg
            .roster
            .values()
            .filter(|entry| entry.display_name == "reviewer")
            .collect();
        assert_eq!(reviewer_entries.len(), 2);
        assert!(reviewer_entries
            .iter()
            .any(|entry| entry.status == AgentStatus::Terminated));
        assert!(reviewer_entries
            .iter()
            .any(|entry| entry.status == AgentStatus::Active));
        assert_ne!(
            reviewer_entries[0].instance_id,
            reviewer_entries[1].instance_id
        );
    }

    #[test]
    fn roster_status_follows_the_live_pane_and_keeps_archives_terminated() {
        let mut registry = AgentRegistry::new();
        registry.roster_register("reviewer", "reviewer", "reviewer", "", Some("w1:p1".into()));

        assert!(registry.roster_update_status_for_pane("w1:p1", AgentStatus::Working));
        assert!(!registry.roster_update_status_for_pane("w1:p1", AgentStatus::Working));
        assert_eq!(
            registry
                .alive_instance("reviewer")
                .map(|entry| entry.status),
            Some(AgentStatus::Working)
        );

        assert!(registry.roster_terminate("reviewer"));
        assert!(!registry.roster_update_status_for_pane("w1:p1", AgentStatus::Idle));
        assert_eq!(
            registry.roster.values().next().map(|entry| entry.status),
            Some(AgentStatus::Terminated)
        );
    }

    #[test]
    fn roster_status_uses_instance_identity_when_live_panes_collide() {
        let mut registry = AgentRegistry::new();
        let first = registry
            .roster_register("reviewer", "reviewer", "reviewer", "", Some("w1:p1".into()))
            .unwrap()
            .instance_id
            .clone();
        let second = registry
            .roster_register(
                "reviewer-replica-1",
                "reviewer",
                "reviewer",
                "-replica-1",
                Some("w1:p1".into()),
            )
            .unwrap()
            .instance_id
            .clone();

        assert!(registry.roster_update_status_instance(&second, AgentStatus::Working));
        assert_eq!(registry.roster[&first].status, AgentStatus::Active);
        assert_eq!(registry.roster[&second].status, AgentStatus::Working);
    }

    #[test]
    fn cold_restore_archives_roster_entries_without_live_terminal_names() {
        let mut registry = AgentRegistry::new();
        registry.roster_register("reviewer", "reviewer", "reviewer", "", Some("w1:p1".into()));
        registry.roster_register(
            "reviewer-replica-1",
            "reviewer",
            "reviewer",
            "-replica-1",
            Some("w1:p2".into()),
        );

        assert!(registry.roster_terminate_missing_live_instances(&[], &["reviewer".into()]));
        assert_eq!(
            registry
                .alive_instance("reviewer")
                .map(|entry| entry.status),
            Some(AgentStatus::Active)
        );
        assert_eq!(
            registry
                .roster
                .values()
                .find(|entry| entry.display_name == "reviewer-replica-1")
                .map(|entry| entry.status),
            Some(AgentStatus::Terminated)
        );
    }

    #[test]
    fn cold_restore_does_not_guess_between_legacy_duplicate_names() {
        let mut registry = AgentRegistry::new();
        let first = registry
            .roster_register("reviewer", "reviewer", "reviewer", "", Some("w1:p1".into()))
            .unwrap()
            .instance_id
            .clone();
        let mut duplicate = registry.roster[&first].clone();
        duplicate.instance_id = "reviewer-legacy-duplicate".into();
        duplicate.last_pane = Some("w1:p2".into());
        registry
            .roster
            .insert(duplicate.instance_id.clone(), duplicate);

        assert!(!registry.roster_terminate_missing_live_instances(&[], &["reviewer".into()]));
        assert!(registry
            .roster
            .values()
            .all(|entry| entry.status == AgentStatus::Active));
    }

    #[test]
    fn remove_profile_drops_its_roster_entries() {
        let mut reg = AgentRegistry::new();
        let p = reg.register_or_get("reviewer", PathBuf::from("/tmp"));
        let replica = p.next_replica_name().unwrap();
        reg.roster_register("reviewer", "reviewer", "reviewer", "", None);
        reg.roster_register(&replica, "reviewer", "reviewer", "-replica-1", None);
        assert_eq!(reg.roster.len(), 2);

        reg.remove_profile("reviewer");
        assert!(reg.get("reviewer").is_none());
        assert!(reg.roster.is_empty());
    }

    #[test]
    fn releasing_a_reservation_keeps_later_pending_replicas_reserved() {
        let mut registry = AgentRegistry::new();
        registry.register_or_get("reviewer", PathBuf::from("/tmp"));
        registry.roster_register("reviewer", "reviewer", "reviewer", "", Some("w1:p1".into()));
        let first = registry
            .roster_register(
                "reviewer-replica-1",
                "reviewer",
                "reviewer",
                "-replica-1",
                None,
            )
            .unwrap()
            .instance_id
            .clone();
        let second = registry
            .roster_register(
                "reviewer-replica-2",
                "reviewer",
                "reviewer",
                "-replica-2",
                None,
            )
            .unwrap()
            .instance_id
            .clone();
        registry
            .get_mut("reviewer")
            .unwrap()
            .record_replica_assignment(2);

        assert!(registry.roster_release_reservation(&first));
        assert_eq!(registry.get("reviewer").unwrap().replicas_assigned, 0);
        assert!(registry.roster.contains_key(&second));

        assert!(registry.roster_release_reservation(&second));
        assert_eq!(registry.get("reviewer").unwrap().replicas_assigned, 0);
        assert_eq!(
            registry
                .get_mut("reviewer")
                .unwrap()
                .next_replica_name()
                .as_deref(),
            Some("reviewer-replica-1")
        );
    }

    #[test]
    fn round_trip_preserves_everything() {
        let mut reg = AgentRegistry::new();
        reg.version = REGISTRY_VERSION;
        let p = reg.register_or_get("architect", PathBuf::from("/proj"));
        p.harness = "claude".into();
        p.model = Some("sonnet".into());
        p.effort = Some(EffortLevel::High);
        p.apikey_ref = Some("kw:architect".into());
        p.set_md("plan.md", PathBuf::from("/proj/plan.md"));
        reg.roster_register("architect", "architect", "architect", "", None);

        let json = serde_json::to_string_pretty(&reg).unwrap();
        let restored = parse(&json);
        assert_eq!(restored, reg);
        assert_eq!(restored.get("architect").unwrap().mds.len(), 1);
    }

    #[test]
    fn saved_registry_restores_profiles_markdown_replicas_and_roster() {
        let path = std::env::temp_dir().join(format!(
            "herdr-agent-registry-{}-{}.json",
            std::process::id(),
            now_millis()
        ));
        let mut registry = AgentRegistry::new();
        let profile = registry.register_or_get("reviewer", PathBuf::from("/repo"));
        profile.harness = "claude".into();
        profile.model = Some("sonnet".into());
        profile.effort = Some(EffortLevel::High);
        profile.apikey_ref = Some("keychain:reviewer".into());
        profile.set_md("context.md", PathBuf::from("/repo/context.md"));
        profile.replicas_assigned = 2;
        registry.roster_register(
            "reviewer-replica-2",
            "reviewer",
            "reviewer",
            "-replica-2",
            Some("w1:t1:p2".into()),
        );
        registry.roster_terminate("reviewer-replica-2");

        save_to_path(&path, &registry).unwrap();
        let restored = load_from_path(&path);

        assert_eq!(restored, registry);
        let profile = restored.get("reviewer").unwrap();
        assert_eq!(profile.mds[0].path, PathBuf::from("/repo/context.md"));
        assert_eq!(profile.replicas_assigned, 2);
        assert!(restored.roster.values().any(|entry| {
            entry.display_name == "reviewer-replica-2" && entry.status == AgentStatus::Terminated
        }));

        registry.get_mut("reviewer").unwrap().model = None;
        save_to_path(&path, &registry).unwrap();
        assert_eq!(load_from_path(&path), registry);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn failed_registry_replacement_preserves_existing_file() {
        let path = std::env::temp_dir().join(format!(
            "herdr-agent-registry-replace-failure-{}-{}.json",
            std::process::id(),
            now_millis()
        ));
        let registry = AgentRegistry::new();
        save_to_path(&path, &registry).unwrap();
        let existing = std::fs::read_to_string(&path).unwrap();
        let mut updated = AgentRegistry::new();
        updated.register_or_get("reviewer", PathBuf::from("/repo"));
        let mut temporary_path = None;

        let err = save_to_path_with_replace(&path, &updated, |replacement, _| {
            temporary_path = Some(replacement.to_path_buf());
            Err(std::io::Error::other("forced replacement failure"))
        })
        .expect_err("replacement failure should propagate");

        assert_eq!(err.kind(), std::io::ErrorKind::Other);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), existing);
        assert!(!temporary_path.expect("replacement path").exists());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn malformed_existing_registry_is_not_overwritten() {
        let path = std::env::temp_dir().join(format!(
            "herdr-agent-registry-malformed-{}-{}.json",
            std::process::id(),
            now_millis()
        ));
        let malformed = "not json at all {{{";
        std::fs::write(&path, malformed).unwrap();

        let err = save_to_path(&path, &AgentRegistry::new()).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), malformed);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn future_existing_registry_is_not_overwritten() {
        let path = std::env::temp_dir().join(format!(
            "herdr-agent-registry-future-{}-{}.json",
            std::process::id(),
            now_millis()
        ));
        let future = format!(
            r#"{{"version":{},"profiles":{{}},"roster":{{}}}}"#,
            REGISTRY_VERSION + 1
        );
        std::fs::write(&path, &future).unwrap();

        let err = save_to_path(&path, &AgentRegistry::new()).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), future);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn newer_registry_version_is_preserved_without_loading() {
        let registry = parse(r#"{"version":3,"profiles":{"future":{}}}"#);
        assert_eq!(registry.version, 3);
        assert!(registry.profiles.is_empty());
        assert!(registry.roster.is_empty());
        assert!(save(&registry).is_err());
    }

    #[test]
    fn named_sessions_use_distinct_registry_paths() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let config_home = std::env::temp_dir().join(format!(
            "herdr-agent-registry-path-{}-{}",
            std::process::id(),
            now_millis()
        ));
        std::env::set_var("XDG_CONFIG_HOME", &config_home);

        std::env::set_var(crate::session::SESSION_ENV_VAR, "one");
        let first = registry_path();
        std::env::set_var(crate::session::SESSION_ENV_VAR, "two");
        let second = registry_path();

        assert_ne!(first, second);
        assert!(first.ends_with("sessions/one/agents.json"));
        assert!(second.ends_with("sessions/two/agents.json"));

        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(config_home);
    }

    #[test]
    fn new_registry_uses_the_current_version() {
        assert_eq!(AgentRegistry::new().version, REGISTRY_VERSION);
    }

    #[test]
    fn replica_counter_overflow_is_rejected() {
        let mut profile = profile("reviewer");
        profile.replicas_assigned = u32::MAX;
        assert!(profile.next_replica_name().is_none());
    }

    #[test]
    fn older_version_file_loads_and_is_reversioned() {
        let json = r#"{"version":0,"profiles":{},"roster":{}}"#;
        let reg = parse(json);
        assert_eq!(reg.version, REGISTRY_VERSION);
    }

    #[test]
    fn garbage_file_falls_back_to_empty() {
        let reg = parse("not json at all {{{");
        assert!(reg.profiles.is_empty());
    }
}
