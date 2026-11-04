//! Per-tool policy (docs/PLAN_CONTROL_PANEL.md §A2, frozen semantics).
//!
//! File: `<workspace>/.state/tool-policy.json` (same `.state/` the server
//! uses for pid/audit). Created ON FIRST RUN with defaults:
//! `access: "enabled"` for every one of the 55 tools, `safety: true` only for
//! the 12 currently gated tools (SAFETY_CAPABLE). Fresh read on every
//! tools/list and every tools/call. Malformed file → defaults + warn (never
//! crash); unknown tool entries in the file are ignored with a warning.

use std::{
    collections::{BTreeMap, HashMap},
    fs::OpenOptions,
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize};

use crate::tools::REGISTRY;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Enabled,
    Disabled,
    Ask,
}

impl Access {
    pub fn label(&self) -> &'static str {
        match self {
            Access::Enabled => "enabled",
            Access::Disabled => "disabled",
            Access::Ask => "ask",
        }
    }

    pub fn parse(raw: &str) -> Option<Access> {
        match raw {
            "enabled" => Some(Access::Enabled),
            "disabled" => Some(Access::Disabled),
            "ask" => Some(Access::Ask),
            _ => None,
        }
    }
}

impl Serialize for Access {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.label())
    }
}

impl<'de> Deserialize<'de> for Access {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Access, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Access::parse(&raw)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid access value {raw:?}")))
    }
}

/// The 12 tools with real engine checks (shell→sidecar, file→secret paths).
pub const SAFETY_CAPABLE: &[&str] = &[
    "execute_command",
    "start_process",
    "send_input",
    "apply_patch",
    "read_file",
    "write_file",
    "edit_file",
    "multi_edit_file",
    "delete_file",
    "move_file",
    "grep",
    "search_files",
];

/// Whether the engine has a real check for this tool (GUI `safety_capable`).
pub fn safety_capable(tool: &str) -> bool {
    SAFETY_CAPABLE.contains(&tool)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setting {
    pub access: Access,
    #[serde(rename = "safety")]
    pub safety: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    pub version: u8,
    #[serde(rename = "tools")]
    pub tools: BTreeMap<String, Setting>,
}

impl Policy {
    /// The effective setting for a tool (defaults when absent from the file).
    pub fn setting(&self, tool: &str) -> Setting {
        *self.tools.get(tool).unwrap_or(&Setting::default_for(tool))
    }
}

impl Setting {
    /// Default for a tool: access enabled; safety true only for the
    /// 12 safety-capable ones.
    pub fn default_for(tool: &str) -> Setting {
        Setting {
            access: Access::Enabled,
            safety: safety_capable(tool),
        }
    }
}

/// §A2 tools/list visibility: only `access:"disabled"` is hidden.
pub fn is_visible(policy: &Policy, tool: &str) -> bool {
    policy.setting(tool).access != Access::Disabled
}

pub fn policy_path(workspace: &str) -> String {
    format!("{workspace}/.state/tool-policy.json")
}

/// Default policy across the exact registry set (all entries present).
pub fn default_policy() -> Policy {
    let mut tools = BTreeMap::new();
    for spec in REGISTRY {
        tools.insert(spec.name.to_string(), Setting::default_for(spec.name));
    }
    Policy { version: 1, tools }
}

static MALFORMED_WARNED: AtomicBool = AtomicBool::new(false);

/// Last-known-good policy per workspace path (edison major #3): the ONLY
/// fallback for READ/IO failures mid-run (unreadable file keeps the last
/// accepted state). Malformed JSON still falls back to DEFAULTS (§A2 frozen:
/// "fail-open-to-defaults for semantics"); LKG is only refreshed on fully
/// valid reads.
// std Mutex::new isn't const for HashMap::new — init lazily via OnceLock.
static LAST_KNOWN_GOOD: std::sync::OnceLock<Mutex<HashMap<String, Policy>>> =
    std::sync::OnceLock::new();

fn last_known_good() -> &'static Mutex<HashMap<String, Policy>> {
    LAST_KNOWN_GOOD.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Serializes load-modify-write policy mutations (edison minor: POST races).
static WRITE_LOCK: Mutex<()> = Mutex::new(());
/// Unique temp-file counter for atomic writes.
static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Loads the policy for THIS call; malformed → defaults (warn-once per
/// process), missing → defaults (the file is created by `ensure_defaults`
/// at startup, so a missing file here means the state dir was wiped).
pub fn load(workspace: &str) -> Policy {
    let path = policy_path(workspace);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        // Missing file: startup state (ensure_defaults recreates it) →
        // defaults, NOT the last-known-good (a deleted policy is a
        // reset-to-default, matching the plan's "missing → default").
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return default_policy();
        }
        // Any other READ failure (permission changes, disk errors) mid-run:
        // keep the last accepted state — a policy that was previously
        // disabled stays disabled (never silently all-enabled).
        Err(e) => {
            let lkg = last_known_good().lock().unwrap().get(workspace).cloned();
            if !MALFORMED_WARNED.swap(true, Ordering::SeqCst) {
                eprintln!("tool-policy: {path} unreadable ({e}); keeping last-known-good policy");
            }
            return lkg.unwrap_or_else(default_policy);
        }
    };
    match serde_json::from_str::<Policy>(&raw) {
        Ok(p) if validate(&p) => {
            let unknown = p
                .tools
                .keys()
                .any(|name| !REGISTRY.iter().any(|s| s.name == name));
            if unknown && !MALFORMED_WARNED.swap(true, Ordering::SeqCst) {
                eprintln!("tool-policy: ignoring unknown tool entries in {path}");
            }
            last_known_good()
                .lock()
                .unwrap()
                .insert(workspace.to_string(), p.clone());
            p
        }
        _ => {
            if !MALFORMED_WARNED.swap(true, Ordering::SeqCst) {
                eprintln!("tool-policy: {path} is malformed; falling back to defaults");
            }
            default_policy()
        }
    }
}

