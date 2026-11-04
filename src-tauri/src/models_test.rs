use crate::models::{DecidePayload, PendingRequest};

#[test]
fn test_deserialize_pending_numeric_id() {
    let json = r#"{
            "id": 1042,
            "tool": "execute_command",
            "command": "cat /etc/passwd",
            "cwd": "/home/user",
            "reason": "sensitive file read",
            "ruleId": "secret-path.passwd",
            "created_at": 1700000000000,
            "expires_at": 1700000120000
        }"#;

    let req: PendingRequest = serde_json::from_str(json).expect("should parse");
    assert_eq!(req.id, "1042");
    assert_eq!(req.tool, "execute_command");
    assert_eq!(req.command, "cat /etc/passwd");
    assert_eq!(req.cwd, "/home/user");
    assert_eq!(req.reason, "sensitive file read");
    assert_eq!(req.rule_id, "secret-path.passwd");
    assert_eq!(req.created_at, Some(1700000000000));
    assert_eq!(req.expires_at, Some(1700000120000));
}

#[test]
fn test_deserialize_pending_string_id_and_snake_case_rule_id() {
    let json = r#"{
            "id": "req-xyz-99",
            "tool": "start_process",
            "command": "rm -rf /tmp/test",
            "cwd": "/var/tmp",
            "reason": "recursive delete",
            "rule_id": "dangerous.rm",
            "created_at": "2026-11-03T10:00:00Z",
            "expires_at": "2026-11-03T10:02:00Z"
        }"#;

    let req: PendingRequest = serde_json::from_str(json).expect("should parse");
    assert_eq!(req.id, "req-xyz-99");
    assert_eq!(req.rule_id, "dangerous.rm");
    assert_eq!(req.created_at, Some(1793700000000));
    assert_eq!(req.expires_at, Some(1793700120000));
}

#[test]
fn test_deserialize_pending_fractional_seconds() {
    let json = r#"{
            "id": "frac-1",
            "tool": "execute_command",
            "command": "whoami",
            "cwd": "/tmp",
            "reason": "check",
            "rule_id": "test.frac",
            "created_at": "2026-11-03T10:00:00.123456789Z",
            "expires_at": "2026-11-03T10:00:00.5Z"
        }"#;

    let req: PendingRequest = serde_json::from_str(json).expect("should parse");
    assert_eq!(req.created_at, Some(1793700000123));
    assert_eq!(req.expires_at, Some(1793700000500));
}

#[test]
fn test_deserialize_pending_seconds_integer() {
    let json = r#"{
            "id": "sec-1",
            "tool": "execute_command",
            "command": "uptime",
            "cwd": "/tmp",
            "reason": "check",
            "rule_id": "test.sec",
            "created_at": 1000,
            "expires_at": 2000
        }"#;

    let req: PendingRequest = serde_json::from_str(json).expect("should parse");
    assert_eq!(req.created_at, Some(1000000));
    assert_eq!(req.expires_at, Some(2000000));
}

#[test]
fn test_decide_payload_json() {
    let payload = DecidePayload {
        decision: "allow_session".to_string(),
    };
    let serialized = serde_json::to_string(&payload).expect("should serialize");
    assert_eq!(serialized, r#"{"decision":"allow_session"}"#);
}

#[test]
fn test_deserialize_server_config_real_shape() {
    let json = r#"{
        "version": "1.0.0",
        "port": 8799,
        "mode": "enforce",
        "auth": false,
        "tool_counts": { "enabled": 52, "ask": 2, "disabled": 1 },
        "workspace": "/Users/operator/RynsWorkspace",
        "server_pid": 48100
    }"#;

    let cfg: crate::models::ServerConfigResponse =
        serde_json::from_str(json).expect("should parse real server config");
    assert_eq!(cfg.version, "1.0.0");
    assert_eq!(cfg.port, 8799);
    assert_eq!(cfg.mode, "enforce");
    assert!(!cfg.auth);
    assert_eq!(cfg.tool_counts.enabled, 52);
    assert_eq!(cfg.tool_counts.ask, 2);
    assert_eq!(cfg.tool_counts.disabled, 1);
    assert_eq!(cfg.workspace, "/Users/operator/RynsWorkspace");
    assert_eq!(cfg.server_pid, Some(48100));
}

#[test]
fn test_deserialize_server_config_tolerant_aliases() {
    let json = r#"{
        "version": "0.9.0",
        "port": 8799,
        "safety_mode": "observe",
        "auth_enabled": true,
        "tool_counts": { "enabled": 55, "ask": 0, "disabled": 0 },
        "workspace_root": "/custom/path"
    }"#;

    let cfg: crate::models::ServerConfigResponse =
        serde_json::from_str(json).expect("should parse aliased server config");
    assert_eq!(cfg.mode, "observe");
    assert!(cfg.auth);
    assert_eq!(cfg.workspace, "/custom/path");
    assert_eq!(cfg.server_pid, None);
}

#[test]
fn test_server_process_status_serde_is_external() {
    use crate::models::ServerProcessStatus;

    let status = ServerProcessStatus {
        running: true,
        is_external: true,
        pid: Some(54321),
        port: 8799,
        message: "Running as external instance".to_string(),
    };

    // Assert serialization key is exact "is_external"
    let serialized = serde_json::to_string(&status).expect("must serialize");
    assert!(
        serialized.contains("\"is_external\":true"),
        "Serialized JSON must use key 'is_external': {serialized}"
    );
    assert!(
        !serialized.contains("\"external\":"),
        "Serialized JSON must not contain old key 'external': {serialized}"
    );

    // Deserialization round-trip
    let roundtrip: ServerProcessStatus =
        serde_json::from_str(&serialized).expect("must deserialize roundtrip");
    assert_eq!(roundtrip, status);

    // Tolerant deserialization of legacy alias 'external'
    let legacy_json = r#"{"running":true,"external":true,"pid":54321,"port":8799,"message":"ok"}"#;
    let legacy: ServerProcessStatus =
        serde_json::from_str(legacy_json).expect("must deserialize legacy alias");
    assert!(legacy.is_external);
}
