//! workspace_info, check_gold_price, lint_format and system_stats
//! (main.go ~2249, ~2506-2551, ~2597-2790). system_stats splits darwin
//! (platform/darwin.rs) vs Linux shell parsing, exactly like Go; Windows gets
//! the Go windowsUnsupported line.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use super::{Ctx, default_string, safe_path};
// run_raw_command is consumed by linux_system_stats only; cfg'd out here but
// required by the Linux branch below.
#[cfg_attr(not(target_os = "linux"), allow(unused_imports))]
use crate::platform::{run_raw_command, windows_unsupported};
use std::time::Duration;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LintFormatInput {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub fix: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GoldPriceApiItem {
    #[serde(default)]
    weight: f64,
    #[serde(default)]
    #[allow(dead_code)]
    weight_unit: String,
    #[serde(default, rename = "sellPrice")]
    sell_price: i64,
    #[serde(default, rename = "recordedDate")]
    recorded_date: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GoldPriceApiResponse {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Vec<GoldPriceApiItem>,
}

/// workspace_info (main.go ~2249). Divergence kept honest: Go reports its own
/// runtime ("node": "not used (Go)", "go": runtime.Version()); the Rust port
/// reports the same keys with Rust-appropriate values.
pub async fn workspace_info(ctx: &Ctx) -> String {
    let ids: Vec<String> = super::processes::active_ids();
    super::must_json(&json!({
        "workspace": ctx.workspace,
        "hostname": super::hostname(),
        "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "node": "not used (Rust)",
        "go": "n/a (Rust port)",
        "cpu_count": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "active_processes": ids,
    }))
}

/// check_gold_price (main.go ~2506): today's Antam 24K/22K estimate.
/// Divergence: Go uses net/http; here the request goes through the system
/// `curl` binary (no TLS stack in the dependency lock), with Go's
/// status/parse error wording preserved.
pub async fn check_gold_price(input: crate::tools::misc::EmptyInput) -> String {
    let _ = input;
    let body = match fetch_url(
        "https://logam-mulia-api.iamutaki.workers.dev/api/prices/logammulia",
        1_048_576,
    )
    .await
    {
        Ok(b) => b,
        Err(FetchError::Connect(e)) => {
            return format!("gold_price_error: gagal menghubungi API harga emas: {e}");
        }
        Err(FetchError::Status(code)) => {
            return format!("gold_price_error: API harga emas balas status {code}");
        }
        Err(FetchError::TooLarge) => {
            return "gold_price_error: API harga emas balas status 413".to_string();
        }
        Err(FetchError::Read(e)) => return format!("gold_price_error: {e}"),
    };
    let parsed: GoldPriceApiResponse = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => return format!("gold_price_error: gagal parse response API harga emas: {e}"),
    };
    if !parsed.success {
        return "gold_price_error: API harga emas mengembalikan success=false".to_string();
    }
    let Some(gram1) = parsed.data.iter().find(|d| d.weight == 1.0) else {
        return "gold_price_error: data harga emas untuk pecahan 1 gram tidak ditemukan di response API".to_string();
    };
    let price24k = gram1.sell_price;
    let price22k = (price24k as f64 * 22.0 / 24.0) as i64;
    let date_label = format_indonesian_date(&gram1.recorded_date);
    format!(
        "Harga Emas Hari Ini ({date_label})\nSumber: Logam Mulia (Antam resmi) via logammulia.com\n\n24K (Logam Mulia, kadar 99,99%): Rp {} / gram\n22K (estimasi kadar 91,7%):       Rp {} / gram\n\nCatatan: harga 22K adalah estimasi rasio kadar dari harga 24K resmi, bukan harga jual resmi Antam (Antam hanya menjual produk 24K).",
        format_rupiah_thousands(price24k),
        format_rupiah_thousands(price22k)
    )
}

/// GET via system curl: body with -sS --max-filesize, status code captured to
/// separate non-200 from transport failures (Go resp.StatusCode semantics).
async fn fetch_url(url: &str, max_bytes: usize) -> Result<String, FetchError> {
    let tmp_manifest = std::env::temp_dir().join(format!("ryns-gold-{}.body", std::process::id()));
    let status_file = std::env::temp_dir().join(format!("ryns-gold-{}.status", std::process::id()));
    let out = tokio::task::spawn_blocking({
        let url = url.to_string();
        let tmp = tmp_manifest.clone();
        move || {
            std::process::Command::new("curl")
                .args([
                    "-sS",
                    "-w",
                    "%{http_code}",
                    "--max-filesize",
                    &max_bytes.to_string(),
                    "-o",
                    tmp.to_str().unwrap_or_default(),
                    &url,
                ])
                .output()
        }
    })
    .await
    .map_err(|e| FetchError::Connect(e.to_string()))?;
    let _ = std::fs::remove_file(&status_file);
    match out {
        Err(e) => Err(FetchError::Connect(e.to_string())),
        Ok(o) => {
            let dashes = String::from_utf8_lossy(&o.stderr).to_string();
            let stdout = String::from_utf8_lossy(&o.stdout).to_string();
            let code_str = stdout.trim().to_string();
            if !o.status.success() && code_str.is_empty() {
                // curl failed at transport level (mirror Go's %w-wrapped error).
                return Err(FetchError::Connect(dashes.trim().to_string()));
            }
            if o.status.success() && code_str == "000" {
                return Err(FetchError::Connect(dashes.trim().to_string()));
            }
            if code_str == "000" {
                // max-filesize overflow leaves 000.
                return Err(FetchError::Connect(dashes.trim().to_string()));
            }
            let code: u16 = code_str.parse().unwrap_or(0);
            if code != 200 {
                return Err(FetchError::Status(code));
            }
            std::fs::read_to_string(&tmp_manifest).map_err(|e| FetchError::Read(e.to_string()))
        }
    }
}

// TooLarge is the "--max-filesize" trip of the curl path; kept as an
// explicit variant so the size divergence from Go's LimitReader stays
// labeled (curl exits before downloading past the cap on some builds).
#[allow(dead_code)]
enum FetchError {
    Connect(String),
    Status(u16),
    TooLarge,
    Read(String),
}

/// formatIndonesianDate (main.go ~2823): "2026-08-30" → "30 Agustus 2026";
/// passthrough when the input doesn't parse.
pub fn format_indonesian_date(s: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Januari",
        "Februari",
        "Maret",
        "April",
        "Mei",
        "Juni",
        "Juli",
        "Agustus",
        "September",
        "Oktober",
        "November",
        "Desember",
    ];
    match super::parse_ymd(s) {
        Some((y, m, d)) => format!("{d} {} {y}", MONTHS[(m - 1) as usize]),
        None => s.to_string(),
    }
}

