use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

const LEASES_FILENAME: &str = "plugin-command-leases.json";
const LEASES_LOCK_FILENAME: &str = ".plugin-command-leases.lock";
const LEASES_VERSION: u32 = 1;
static NEXT_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PluginCommandLease {
    plugin_root: PathBuf,
    #[serde(default)]
    process_id: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PluginCommandLeaseRegistry {
    #[serde(default = "current_version")]
    version: u32,
    #[serde(default = "first_lease_id")]
    next_lease_id: u64,
    #[serde(default)]
    leases: BTreeMap<String, PluginCommandLease>,
}

impl Default for PluginCommandLeaseRegistry {
    fn default() -> Self {
        Self {
            version: LEASES_VERSION,
            next_lease_id: first_lease_id(),
            leases: BTreeMap::new(),
        }
    }
}

fn current_version() -> u32 {
    LEASES_VERSION
}

fn first_lease_id() -> u64 {
    1
}

fn registry_path() -> PathBuf {
    crate::session::data_dir().join(LEASES_FILENAME)
}

fn with_registry_lock<T>(
    path: &Path,
    operation: impl FnOnce(&Path) -> std::io::Result<T>,
) -> std::io::Result<T> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_file_name(LEASES_LOCK_FILENAME);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock.lock()?;
    operation(path)
}

fn load_from_path(path: &Path) -> std::io::Result<PluginCommandLeaseRegistry> {
    if !path.exists() {
        return Ok(PluginCommandLeaseRegistry::default());
    }
    let registry: PluginCommandLeaseRegistry =
        serde_json::from_str(&std::fs::read_to_string(path)?)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    if registry.version > LEASES_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "plugin command lease registry version {} is newer than supported version {LEASES_VERSION}",
                registry.version
            ),
        ));
    }
    Ok(registry)
}

fn save_to_path(path: &Path, registry: &PluginCommandLeaseRegistry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let unique = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let temp_path = path.with_file_name(format!(
        ".{LEASES_FILENAME}.{}.{unique}.tmp",
        std::process::id()
    ));
    let json = serde_json::to_vec_pretty(registry)?;
    if let Err(err) = std::fs::write(&temp_path, json) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }
    if let Err(err) = crate::platform::replace_file(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }
    Ok(())
}

fn update_at_path<T>(
    path: &Path,
    mutation: impl FnOnce(&mut PluginCommandLeaseRegistry) -> std::io::Result<T>,
) -> std::io::Result<T> {
    with_registry_lock(path, |path| {
        let mut registry = load_from_path(path)?;
        let result = mutation(&mut registry)?;
        save_to_path(path, &registry)?;
        Ok(result)
    })
}

fn prune_dead_leases(
    registry: &mut PluginCommandLeaseRegistry,
    mut process_exists: impl FnMut(u32) -> bool,
) -> bool {
    let before = registry.leases.len();
    registry
        .leases
        .retain(|_, lease| lease.process_id.map(&mut process_exists).unwrap_or(true));
    registry.leases.len() != before
}

fn acquire_at_path(path: &Path, plugin_root: PathBuf, limit: usize) -> std::io::Result<String> {
    update_at_path(path, |registry| {
        prune_dead_leases(registry, crate::platform::process_exists);
        if registry.leases.len() >= limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "maximum concurrent plugin commands reached",
            ));
        }
        let id = registry.next_lease_id;
        registry.next_lease_id = registry.next_lease_id.checked_add(1).ok_or_else(|| {
            std::io::Error::other("plugin command lease identifier space exhausted")
        })?;
        let lease_id = format!("plugin-command-{id}");
        registry.leases.insert(
            lease_id.clone(),
            PluginCommandLease {
                plugin_root,
                process_id: None,
            },
        );
        Ok(lease_id)
    })
}

pub fn acquire(plugin_root: PathBuf, limit: usize) -> std::io::Result<String> {
    acquire_at_path(
        &registry_path(),
        crate::worktree::canonical_or_original(&plugin_root),
        limit,
    )
}

fn release_at_path(path: &Path, lease_id: &str) -> std::io::Result<bool> {
    update_at_path(path, |registry| {
        Ok(registry.leases.remove(lease_id).is_some())
    })
}

pub fn release(lease_id: &str) -> std::io::Result<bool> {
    release_at_path(&registry_path(), lease_id)
}

fn track_runner_process_at_path(
    path: &Path,
    lease_id: &str,
    process_id: u32,
) -> std::io::Result<()> {
    update_at_path(path, |registry| {
        let lease = registry.leases.get_mut(lease_id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "plugin command lease no longer exists",
            )
        })?;
        lease.process_id.get_or_insert(process_id);
        Ok(())
    })
}

pub fn track_runner_process(lease_id: &str, process_id: u32) -> std::io::Result<()> {
    track_runner_process_at_path(&registry_path(), lease_id, process_id)
}

