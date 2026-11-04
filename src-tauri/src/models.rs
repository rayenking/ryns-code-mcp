use serde::{Deserialize, Deserializer, Serialize};

pub fn deserialize_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IdValue {
        Str(String),
        Num(u64),
        Signed(i64),
    }

    match IdValue::deserialize(deserializer)? {
        IdValue::Str(s) => Ok(s),
        IdValue::Num(n) => Ok(n.to_string()),
        IdValue::Signed(n) => Ok(n.to_string()),
    }
}

fn days_from_civil(mut y: i64, m: u64, d: u64) -> i64 {
    y -= (m <= 2) as i64;
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

pub fn parse_iso8601_to_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let (date_part, rest) = s.split_at(10);
    let mut date_iter = date_part.split('-');
    let year: i64 = date_iter.next()?.parse().ok()?;
    let month: u64 = date_iter.next()?.parse().ok()?;
    let day: u64 = date_iter.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let time_str = rest.strip_prefix('T').or_else(|| rest.strip_prefix(' '))?;
    let mut time_end = time_str.len();
    let mut tz_offset_secs: i64 = 0;
    if let Some(pos) = time_str.find(['Z', '+', '-']) {
        time_end = pos;
        let tz_str = &time_str[pos..];
        if tz_str != "Z" {
            let sign = if tz_str.starts_with('+') { 1 } else { -1 };
            let tz_digits = &tz_str[1..];
            let mut parts = tz_digits.split(':');
            let h: i64 = parts.next()?.parse().ok()?;
            let m: i64 = parts.next().unwrap_or("0").parse().ok()?;
            tz_offset_secs = sign * (h * 3600 + m * 60);
        }
    }

    let main_time = &time_str[..time_end];
    let mut time_parts = main_time.split('.');
    let hms = time_parts.next()?;
    let fraction = time_parts.next();

    let mut hms_parts = hms.split(':');
    let hour: i64 = hms_parts.next()?.parse().ok()?;
    let min: i64 = hms_parts.next()?.parse().ok()?;
    let sec: i64 = hms_parts.next()?.parse().ok()?;

    let mut ms: u64 = 0;
    if let Some(frac) = fraction {
        let mut digits = frac
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>();
        if digits.len() > 3 {
            digits.truncate(3);
        }
        while digits.len() < 3 {
            digits.push('0');
        }
        ms = digits.parse().ok()?;
    }

    let days = days_from_civil(year, month, day);
    let total_secs = days * 86400 + hour * 3600 + min * 60 + sec - tz_offset_secs;
    if total_secs < 0 {
        return None;
    }
    Some((total_secs as u64) * 1000 + ms)
}

pub fn deserialize_timestamp<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<serde_json::Value>::deserialize(deserializer)?;
    match opt {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(num)) => {
            if let Some(n) = num.as_u64() {
                if n < 100_000_000_000 {
                    Ok(Some(n * 1000))
                } else {
                    Ok(Some(n))
                }
            } else if let Some(f) = num.as_f64() {
                if f < 100_000_000_000.0 {
                    Ok(Some((f * 1000.0) as u64))
                } else {
                    Ok(Some(f as u64))
                }
            } else {
                Ok(None)
            }
        }
        Some(serde_json::Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            if let Ok(n) = trimmed.parse::<u64>() {
                if n < 100_000_000_000 {
                    return Ok(Some(n * 1000));
                } else {
                    return Ok(Some(n));
                }
            }
            if let Ok(f) = trimmed.parse::<f64>() {
                if f < 100_000_000_000.0 {
                    return Ok(Some((f * 1000.0) as u64));
                } else {
                    return Ok(Some(f as u64));
                }
            }
            if let Some(ms) = parse_iso8601_to_ms(trimmed) {
                Ok(Some(ms))
            } else {
                Ok(None)
            }
        }
        _ => Ok(None),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingRequest {
    #[serde(deserialize_with = "deserialize_id")]
    pub id: String,
    #[serde(default)]
    pub tool: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default, rename = "ruleId", alias = "rule_id")]
    pub rule_id: String,
    #[serde(default, deserialize_with = "deserialize_timestamp")]
    pub created_at: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_timestamp")]
    pub expires_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PendingListResponse {
    #[serde(default)]
    pub pending: Vec<PendingRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecidePayload {
    pub decision: String, // "allow" | "deny" | "allow_session"
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DecideResponse {
    pub ok: Option<bool>,
    pub error: Option<String>,
}

fn default_server_url() -> String {
    "http://127.0.0.1:8799".to_string()
}

fn default_port() -> u16 {
    8799
}

fn default_safety_mode() -> String {
    "enforce".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SafetySettings {
    #[serde(default = "default_server_url")]
    pub server_url: String,
    #[serde(default)]
    pub auth_token: String,
    #[serde(default = "default_port")]
    pub server_port: u16,
    #[serde(default)]
    pub server_binary_path: String,
    #[serde(default = "default_safety_mode")]
    pub safety_mode: String,
    #[serde(default)]
    pub autostart: bool,
}

impl Default for SafetySettings {
    fn default() -> Self {
        Self {
            server_url: default_server_url(),
            auth_token: String::new(),
            server_port: default_port(),
            server_binary_path: String::new(),
            safety_mode: default_safety_mode(),
            autostart: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditRecord {
    pub id: String,
    pub tool: String,
    pub command: String,
    pub cwd: String,
    pub reason: String,
    #[serde(rename = "ruleId", alias = "rule_id")]
    pub rule_id: String,
    pub decision: String,   // "allow", "deny", "allow_session"
    pub decided_at: String, // ISO timestamp or epoch
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionTestResult {
    pub ok: bool,
    pub message: String,
    pub pending_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SafetyEvent {
    #[serde(rename = "pending")]
    Pending { request: PendingRequest },
    #[serde(rename = "resolved")]
    Resolved {
        id: String,
        decision: Option<String>,
    },
    #[serde(rename = "status")]
    Status { connected: bool, message: String },
}

// ============================================================================
// Control Panel Models (PLAN_CONTROL_PANEL.md §A3)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ToolCounts {
    #[serde(default)]
    pub enabled: usize,
    #[serde(default)]
    pub ask: usize,
    #[serde(default)]
    pub disabled: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ServerConfigResponse {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default, alias = "safety_mode")]
    pub mode: String,
    #[serde(default, alias = "auth_enabled")]
    pub auth: bool,
    #[serde(default)]
    pub tool_counts: ToolCounts,
    #[serde(default, alias = "workspace_root")]
    pub workspace: String,
    #[serde(default)]
    pub server_pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ToolInfo {
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub access: String, // "enabled" | "disabled" | "ask"
    #[serde(default)]
    pub safety: bool,
    #[serde(default)]
    pub safety_capable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolsListResponse {
    Wrapped {
        #[serde(default)]
        tools: Vec<ToolInfo>,
    },
    List(Vec<ToolInfo>),
}

impl ToolsListResponse {
    pub fn into_tools(self) -> Vec<ToolInfo> {
        match self {
            ToolsListResponse::Wrapped { tools } => tools,
            ToolsListResponse::List(tools) => tools,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateToolPolicyPayload {
    pub tool: String,
    pub access: String, // "enabled" | "disabled" | "ask"
    pub safety: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerProcessStatus {
    pub running: bool,
    #[serde(rename = "is_external", alias = "external")]
    pub is_external: bool, // true if served by another pre-existing process
    pub pid: Option<u32>,
    pub port: u16,
    pub message: String,
}

#[cfg(test)]
#[path = "models_test.rs"]
mod tests;
