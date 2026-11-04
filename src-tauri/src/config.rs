use std::fs;
use std::path::{Path, PathBuf};

use crate::models::SafetySettings;

const CONFIG_FILE_NAME: &str = "safety-settings.json";

pub fn get_config_path(config_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = config_dir {
        dir.join(CONFIG_FILE_NAME)
    } else {
        PathBuf::from(CONFIG_FILE_NAME)
    }
}

pub fn load_settings(config_dir: Option<&Path>) -> SafetySettings {
    let path = get_config_path(config_dir);
    if path.exists() {
        if let Ok(contents) = fs::read_to_string(&path) {
            if let Ok(settings) = serde_json::from_str::<SafetySettings>(&contents) {
                return settings;
            }
        }
    }

    // Default with environment variable fallback
    let mut default = SafetySettings::default();
    if let Ok(url) = std::env::var("RYNS_PUBLIC_BASE_URL") {
        if !url.trim().is_empty() {
            default.server_url = url.trim().to_string();
        }
    }
    if let Ok(token) = std::env::var("RYNS_AUTH_TOKEN") {
        let first = token.split(',').next().unwrap_or("").trim();
        if !first.is_empty() {
            default.auth_token = first.to_string();
        }
    }
    default
}

pub fn save_settings(config_dir: Option<&Path>, settings: &SafetySettings) -> Result<(), String> {
    let path = get_config_path(config_dir);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let data = serde_json::to_string_pretty(settings)
        .map_err(|e| format!("Failed to serialize settings: {e}"))?;

    #[cfg(unix)]
    {
        use std::fs::{set_permissions, OpenOptions, Permissions};
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("Failed to open {} with mode 0600: {e}", path.display()))?;

        // Tighten permissions after open so pre-existing files are also set to 0600
        let _ = set_permissions(&path, Permissions::from_mode(0o600));

        file.write_all(data.as_bytes())
            .map_err(|e| format!("Failed to write settings to {}: {e}", path.display()))?;
    }

    #[cfg(not(unix))]
    {
        fs::write(&path, data)
            .map_err(|e| format!("Failed to write settings to {}: {e}", path.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_save_and_load_settings_mode_0600() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let test_dir = std::env::temp_dir().join(format!("ryns-test-config-{nanos}"));
        let _ = fs::create_dir_all(&test_dir);

        let settings = SafetySettings {
            server_url: "http://127.0.0.1:9999".to_string(),
            auth_token: "secret-token-xyz".to_string(),
            ..SafetySettings::default()
        };

        save_settings(Some(&test_dir), &settings).expect("save_settings failed");
        let loaded = load_settings(Some(&test_dir));
        assert_eq!(loaded, settings);

        #[cfg(unix)]
        {
            use std::fs::{set_permissions, Permissions};
            use std::os::unix::fs::PermissionsExt;
            let path = get_config_path(Some(&test_dir));
            let meta = fs::metadata(&path).expect("metadata failed");
            let mode = meta.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "Config file permissions must be 0600");

            // Intentionally loosen permissions to 0o644 to test tightening
            set_permissions(&path, Permissions::from_mode(0o644)).expect("set loose perms");
            let loose_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(loose_mode, 0o644);

            // Re-save settings over pre-existing loose file
            save_settings(Some(&test_dir), &settings).expect("second save_settings failed");
            let tightened = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                tightened, 0o600,
                "Pre-existing loose file must be tightened to 0600"
            );
        }

        let _ = fs::remove_dir_all(&test_dir);
    }
}