/// Structural validation: known tools only, valid access strings, and every
/// registry tool present. Anything else counts as malformed → defaults.
fn validate(p: &Policy) -> bool {
    let ok_values = p
        .tools
        .values()
        .all(|s| matches!(s.access, Access::Enabled | Access::Disabled | Access::Ask));
    let complete = REGISTRY.iter().all(|s| p.tools.contains_key(s.name));
    ok_values && complete
}

/// Creates the default file on first run (0600, atomic temp+rename). Never
/// overwrites an existing policy.
pub fn ensure_defaults(workspace: &str) {
    let path = policy_path(workspace);
    if Path::new(&path).exists() {
        return;
    }
    let _ = std::fs::create_dir_all(format!("{workspace}/.state"));
    // Best-effort on first run: a failed write leaves the defaults dictating
    // behavior in-memory (load falls back when the file is missing).
    let _ = write_atomic(workspace, &default_policy());
}

/// Atomic write: temp file in the same dir (0600 on unix) + rename.
pub fn write_atomic(workspace: &str, policy: &Policy) -> Result<(), String> {
    let path = policy_path(workspace);
    let dir = Path::new(&path).parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let seq = WRITE_SEQ.fetch_add(1, Ordering::SeqCst);
    let tmp = dir.join(format!(
        ".tool-policy.json-{}-{seq}.tmp",
        std::process::id()
    ));
    let body = serde_json::to_string_pretty(policy).map_err(|e| e.to_string())?;
    {
        let open = || {
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp)
            }
            #[cfg(not(unix))]
            {
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&tmp)
            }
        };
        let mut f = open().map_err(|e| e.to_string())?;
        use std::io::Write;
        f.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Single-entry mutation used by POST /api/tools/policy: loads the current