/// formatRupiahThousands (main.go ~2834): dot-grouped thousands, negative
/// sign preserved.
pub fn format_rupiah_thousands(n: i64) -> String {
    let neg = n < 0;
    let s = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in s.bytes().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(c as char);
    }
    if neg { format!("-{out}") } else { out }
}

// ---- lint_format ---------------------------------------------------------------

const LINT_IGNORE_DIRS: [&str; 8] = [
    ".git",
    "node_modules",
    "vendor",
    "target",
    "dist",
    "build",
    ".next",
    "__pycache__",
];

/// Go classifyLintFile: extension → language bucket, "" when unsupported.
fn classify_lint_file(path: &str) -> &'static str {
    let ext = std::path::Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "go" => "go",
        "rs" => "rust",
        "js" | "jsx" | "ts" | "tsx" | "json" | "css" | "scss" | "html" | "md" | "yaml" | "yml" => {
            "js"
        }
        "py" => "py",
        _ => "",
    }
}

/// Go collectLintFiles: single file plan B, else recursive walk skipping
/// ignore-listed and dot dirs; files grouped by language.
async fn collect_lint_files(root: &str) -> Result<Vec<(String, Vec<String>)>, String> {
    let info = tokio::fs::metadata(root)
        .await
        .map_err(|e| format!("{}: {e}", go_open(root, &e)))?;
    let mut groups: std::collections::BTreeMap<&'static str, Vec<String>> = Default::default();
    if !info.is_dir() {
        let cat = classify_lint_file(root);
        if !cat.is_empty() {
            groups.entry(cat).or_default().push(root.to_string());
        }
        let mut out: Vec<(String, Vec<String>)> = groups
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        out.sort_by(|a, b| order_of(a.0.as_str()).cmp(&order_of(b.0.as_str())));
        return Ok(out);
    }
    walk_for_lint(root, &mut groups).await?;
    let mut out: Vec<(String, Vec<String>)> = groups
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    out.sort_by(|a, b| order_of(a.0.as_str()).cmp(&order_of(b.0.as_str())));
    Ok(out)
}

