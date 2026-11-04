//! macOS-only automation helpers mirroring the platform.go
//! "keyboard / mouse / windows / notifications (macOS)" section and
//! `darwinSystemStats` (platform.go ~125-445). These need Accessibility
//! permission for whatever launches the server. Compiled on
//! `target_os = "macos"` only; Linux builds take the xdotool paths inline in
//! the desktop tools.

use std::time::Duration;

use crate::platform::{run_plain, run_raw_command};

/// Go `asQuote`: an AppleScript string literal.
pub fn as_quote(s: &str) -> String {
    let s = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{s}\"")
}

/// Go `osascript`: osascript -e <script> with runPlain semantics.
pub async fn osascript(timeout: Duration, script: &str) -> (String, Option<String>) {
    run_plain(
        "osascript",
        &["-e".to_string(), script.to_string()],
        "",
        timeout,
    )
    .await
}

/// Go macTypeText (platform.go ~128).
pub async fn mac_type_text(text: &str) -> (String, Option<String>) {
    let script = format!(
        "tell application \"System Events\" to keystroke {}",
        as_quote(text)
    );
    osascript(Duration::from_secs(30), &script).await
}

/// Go macKeyCodes (platform.go ~133).
pub fn mac_key_code(key: &str) -> Option<i32> {
    Some(match key {
        "return" => 36,
        "enter" => 76,
        "tab" => 48,
        "space" => 49,
        "escape" | "esc" => 53,
        "backspace" | "delete" => 51,
        "forwarddelete" => 117,
        "up" => 126,
        "down" => 125,
        "left" => 123,
        "right" => 124,
        "home" => 115,
        "end" => 119,
        "pageup" | "page_up" | "prior" => 116,
        "pagedown" | "page_down" | "next" => 121,
        "f1" => 122,
        "f2" => 120,
        "f3" => 99,
        "f4" => 118,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f8" => 100,
        "f9" => 101,
        "f10" => 109,
        "f11" => 103,
        "f12" => 111,
        _ => return None,
    })
}

/// Go macKeyScript (platform.go ~143): xdotool-style key syntax
/// ("ctrl+c", "cmd+shift+t", space-separated combos) → AppleScript.
pub fn mac_key_script(keys: &str) -> Result<String, String> {
    let combos: Vec<&str> = keys.split_whitespace().collect();
    if combos.is_empty() {
        return Err("no keys given".to_string());
    }
    let mut sb = String::from("tell application \"System Events\"\n");
    for combo in combos {
        let parts: Vec<&str> = combo.split('+').collect();
        let key = parts[parts.len() - 1].to_lowercase();
        let mut mods: Vec<&str> = Vec::new();
        for m in &parts[..parts.len() - 1] {
            match m.to_lowercase().as_str() {
                "ctrl" | "control" => mods.push("control down"),
                "alt" | "option" | "opt" => mods.push("option down"),
                "shift" => mods.push("shift down"),
                "cmd" | "command" | "super" | "meta" | "win" => mods.push("command down"),
                other => return Err(format!("unknown modifier {other:?}")),
            }
        }
        let using = if mods.is_empty() {
            String::new()
        } else {
            format!(" using {{ {} }}", mods.join(", "))
        };
        if let Some(code) = mac_key_code(&key) {
            sb.push_str(&format!("key code {code}{using}\n"));
        } else if key.chars().count() == 1 {
            sb.push_str(&format!("keystroke {}{using}\n", as_quote(&key)));
        } else {
            return Err(format!("unsupported key {combo:?} on macOS"));
        }
    }
    sb.push_str("end tell");
    Ok(sb)
}

/// Go macPressKeys (platform.go ~186).
pub async fn mac_press_keys(keys: &str) -> ((), Option<String>) {
    let script = match mac_key_script(keys) {
        Ok(s) => s,
        Err(e) => return ((), Some(e)),
    };
    let (out, err) = osascript(Duration::from_secs(10), &script).await;
    match err {
        Some(e) => ((), Some(format!("{e}: {}", out.trim()))),
        None => ((), None),
    }
}