fn track_command_process_at_path(
    path: &Path,
    lease_id: &str,
    process_id: u32,
) -> std::io::Result<()> {
    update_at_path(path, |registry| {
        let lease = registry.leases.get_mut(lease_id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "plugin command lease no longer exists",
            )
        })?;
        lease.process_id = Some(process_id);
        Ok(())
    })
}

pub fn track_command_process(lease_id: &str, process_id: u32) -> std::io::Result<()> {
    track_command_process_at_path(&registry_path(), lease_id, process_id)
}

fn active_root_within_at_path(
    path: &Path,
    checkout_path: &Path,
) -> std::io::Result<Option<PathBuf>> {
    let checkout_path = crate::worktree::canonical_or_original(checkout_path);
    with_registry_lock(path, |path| {
        let mut registry = load_from_path(path)?;
        if prune_dead_leases(&mut registry, crate::platform::process_exists) {
            save_to_path(path, &registry)?;
        }
        Ok(registry.leases.values().find_map(|lease| {
            lease
                .plugin_root
                .starts_with(&checkout_path)
                .then(|| lease.plugin_root.clone())
        }))
    })
}

pub fn active_root_within(checkout_path: &Path) -> std::io::Result<Option<PathBuf>> {
    active_root_within_at_path(&registry_path(), checkout_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_registry_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "herdr-plugin-command-leases-{name}-{}-{nanos}.json",
            std::process::id()
        ))
    }

    #[test]
    fn durable_leases_enforce_the_shared_limit_and_reuse_monotonic_ids() {
        let path = temp_registry_path("limit");
        let first = acquire_at_path(&path, PathBuf::from("/tmp/one"), 2).unwrap();
        let second = acquire_at_path(&path, PathBuf::from("/tmp/two"), 2).unwrap();
        let err = acquire_at_path(&path, PathBuf::from("/tmp/three"), 2).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(first, "plugin-command-1");
        assert_eq!(second, "plugin-command-2");
        assert!(release_at_path(&path, &first).unwrap());
        assert_eq!(
            acquire_at_path(&path, PathBuf::from("/tmp/three"), 2).unwrap(),
            "plugin-command-3"
        );

        let _ = std::fs::remove_file(path.with_file_name(LEASES_LOCK_FILENAME));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn durable_leases_find_only_plugin_roots_inside_the_checkout() {
        let path = temp_registry_path("root");
        let checkout = std::env::temp_dir().join(format!(
            "herdr-plugin-command-lease-checkout-{}",
            std::process::id()
        ));
        let plugin_root = checkout.join("plugins/example");
        std::fs::create_dir_all(&plugin_root).unwrap();
        let lease = acquire_at_path(
            &path,
            crate::worktree::canonical_or_original(&plugin_root),
            32,
        )
        .unwrap();

        assert_eq!(
            active_root_within_at_path(&path, &checkout).unwrap(),
            Some(crate::worktree::canonical_or_original(&plugin_root))
        );
        assert_eq!(
            active_root_within_at_path(&path, &checkout.with_extension("other")).unwrap(),
            None
        );
        assert!(release_at_path(&path, &lease).unwrap());

        let _ = std::fs::remove_file(path.with_file_name(LEASES_LOCK_FILENAME));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(checkout);
    }

    #[test]
    fn runner_tracking_never_overwrites_the_command_process() {
        let path = temp_registry_path("process-tracking");
        let lease = acquire_at_path(&path, PathBuf::from("/tmp/plugin"), 32).unwrap();

        track_runner_process_at_path(&path, &lease, 10).unwrap();
        track_command_process_at_path(&path, &lease, 20).unwrap();
        track_runner_process_at_path(&path, &lease, 30).unwrap();

        let registry = load_from_path(&path).unwrap();
        assert_eq!(registry.leases[&lease].process_id, Some(20));

        let _ = std::fs::remove_file(path.with_file_name(LEASES_LOCK_FILENAME));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn dead_process_leases_stop_counting_toward_the_limit() {
        let path = temp_registry_path("dead-process");
        let lease = acquire_at_path(&path, PathBuf::from("/tmp/plugin"), 1).unwrap();
        track_command_process_at_path(&path, &lease, 99).unwrap();

        let mut registry = load_from_path(&path).unwrap();
        assert!(prune_dead_leases(&mut registry, |process_id| process_id != 99));
        assert!(registry.leases.is_empty());

        let _ = std::fs::remove_file(path.with_file_name(LEASES_LOCK_FILENAME));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn legacy_untracked_leases_remain_conservatively_active() {
        let mut registry: PluginCommandLeaseRegistry = serde_json::from_str(
            r#"{
                "version": 1,
                "next_lease_id": 2,
                "leases": {
                    "plugin-command-1": {"plugin_root": "/tmp/plugin"}
                }
            }"#,
        )
        .unwrap();

        assert!(!prune_dead_leases(&mut registry, |_| false));
        assert_eq!(registry.leases.len(), 1);
        assert_eq!(registry.leases["plugin-command-1"].process_id, None);
    }
}