/// Go iterates a map with unspecified order; the output schema below pins the
/// order go → rust → js → py exactly like most Go runs (single map insertion
/// order is not guaranteed — this fixed order is deterministic and
/// documented).
fn order_of(group: &str) -> u8 {
    match group {
        "go" => 0,
        "rust" => 1,
        "js" => 2,
        "py" => 3,
        _ => 4,
    }
}

fn go_open(path: &str, e: &std::io::Error) -> String {
    super::go_io_error("lstat", path, e)
}

async fn walk_for_lint(
    dir: &str,
    groups: &mut std::collections::BTreeMap<&'static str, Vec<String>>,
) -> Result<(), String> {
    walk(dir, groups).await
}

// Boxed for async recursion (mirrors Go's filepath.WalkDir recursion).
async fn walk(
    dir: &str,
    groups: &mut std::collections::BTreeMap<&'static str, Vec<String>>,
) -> Result<(), String> {
    let mut rd = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| super::go_io_error("open", dir, &e))?;
    let mut kids: Vec<(String, bool)> = Vec::new();
    loop {
        match rd.next_entry().await {
            Ok(Some(e)) => {
                let name = e.file_name().to_string_lossy().into_owned();
                let is_dir = e.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                kids.push((name, is_dir));
            }
            Ok(None) => break,
            Err(e) => return Err(super::go_io_error("readdirent", dir, &e)),
        }
    }
    kids.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, is_dir) in kids {
        let p = super::path_join(dir, &name);
        if is_dir {
            if LINT_IGNORE_DIRS.contains(&name.as_str()) || name.starts_with('.') {
                continue; // filepath.SkipDir
            }
            Box::pin(walk(&p, groups)).await?;
        } else if let Some(cat) = classify_eff(&p) {
            groups.entry(cat).or_default().push(p);
        }
    }
    Ok(())
}

/// classify via full path (Go passes the path; extension stripped below).
fn classify_eff(p: &str) -> Option<&'static str> {
    let cat = classify_lint_file(p);
    if cat.is_empty() { None } else { Some(cat) }
}