/// (or default) policy, applies the validated values, writes atomically,
/// returns the updated object.
pub fn update_entry(
    workspace: &str,
    tool: &str,
    access: Access,
    safety: bool,
) -> Result<Policy, String> {
    // Serialize load-modify-write so two concurrent POSTs cannot lose one
    // another's change (both then persist, in either order).
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut policy = load(workspace);
    policy
        .tools
        .insert(tool.to_string(), Setting { access, safety });
    write_atomic(workspace, &policy)?;
    last_known_good()
        .lock()
        .unwrap()
        .insert(workspace.to_string(), policy.clone());
    Ok(policy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "ryns-policy-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    /// §A5: defaults have exactly the registry set, enabled, and safety=true
    /// only for the 12 safety-capable tools.
    #[test]
    fn default_policy_shape() {
        let p = default_policy();
        assert_eq!(p.version, 1);
        assert_eq!(p.tools.len(), REGISTRY.len());
        for spec in REGISTRY {
            let s = &p.tools[spec.name];
            assert_eq!(s.access, Access::Enabled, "{}", spec.name);
            assert_eq!(s.safety, safety_capable(spec.name), "{}", spec.name);
        }
        assert_eq!(SAFETY_CAPABLE.len(), 12);
    }

    /// §A5: missing file → defaults; malformed → defaults (+ no crash);
    /// unknown tool entries ignored; invalid access/missing entries →
    /// malformed → defaults.
    #[test]
    fn load_fallback_matrix() {
        let w = ws("fallback");
        // missing → defaults
        assert_eq!(load(&w), default_policy());

        // written good policy round-trips.
        ensure_defaults(&w);
        assert_eq!(load(&w), default_policy());
        let mut good = default_policy();
        good.tools.insert(
            "execute_command".into(),
            Setting {
                access: Access::Ask,
                safety: false,
            },
        );
        good.tools.insert(
            "edit_file".into(),
            Setting {
                access: Access::Disabled,
                safety: true,
            },
        );
        write_atomic(&w, &good).unwrap();
        assert_eq!(load(&w).tools["execute_command"].access, Access::Ask);
        assert_eq!(load(&w).tools["edit_file"].access, Access::Disabled);

        // malformed JSON → defaults.
        std::fs::write(policy_path(&w), "{not json").unwrap();
        assert_eq!(load(&w), default_policy());

        // unknown tool entries are tolerated (registry entries still load);
        // partially-populated maps are INVALID (missing entries → defaults).
        let mut with_unknown = default_policy();
        with_unknown.tools.insert(
            "nope_one".into(),
            Setting {
                access: Access::Disabled,
                safety: false,
            },
        );
        with_unknown.tools.insert(
            "edit_file".into(),
            Setting {
                access: Access::Ask,
                safety: true,
            },
        );
        with_unknown.tools.insert(
            "execute_command".into(),
            Setting {
                access: Access::Enabled,
                safety: false,
            },
        );
        write_atomic(&w, &with_unknown).unwrap();
        let loaded = load(&w);
        assert_eq!(loaded.tools["edit_file"].access, Access::Ask);
        assert!(!loaded.tools["execute_command"].safety);

        // Missing registry entries → malformed → defaults.
        std::fs::write(
            policy_path(&w),
            r#"{ "version": 1, "tools": { "edit_file": {"access":"ask","safety":true} } }"#,
        )
        .unwrap();
        assert_eq!(load(&w), default_policy());

        // invalid access value → malformed → defaults.
        std::fs::write(
            policy_path(&w),
            r#"{ "version": 1, "tools": { "edit_file": {"access":"clown","safety":true}, "execute_command": {"access":"enabled","safety":false} } }"#,
        )
        .unwrap();
        assert_eq!(load(&w), default_policy());
    }

    /// 0600 + atomic write on unix.
    #[cfg(unix)]
    #[test]
    fn written_file_permissions_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let w = ws("perm");
        ensure_defaults(&w);
        let mode = std::fs::metadata(policy_path(&w))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "policy file perm: {mode:o}");
    }

    /// §A5 (edison major #3): a read failure mid-run keeps LAST policy, not
    /// defaults. Skips when running as root (root reads chmod-000 files
    /// anyway, so the failure path can't be triggered).
    #[cfg(unix)]
    #[test]
    fn io_error_keeps_last_known_good() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("io_error_keeps_last_known_good: skipped (running as root)");
            return;
        }
        let w = ws("lkg");
        ensure_defaults(&w);
        // Establish a NON-default state: execute disabled.
        let mut disabled = default_policy();
        disabled.tools.insert(
            "execute_command".into(),
            Setting {
                access: Access::Disabled,
                safety: true,
            },
        );
        write_atomic(&w, &disabled).unwrap();
        assert_eq!(load(&w).tools["execute_command"].access, Access::Disabled);
        // Sanity: LKG map actually holds the disabled state.
        assert_eq!(
            last_known_good().lock().unwrap().get(&w).unwrap().tools["execute_command"].access,
            Access::Disabled,
            "lkg must have been recorded by the earlier successful read"
        );
        // Make the file unreadable mid-run.
        let path = policy_path(&w);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(
            load(&w).tools["execute_command"].access,
            Access::Disabled,
            "IO failure must keep the last-known-good policy"
        );
        // Restore for cleanup on systems where removal needs perms.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
        let _ = std::fs::remove_file(&path);
    }

    /// §A5: malformed JSON still falls back to DEFAULTS (frozen, unchanged).
    #[cfg(unix)]
    #[test]
    fn malformed_falls_back_to_defaults() {
        let w = ws("malformed");
        ensure_defaults(&w);
        std::fs::write(policy_path(&w), "{ nope").unwrap();
        assert_eq!(load(&w), default_policy());
    }

    /// edison minor: two concurrent policy writes BOTH persist.
    #[cfg(unix)]
    #[test]
    fn concurrent_updates_both_persist() {
        use std::sync::Arc;
        let w = ws("race");
        ensure_defaults(&w);
        let w2 = Arc::new(w.clone());
        let t1 = std::thread::spawn({
            let w = Arc::clone(&w2);
            move || update_entry(&w, "edit_file", Access::Ask, true).unwrap()
        });
        let t2 = std::thread::spawn({
            let w = Arc::clone(&w2);
            move || update_entry(&w, "screenshot", Access::Disabled, false).unwrap()
        });
        t1.join().unwrap();
        t2.join().unwrap();
        let now = load(&w);
        assert!(now.tools["edit_file"].access == Access::Ask);
        assert!(now.tools["screenshot"].access == Access::Disabled);
    }

    #[test]
    fn access_parse_and_labels() {
        assert_eq!(Access::parse("enabled"), Some(Access::Enabled));
        assert_eq!(Access::parse("disabled"), Some(Access::Disabled));
        assert_eq!(Access::parse("ask"), Some(Access::Ask));
        assert_eq!(Access::parse("clown"), None);
        assert_eq!(Access::Enabled.label(), "enabled");
    }

    #[test]
    fn update_entry_roundtrip() {
        let w = ws("update");
        ensure_defaults(&w);
        let p = update_entry(&w, "edit_file", Access::Ask, true).unwrap();
        assert_eq!(p.tools["edit_file"].access, Access::Ask);
        assert!(p.tools["edit_file"].safety);
        assert_eq!(load(&w).tools["edit_file"].access, Access::Ask);
        // Other entries untouched (defaults still there).
        assert_eq!(load(&w).tools["execute_command"].access, Access::Enabled);
        assert!(safety_capable("execute_command"));
        assert!(!safety_capable("screenshot"));
    }
}