/// Go macMouseClick (platform.go ~197): needs cliclick
/// (`brew install cliclick`).
pub async fn mac_mouse_click(
    x: i64,
    y: i64,
    button: i64,
    double: bool,
) -> (String, Option<String>) {
    let look = std::process::Command::new("cliclick").arg("-h").output();
    let cliclick_missing = match &look {
        Err(_) => true,
        Ok(o) => o.status.code().is_none(),
    };
    if cliclick_missing {
        return (
            String::new(),
            Some("cliclick tidak ditemukan; install dengan: brew install cliclick".to_string()),
        );
    }
    let verb_res: Result<&str, String> = if button == 2 {
        Err("middle click tidak didukung di macOS".to_string())
    } else if button == 3 {
        Ok("rc")
    } else if double {
        Ok("dc")
    } else {
        Ok("c")
    };
    let verb = match verb_res {
        Ok(v) => v,
        Err(e) => return (String::new(), Some(e)),
    };
    let (out, err) = run_plain(
        "cliclick",
        &[format!("{verb}:{x},{y}")],
        "",
        Duration::from_secs(5),
    )
    .await;
    if let Some(e) = err {
        return (String::new(), Some(format!("{e}: {}", out.trim())));
    }
    (format!("{verb} at ({x},{y}) button {button}"), None)
}

/// Go macActiveWindow (platform.go ~218): focused app pid/name/title via
/// System Events.
pub async fn mac_active_window() -> String {
    let script = "tell application \"System Events\"\n\tset p to first application process whose frontmost is true\n\tset pn to name of p\n\tset pid to unix id of p\n\tset wt to \"\"\n\ttry\n\t\tset wt to name of front window of p\n\tend try\n\treturn (pid as text) & linefeed & wt & linefeed & pn\nend tell";
    let (out, err) = osascript(Duration::from_secs(5), script).await;
    if let Some(e) = err {
        return format!("get_active_window_error: {e}\n{}", out.trim());
    }
    let mut lines: Vec<String> = out
        .trim_end_matches('\n')
        .split('\n')
        .map(str::to_string)
        .collect();
    while lines.len() < 3 {
        lines.push(String::new());
    }
    format!(
        "id: {} (pid)\ntitle: {}\nclass: {}",
        lines.first().map(String::as_str).unwrap_or(""),
        lines.get(1).map(String::as_str).unwrap_or(""),
        lines.get(2).map(String::as_str).unwrap_or("")
    )
}

/// Go macListWindows (platform.go ~240): every non-background process's
/// windows as "<proc>\t<title>\n" lines.
pub async fn mac_list_windows() -> String {
    let script = "set out to \"\"\ntell application \"System Events\"\n\trepeat with p in (every application process whose background only is false)\n\t\tset pn to name of p\n\t\ttry\n\t\t\trepeat with w in (windows of p)\n\t\t\t\tset out to out & pn & tab & (name of w) & linefeed\n\t\t\tend repeat\n\t\tend try\n\tend repeat\nend tell\nreturn out";
    let (out, err) = osascript(Duration::from_secs(10), script).await;
    if let Some(e) = err {
        return format!("list_windows_error: {e}\n{}", out.trim());
    }
    if out.trim().is_empty() {
        return "(no windows found)".to_string();
    }
    out
}

/// Go macNotify (platform.go ~263).
pub async fn mac_notify(title: &str, body: &str) -> Option<String> {
    let script = format!(
        "display notification {} with title {}",
        as_quote(body),
        as_quote(title)
    );
    let (_, err) = osascript(Duration::from_secs(5), &script).await;
    err
}

// ---- system stats (platform.go ~292-445) --------------------------------------

/// Go vmStatPages: "Label: N." line → N, else 0.
fn vm_stat_pages(out: &str, label: &str) -> i64 {
    let wanted = format!("{label}:");
    for line in out.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix(&wanted) {
            let num: String = rest
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = num.parse::<i64>() {
                return n;
            }
            return 0;
        }
    }
    0
}

/// Go parseVMStat: used/available memory estimate from vm_stat output.
fn parse_vm_stat(out: &str) -> (i64, i64) {
    let mut page_size = 4096i64;
    for line in out.lines() {
        if let Some(rest) = line.trim().strip_prefix("page size of ") {
            let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = num.parse::<i64>()
                && n > 0
            {
                page_size = n;
            }
        }
    }
    let used = (vm_stat_pages(out, "Pages active")
        + vm_stat_pages(out, "Pages wired down")
        + vm_stat_pages(out, "Pages occupied by compressor"))
        * page_size;
    let avail = (vm_stat_pages(out, "Pages free")
        + vm_stat_pages(out, "Pages inactive")
        + vm_stat_pages(out, "Pages speculative"))
        * page_size;
    (used, avail)
}