/// Go collectLintFiles + runLintTool logic (main.go ~2597): report or fix per
/// language group, with the exact Indonesian report lines.
pub async fn lint_format(ctx: &Ctx, input: LintFormatInput) -> String {
    let root = match safe_path(ctx, &default_string(Some(input.path.as_str()), ".")) {
        Ok(r) => r,
        Err(e) => return format!("lint_format failed: {e}"),
    };
    let groups = match collect_lint_files(&root).await {
        Ok(g) => g,
        Err(e) => return format!("lint_format failed: {e}"),
    };
    if groups.is_empty() {
        return format!(
            "Tidak ada file yang didukung lint_format (go/rust/js-ts-json-css-md/py) di {root}."
        );
    }
    let mut sb = String::new();
    if input.fix {
        sb.push_str("Mode: FIX (perubahan langsung diterapkan)\n\n");
    } else {
        sb.push_str("Mode: CHECK (cuma laporan, belum ada file yang diubah)\n\n");
    }

    for (group, files) in &groups {
        match group.as_str() {
            "go" => {
                sb.push_str("== Go (gofmt) ==\n");
                let mut args: Vec<String> = vec!["-l".into()];
                args.extend(files.iter().cloned());
                let (check_out, check_err) = run_lint_tool("gofmt", &args, &root).await;
                let needs_format = !check_out.trim().is_empty();
                if let Some(e) = check_err {
                    sb.push_str(&format!("error saat cek format:\n{check_out} (…{e})\n"));
                } else if input.fix {
                    let mut apply: Vec<String> = vec!["-w".into()];
                    apply.extend(files.iter().cloned());
                    let (_, apply_err) = run_lint_tool("gofmt", &apply, &root).await;
                    if apply_err.is_some() {
                        sb.push_str(&format!("error saat apply format:\n{check_out}\n"));
                    } else if needs_format {
                        sb.push_str(&format!("File yang diformat ulang:\n{check_out}"));
                    } else {
                        sb.push_str("Semua file Go udah rapi, tidak ada perubahan.\n");
                    }
                } else if needs_format {
                    sb.push_str(&format!(
                        "File yang butuh diformat (jalankan dengan fix=true untuk beresin):\n{check_out}"
                    ));
                } else {
                    sb.push_str("Semua file Go udah sesuai format gofmt.\n");
                }
                sb.push('\n');
            }
            "rust" => {
                sb.push_str("== Rust (rustfmt) ==\n");
                let mut args: Vec<String> = vec!["--check".into()];
                args.extend(files.iter().cloned());
                let (check_out, check_err) = run_lint_tool("rustfmt", &args, &root).await;
                let needs_format = check_err.is_some();
                if input.fix {
                    let (_, apply_err) = run_lint_tool("rustfmt", files, &root).await;
                    if apply_err.is_some() {
                        sb.push_str(&format!("error saat apply format:\n{check_out}\n"));
                    } else if needs_format {
                        sb.push_str(&format!("File yang diformat ulang:\n{check_out}"));
                    } else {
                        sb.push_str("Semua file Rust udah rapi, tidak ada perubahan.\n");
                    }
                } else if needs_format {
                    sb.push_str(&format!(
                        "File yang butuh diformat (jalankan dengan fix=true untuk beresin):\n{check_out}"
                    ));
                } else {
                    sb.push_str("Semua file Rust udah sesuai format rustfmt.\n");
                }
                sb.push('\n');
            }
            "js" => {
                sb.push_str("== JS/TS/JSON/CSS/MD (prettier) ==\n");
                let mut args: Vec<String> =
                    vec!["--no-install".into(), "prettier".into(), "--check".into()];
                args.extend(files.iter().cloned());
                let (check_out, check_err) = run_lint_tool("npx", &args, &root).await;
                let lower = check_out.to_lowercase();
                let not_installed = check_err.is_some()
                    && (lower.contains("could not determine executable to run")
                        || lower.contains("not found")
                        || lower.contains("command not found"));
                let needs_format = check_err.is_some() && !not_installed;
                if not_installed {
                    sb.push_str("Prettier belum terinstall di project ini (npx --no-install gagal). Install dulu: npm install --save-dev prettier\n");
                } else if input.fix {
                    let mut apply: Vec<String> =
                        vec!["--no-install".into(), "prettier".into(), "--write".into()];
                    apply.extend(files.iter().cloned());
                    let (_, apply_err) = run_lint_tool("npx", &apply, &root).await;
                    if apply_err.is_some() {
                        sb.push_str(&format!("error saat apply format:\n{check_out}\n"));
                    } else if needs_format {
                        sb.push_str(&format!("File yang diformat ulang:\n{check_out}"));
                    } else {
                        sb.push_str("Semua file udah rapi, tidak ada perubahan.\n");
                    }
                } else if needs_format {
                    sb.push_str(&format!(
                        "File yang butuh diformat (jalankan dengan fix=true untuk beresin):\n{check_out}"
                    ));
                } else {
                    sb.push_str("Semua file udah sesuai format prettier.\n");
                }
                sb.push('\n');
            }
            "py" => {
                sb.push_str("== Python (syntax check, formatter belum ada) ==\n");
                let mut problems: Vec<String> = Vec::new();
                for f in files {
                    let args = vec!["-m".into(), "py_compile".into(), f.clone()];
                    let (out, err) = run_lint_tool("python3", &args, &root).await;
                    if err.is_some() {
                        problems.push(format!("{f}:\n{}", out.trim()));
                    }
                }
                if problems.is_empty() {
                    sb.push_str(&format!(
                        "Semua {} file Python valid secara sintaks.\n",
                        files.len()
                    ));
                } else {
                    sb.push_str(&format!(
                        "{} file bermasalah:\n{}\n",
                        problems.len(),
                        problems.join("\n")
                    ));
                }
                sb.push_str("Catatan: belum ada black/ruff terinstall, jadi cuma cek sintaks, bukan auto-format.\n\n");
            }
            _ => {}
        }
    }

    format!("{}\n", sb.trim())
}

