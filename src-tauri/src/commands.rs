use tauri::{AppHandle, State};
use tauri_plugin_autostart::ManagerExt;

use crate::client::{
    fetch_pending, fetch_server_config as fetch_config_client,
    fetch_tools_list as fetch_tools_client, post_decision, test_connection as test_conn_client,
    update_tool_policy as update_policy_client,
};
use crate::config::save_settings as persist_settings;
use crate::models::{
    AuditRecord, ConnectionTestResult, PendingRequest, SafetySettings, ServerConfigResponse,
    ServerProcessStatus, ToolInfo, UpdateToolPolicyPayload,
};
use crate::state::AppState;

// ============================================================================
// Settings Commands
// ============================================================================

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> Result<SafetySettings, String> {
    let s = state.settings.read().await;
    Ok(s.clone())
}

#[tauri::command]
pub async fn save_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    settings: SafetySettings,
) -> Result<(), String> {
    persist_settings(state.config_dir.as_deref(), &settings)?;
    {
        let mut s = state.settings.write().await;
        *s = settings;
    }
    state.start_sse_loop(app).await;
    Ok(())
}

#[tauri::command]
pub async fn test_connection(
    state: State<'_, AppState>,
    url: Option<String>,
    token: Option<String>,
) -> Result<ConnectionTestResult, String> {
    let (target_url, target_token) = {
        let s = state.settings.read().await;
        (
            url.unwrap_or_else(|| s.server_url.clone()),
            token.unwrap_or_else(|| s.auth_token.clone()),
        )
    };

    test_conn_client(&state.client, &target_url, &target_token).await
}

// ============================================================================
// Server Process Lifecycle Commands (PLAN_CONTROL_PANEL.md §0 / §B)
// ============================================================================

#[tauri::command]
pub async fn start_server(state: State<'_, AppState>) -> Result<ServerProcessStatus, String> {
    let settings = {
        let s = state.settings.read().await;
        s.clone()
    };
    state.server_process.start(&settings).await
}

#[tauri::command]
pub async fn stop_server(state: State<'_, AppState>) -> Result<ServerProcessStatus, String> {
    state.server_process.stop().await
}

#[tauri::command]
pub async fn restart_server(state: State<'_, AppState>) -> Result<ServerProcessStatus, String> {
    let settings = {
        let s = state.settings.read().await;
        s.clone()
    };
    state.server_process.restart(&settings).await
}

#[tauri::command]
pub async fn get_server_status(state: State<'_, AppState>) -> Result<ServerProcessStatus, String> {
    let port = {
        let s = state.settings.read().await;
        s.server_port
    };
    Ok(state.server_process.status(port))
}

// ============================================================================
// Control Panel Data Commands (PLAN_CONTROL_PANEL.md §A3)
// ============================================================================

#[tauri::command]
pub async fn fetch_server_config(
    state: State<'_, AppState>,
) -> Result<ServerConfigResponse, String> {
    let (url, token) = {
        let s = state.settings.read().await;
        (s.server_url.clone(), s.auth_token.clone())
    };

    fetch_config_client(&state.client, &url, &token).await
}

#[tauri::command]
pub async fn fetch_tools_list(state: State<'_, AppState>) -> Result<Vec<ToolInfo>, String> {
    let (url, token) = {
        let s = state.settings.read().await;
        (s.server_url.clone(), s.auth_token.clone())
    };

    fetch_tools_client(&state.client, &url, &token).await
}

#[tauri::command]
pub async fn update_tool_policy(
    state: State<'_, AppState>,
    payload: UpdateToolPolicyPayload,
) -> Result<serde_json::Value, String> {
    let (url, token) = {
        let s = state.settings.read().await;
        (s.server_url.clone(), s.auth_token.clone())
    };

    update_policy_client(&state.client, &url, &token, &payload).await
}

// ============================================================================
// Autostart Commands (tauri-plugin-autostart)
// ============================================================================

#[tauri::command]
pub async fn get_autostart(app: AppHandle) -> Result<bool, String> {
    #[cfg(desktop)]
    {
        app.autolaunch()
            .is_enabled()
            .map_err(|e| format!("Failed to query autostart status: {e}"))
    }
    #[cfg(not(desktop))]
    {
        let _ = app;
        Ok(false)
    }
}

#[tauri::command]
pub async fn set_autostart(
    app: AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<bool, String> {
    #[cfg(desktop)]
    {
        let autolaunch = app.autolaunch();
        if enabled {
            autolaunch
                .enable()
                .map_err(|e| format!("Failed to enable autostart: {e}"))?;
        } else {
            autolaunch
                .disable()
                .map_err(|e| format!("Failed to disable autostart: {e}"))?;
        }
    }

    {
        let mut s = state.settings.write().await;
        s.autostart = enabled;
        let _ = persist_settings(state.config_dir.as_deref(), &s);
    }

    Ok(enabled)
}

// ============================================================================
// Safety Console Commands (Existing ask / decide / history preserved)
// ============================================================================

#[tauri::command]
pub async fn list_pending(state: State<'_, AppState>) -> Result<Vec<PendingRequest>, String> {
    let (url, token) = {
        let s = state.settings.read().await;
        (s.server_url.clone(), s.auth_token.clone())
    };

    match fetch_pending(&state.client, &url, &token).await {
        Ok(remote_list) => {
            let mut p = state.pending.write().await;
            *p = remote_list.clone();
            Ok(remote_list)
        }
        Err(_) => {
            let p = state.pending.read().await;
            Ok(p.clone())
        }
    }
}

#[tauri::command]
pub async fn decide(
    state: State<'_, AppState>,
    id: String,
    decision: String,
) -> Result<(), String> {
    let (url, token) = {
        let s = state.settings.read().await;
        (s.server_url.clone(), s.auth_token.clone())
    };

    let valid_decision = match decision.as_str() {
        "allow" | "deny" | "allow_session" => decision.as_str(),
        _ => {
            return Err(format!(
                "Invalid decision '{decision}'. Expected allow, deny, or allow_session"
            ))
        }
    };

    post_decision(&state.client, &url, &token, &id, valid_decision).await?;

    let removed = {
        let mut p = state.pending.write().await;
        let mut found = None;
        if let Some(pos) = p.iter().position(|r| r.id == id) {
            found = Some(p.remove(pos));
        }
        found
    };

    let now_str = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();

    let record = if let Some(req) = removed {
        AuditRecord {
            id: req.id,
            tool: req.tool,
            command: req.command,
            cwd: req.cwd,
            reason: req.reason,
            rule_id: req.rule_id,
            decision: valid_decision.to_string(),
            decided_at: now_str,
        }
    } else {
        AuditRecord {
            id: id.clone(),
            tool: String::new(),
            command: String::new(),
            cwd: String::new(),
            reason: String::new(),
            rule_id: String::new(),
            decision: valid_decision.to_string(),
            decided_at: now_str,
        }
    };

    {
        let mut audit = state.audit.write().await;
        audit.insert(0, record);
        if audit.len() > 100 {
            audit.truncate(100);
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn get_audit_history(state: State<'_, AppState>) -> Result<Vec<AuditRecord>, String> {
    let audit = state.audit.read().await;
    Ok(audit.clone())
}

#[tauri::command]
pub async fn clear_audit_history(state: State<'_, AppState>) -> Result<(), String> {
    let mut audit = state.audit.write().await;
    audit.clear();
    Ok(())
}

#[tauri::command]
pub async fn reconnect_sse(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    state.start_sse_loop(app).await;
    Ok(())
}