/// Go parseBattery: (pct, state, remaining) from `pmset -g batt` output.
fn parse_battery(pmset: &str) -> Option<(String, String, String)> {
    let idx = pmset.find("InternalBattery")?;
    // Go regex: `(\d+)%;\s*([^;]+);(?:\s*(\d+:\d+) remaining)?` — find the
    // first "NN%; state;" after InternalBattery.
    let tail = &pmset[idx..];
    let mut pct_state_rem: Option<(String, String, String)> = None;
    let bytes = tail.as_bytes();
    let mut i = 0;
    while i < bytes.len() && pct_state_rem.is_none() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i + 1 < bytes.len() && bytes[i] == b'%' && bytes[i + 1] == b';' {
                let pct = tail[start..i].to_string();
                let rest = tail[i + 2..].trim_start();
                if let Some(semi) = rest.find(';') {
                    let state = rest[..semi].trim().to_string();
                    let after = &rest[semi + 1..];
                    let mut remaining = String::new();
                    if let Some(pos) = after.find(" remaining") {
                        // " 1:23 rema…" — walk back from pos to the time.
                        let before = &after[..pos];
                        let t: String = before
                            .chars()
                            .rev()
                            .take_while(|c| c.is_ascii_digit() || *c == ':')
                            .collect::<String>()
                            .chars()
                            .rev()
                            .collect();
                        remaining = t;
                    }
                    pct_state_rem = Some((format!("{pct}%"), state, remaining));
                }
            }
        }
        i += 1;
    }
    pct_state_rem
}

/// Go humanBytes: "%.1fB|Ki|Mi|Gi|Ti" chains.
pub fn human_bytes(n: i64) -> String {
    let mut f = n as f64;
    let units = ["B", "Ki", "Mi", "Gi", "Ti"];
    let mut i = 0;
    while f >= 1024.0 && i < units.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    format!("{f:.1}{}", units[i])
}

/// Go formatUptime (platform.go ~346).
fn format_uptime(mins_total: i64) -> String {
    let days = mins_total / 1440;
    let hours = (mins_total % 1440) / 60;
    let mins = mins_total % 60;
    if days > 0 {
        return format!("{days} hari, {hours} jam, {mins} menit");
    }
    if hours > 0 {
        return format!("{hours} jam, {mins} menit");
    }
    format!("{mins} menit")
}