/// Go runLintTool: 30s timeout, CombinedOutput, childEnv, workspace dir.
async fn run_lint_tool(name: &str, args: &[String], dir: &str) -> (String, Option<String>) {
    #[cfg(windows)]
    {
        let _ = name;
        let _ = args;
        let _ = dir;
        (String::new(), Some("not supported on windows".into()))
    }
    #[cfg(unix)]
    {
        let mut cmd = tokio::process::Command::new(name);
        cmd.args(args)
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd.env_clear();
        for (k, v) in crate::auth::child_env() {
            cmd.env(k, v);
        }
        let run = async {
            let out = match cmd.output().await {
                Ok(o) => o,
                Err(e) => return Err((String::new(), e.to_string())),
            };
            let mut combined = out.stdout;
            combined.extend_from_slice(&out.stderr);
            let text = String::from_utf8_lossy(&combined).into_owned();
            if !out.status.success() {
                return Err((
                    text,
                    format!("exit status {}", out.status.code().unwrap_or_default()),
                ));
            }
            Ok((text, None))
        };
        match tokio::time::timeout(Duration::from_secs(30), run).await {
            Ok(Ok((t, None))) => (t, None),
            Ok(Ok((t, Some(e)))) => (t, Some(e)),
            Ok(Err((t, e))) => (t, Some(e)),
            Err(_) => (String::new(), Some("context deadline exceeded".to_string())),
        }
    }
}

// ---- system_stats ---------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EmptyInput {}

/// system_stats (main.go ~2706).
pub async fn system_stats(ctx: &Ctx, input: EmptyInput) -> String {
    let _ = (ctx, input);
    if let Some(msg) = windows_unsupported("system_stats") {
        return msg;
    }
    #[cfg(target_os = "macos")]
    {
        crate::platform::darwin::darwin_system_stats().await
    }
    #[cfg(not(target_os = "macos"))]
    {
        linux_system_stats().await
    }
}

/// Go's Linux branch of system_stats (main.go ~2713-2790).
#[cfg(not(target_os = "macos"))]
async fn linux_system_stats() -> String {
    let mut sb = String::new();
    if let (out, None) = run_raw_command("nproc", Duration::from_secs(5)).await {
        sb.push_str(&format!("CPU: {out} core\n"));
    }
    if let (out, None) = run_raw_command("uptime -p", Duration::from_secs(5)).await {
        sb.push_str(&format!(
            "Uptime: {}\n",
            out.strip_prefix("up ").unwrap_or(&out)
        ));
    }
    if let (out, None) = run_raw_command("cat /proc/loadavg", Duration::from_secs(5)).await {
        let f: Vec<&str> = out.split_whitespace().collect();
        if f.len() >= 3 {
            sb.push_str(&format!(
                "Load average (1/5/15 menit): {} / {} / {}\n",
                f[0], f[1], f[2]
            ));
        }
    }
    sb.push('\n');

    if let (out, None) = run_raw_command("free -h", Duration::from_secs(5)).await {
        for line in out.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 7 && fields[0].starts_with("Mem") {
                sb.push_str(&format!(
                    "RAM: total {}, terpakai {}, tersedia {}\n",
                    fields[1], fields[2], fields[6]
                ));
            } else if fields.len() >= 3 && fields[0].starts_with("Swap") {
                sb.push_str(&format!(
                    "Swap: total {}, terpakai {}\n",
                    fields[1], fields[2]
                ));
            }
        }
    }
    sb.push('\n');

    if let (out, None) = run_raw_command("df -h /", Duration::from_secs(5)).await {
        if let Some(last) = out.lines().last() {
            let fields: Vec<&str> = last.split_whitespace().collect();
            if fields.len() >= 5 {
                sb.push_str(&format!(
                    "Disk (/): total {}, terpakai {} ({}), tersisa {}\n",
                    fields[1], fields[2], fields[4], fields[3]
                ));
            }
        }
    }
    sb.push('\n');

    let batt = run_raw_command(
        "upower -i $(upower -e | grep -i BAT) 2>/dev/null",
        Duration::from_secs(5),
    )
    .await;
    match batt {
        (batt_out, None) if !batt_out.trim().is_empty() => {
            let vendor = go_line_field(&batt_out, "vendor");
            let model = go_line_field(&batt_out, "model");
            let pct = go_line_field(&batt_out, "percentage");
            let state = go_line_field(&batt_out, "state");
            let capacity = go_line_field(&batt_out, "capacity");
            sb.push_str(&format!("Baterai: {vendor} {model}, {pct}, status {state}"));
            if !capacity.is_empty() {
                sb.push_str(&format!(", kesehatan {capacity} dari kapasitas awal"));
            }
            sb.push('\n');
        }
        _ => {
            sb.push_str("Baterai: tidak terdeteksi (mungkin desktop / tanpa baterai)\n");
        }
    }

    if let (out, None) = run_raw_command("sensors 2>/dev/null", Duration::from_secs(5)).await {
        let temps: Vec<&str> = out
            .lines()
            .filter(|l| l.contains("°C"))
            .map(str::trim)
            .collect();
        if !temps.is_empty() {
            sb.push_str("\nSuhu:\n");
            for t in temps {
                sb.push_str(&format!("  {t}\n"));
            }
        }
    }

    format!("{}\n", sb.trim())
}

/// Go's `(?m)^\s*key:\s+(.+)$` matcher as a plain scan.
#[cfg(not(target_os = "macos"))]
fn go_line_field(text: &str, key: &str) -> String {
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix(key) {
            if let Some(rest) = rest.strip_prefix(':') {
                let v = rest.trim();
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod date_price_tests {
    use super::*;

    #[test]
    fn indonesian_date_month_table() {
        assert_eq!(format_indonesian_date("2026-01-02"), "2 Januari 2026");
        assert_eq!(format_indonesian_date("2026-08-30"), "30 Agustus 2026");
        assert_eq!(format_indonesian_date("2026-12-31"), "31 Desember 2026");
        // Passthrough exactly like Go when it doesn't parse.
        assert_eq!(format_indonesian_date("bukan-tanggal"), "bukan-tanggal");
        assert_eq!(format_indonesian_date("2026-13-01"), "2026-13-01");
    }

    #[test]
    fn rupiah_thousands_grouping() {
        assert_eq!(format_rupiah_thousands(0), "0");
        assert_eq!(format_rupiah_thousands(7), "7");
        assert_eq!(format_rupiah_thousands(1234), "1.234");
        assert_eq!(format_rupiah_thousands(1234567890), "1.234.567.890");
        assert_eq!(format_rupiah_thousands(-9876), "-9.876");
    }
}