/// Go darwinSystemStats (platform.go ~358): CPU/load/RAM/swap/disk/battery/
/// temperature snapshot built from sysctl/vm_stat/df/pmset.
pub async fn darwin_system_stats() -> String {
    let mut sb = String::new();
    macro_rules! sh {
        ($cmd:expr) => {{
            let (out, err) = run_raw_command($cmd, Duration::from_secs(8)).await;
            (out.trim().to_string(), err.is_none())
        }};
    }

    let (cores, ok) = sh!("sysctl -n hw.ncpu");
    if ok && !cores.is_empty() {
        sb.push_str(&format!("CPU: {cores} core"));
        let (brand, brand_ok) = sh!("sysctl -n machdep.cpu.brand_string");
        if brand_ok && !brand.is_empty() {
            sb.push_str(&format!(" ({brand})"));
        }
        sb.push('\n');
    }
    let (boottime, boot_ok) = sh!("sysctl -n kern.boottime");
    if boot_ok {
        // Go: `sec\s*=\s*(\d+)`.
        if let Some(pos) = boottime.find("sec") {
            let rest = &boottime[pos..];
            if let Some(eq) = rest.find('=') {
                let num: String = rest[eq + 1..]
                    .trim()
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(sec) = num.parse::<i64>() {
                    let boot = std::time::UNIX_EPOCH + std::time::Duration::from_secs(sec as u64);
                    let up = std::time::SystemTime::now()
                        .duration_since(boot)
                        .unwrap_or_default();
                    let mins = up.as_secs() / 60;
                    sb.push_str(&format!("Uptime: {}\n", format_uptime(mins as i64)));
                }
            }
        }
    }
    let (la, la_ok) = sh!("sysctl -n vm.loadavg");
    if la_ok {
        let f: Vec<&str> = la
            .trim_matches(|c: char| c == '{' || c == '}' || c == ' ')
            .split_whitespace()
            .collect();
        if f.len() >= 3 {
            sb.push_str(&format!(
                "Load average (1/5/15 menit): {} / {} / {}\n",
                f[0], f[1], f[2]
            ));
        }
    }
    sb.push('\n');

    let (mem_str, mem_ok) = sh!("sysctl -n hw.memsize");
    if mem_ok {
        let num: String = mem_str.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(total) = num.parse::<i64>() {
            let (vm, vm_ok) = sh!("vm_stat");
            if vm_ok {
                let (used, avail) = parse_vm_stat(&vm);
                sb.push_str(&format!(
                    "RAM: total {}, terpakai {}, tersedia {}\n",
                    human_bytes(total),
                    human_bytes(used),
                    human_bytes(avail)
                ));
            }
        }
    }
    let (sw, sw_ok) = sh!("sysctl -n vm.swapusage");
    if sw_ok {
        // Go: `total = (\S+)\s+used = (\S+)`.
        if let (Some(t), Some(u)) = (extract_kv(&sw, "total ="), extract_kv(&sw, "used = ")) {
            sb.push_str(&format!("Swap: total {t}, terpakai {u}\n"));
        }
    }
    sb.push('\n');

    // "/" is a sealed system volume on modern macOS; user data lives on
    // /System/Volumes/Data (mirror platform.go ~402).
    let disk_path = if std::path::Path::new("/System/Volumes/Data").is_file()
        || std::path::Path::new("/System/Volumes/Data").exists()
    {
        "/System/Volumes/Data"
    } else {
        "/"
    };
    let (df, df_ok) = sh!(&format!("df -h {disk_path}"));
    if df_ok {
        let last = df.lines().last().unwrap_or("");
        let fields: Vec<&str> = last.split_whitespace().collect();
        if fields.len() >= 5 {
            sb.push_str(&format!(
                "Disk ({}): total {}, terpakai {} ({}), tersisa {}\n",
                disk_path, fields[1], fields[2], fields[4], fields[3]
            ));
        }
    }
    sb.push('\n');

    let (pm, pm_ok) = sh!("pmset -g batt");
    if pm_ok {
        if let Some((pct, state, remaining)) = parse_battery(&pm) {
            sb.push_str(&format!("Baterai: {pct}, status {state}"));
            if !remaining.is_empty() && remaining != "0:00" {
                sb.push_str(&format!(", sisa waktu {remaining}"));
            }
            let (prof, prof_ok) = sh!("system_profiler SPPowerDataType");
            if prof_ok {
                if let Some(cap) = labeled_value(&prof, "Maximum Capacity:") {
                    sb.push_str(&format!(", kesehatan {cap} dari kapasitas awal"));
                }
                if let Some(cycles) = labeled_value(&prof, "Cycle Count:") {
                    sb.push_str(&format!(", {cycles} siklus"));
                }
            }
            sb.push('\n');
        } else {
            sb.push_str("Baterai: tidak terdeteksi (mungkin desktop / tanpa baterai)\n");
        }
    }

    if command_exists("osx-cpu-temp") {
        let (t, t_ok) = sh!("osx-cpu-temp");
        if t_ok && !t.is_empty() {
            sb.push_str(&format!("\nSuhu CPU: {t}\n"));
        }
    } else {
        sb.push_str(
            "\nSuhu: tidak tersedia (macOS butuh sudo/powermetrics atau tool tambahan seperti osx-cpu-temp)\n",
        );
    }
    format!("{}\n", sb.trim())
}

/// "key: value" extraction at line starts (Go's `(?m)^\s*key:\s+(.+)$`).
fn labeled_value(text: &str, key: &str) -> Option<String> {
    let key = key.trim_end_matches(':');
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix(key) {
            let rest = rest.strip_prefix(':')?;
            if t.starts_with(key) {
                let v = rest.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// vm.swapusage parser helper ("total = 4.09G  used = 1.38G").
fn extract_kv(text: &str, key: &str) -> Option<String> {
    let idx = text.find(key)?;
    let rest = text[idx + key.len()..].trim_start();
    let token: &str = rest.split_whitespace().next()?;
    Some(token.to_string())
}

fn command_exists(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--help")
        .output()
        .map(|o| o.status.code().is_some())
        .unwrap_or(false)
}
