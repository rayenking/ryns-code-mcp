//! Desktop / media tools mirroring Go's setupTools block for screenshot,
//! clipboard_get/set, mouse_click, keyboard_type, key_press,
//! get_active_window, list_windows, notify_send, ocr_screenshot,
//! camera_capture, record_video (main.go ~1887-2242). Every tool takes the
//! windowsUnsupported gate first, then splits darwin (AppleScript/cliclick/
//! screencapture) vs Linux-X11 (xdotool/xclip/import/ffmpeg). macOS camera
//! and record are deliberately unsupported in Go too (same Persian-language
//! error lines, mirrored).

use std::time::SystemTime;

use super::{Ctx, ToolOut};
#[cfg_attr(target_os = "macos", allow(unused_imports))]
use crate::platform::{
    capture_screen, clipboard_read, clipboard_write, run_plain, run_x_tool, windows_unsupported,
};
use crate::tools::safe_path;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScreenshotInput {
    #[serde(default, rename = "save")]
    pub save: Option<String>,
    #[serde(default, rename = "window")]
    pub window: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipboardSetInput {
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClickInput {
    pub x: i64,
    pub y: i64,
    #[serde(default)]
    pub button: i64,
    #[serde(default)]
    pub double: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TypeTextInput {
    pub text: String,
    #[serde(default, rename = "delay_ms")]
    // delay_ms is consumed by the Linux xdotool branch (`type --delay`).
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub delay_ms: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct KeyPressInput {
    pub keys: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NotifyInput {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    // urgency is forwarded only on the Linux notify-send branch.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    #[serde(default)]
    pub urgency: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OcrInput {
    #[serde(default, rename = "window")]
    pub window: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CameraInput {
    #[serde(default)]
    pub device: String,
    #[serde(default, rename = "save")]
    pub save: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecordInput {
    #[serde(default)]
    pub source: String,
    #[serde(default, rename = "duration_seconds")]
    pub duration_seconds: i64,
    #[serde(default)]
    pub device: String,
}

/// Unique temp-file prefix under the OS temp dir (Go os.CreateTemp shape:
/// prefix + random suffix + suffix). Returns the path as a String.
fn temp_file(prefix: &str, ext: &str) -> Result<String, String> {
    let d = std::env::temp_dir();
    let n = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos() as usize ^ (t.as_secs() as usize))
        .unwrap_or_else(|_| rand_usize());
    let pid = std::process::id() as usize;
    let mut attempt = 0;
    loop {
        let p = d.join(format!("{prefix}-{n}-{pid}-{attempt}.{ext}"));
        let s = crate::tools::path_abs(p.to_str().ok_or("temp path not UTF-8")?)?;
        if !std::path::Path::new(&s).exists() {
            // Go CreateTemp creates+opens the file; we only need exclusivity
            // for the path, the capture tools overwrite it anyway.
            return Ok(s);
        }
        attempt += 1;
        if attempt > 64 {
            return Err("temp file loop".to_string());
        }
    }
}

fn rand_usize() -> usize {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.as_nanos() as usize)
        .unwrap_or(0)
}

/// screenshot (main.go ~1887).
pub async fn screenshot(ctx: &Ctx, input: ScreenshotInput) -> ToolOut {
    if let Some(msg) = windows_unsupported("screenshot") {
        return ToolOut::Text(msg);
    }
    let tmp = match temp_file("ryns-screenshot", "png") {
        Ok(t) => t,
        Err(e) => return ToolOut::Text(format!("screenshot_error: {e}")),
    };
    let (out, err) = capture_screen(input.window.as_deref().unwrap_or(""), &tmp).await;
    if let Some(e) = err {
        return ToolOut::Text(format!("screenshot_error: {e}\n{out}"));
    }
    let data = match tokio::fs::read(&tmp).await {
        Ok(d) => d,
        Err(e) => return ToolOut::Text(format!("screenshot_error: {}", e)),
    };
    let _ = tokio::fs::remove_file(&tmp).await;

    let mut saved_note = String::new();
    if let Some(save) = input.save.filter(|s| !s.is_empty()) {
        let dest = match safe_path(ctx, &save) {
            Ok(d) => d,
            Err(e) => return ToolOut::Text(format!("screenshot_error: {e}")),
        };
        crate::tools::mkdir_parent(&dest).await;
        if let Err(e) = tokio::fs::write(&dest, &data).await {
            return ToolOut::Text(format!("screenshot_error: {e}"));
        }
        saved_note = format!(" (saved to {dest})");
    }
    let text = format!("screenshot captured, {} bytes{}", data.len(), saved_note);
    ToolOut::image(text, "image/png", data)
}

/// clipboard_get (main.go ~1931).
/// Parameter-less tools still go through the rmcp `Parameters<T>` plumbing:
/// an empty struct serializes from `{}` (Go's Go types with zero fields take
/// absent arguments the same way).
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct NoInput {}

pub async fn clipboard_get(_input: NoInput) -> ToolOut {
    if let Some(msg) = windows_unsupported("clipboard_get") {
        return ToolOut::Text(msg);
    }
    let (out, err) = clipboard_read().await;
    if err.is_some() {
        return ToolOut::Text("(clipboard is empty or unavailable)".to_string());
    }
    ToolOut::Text(out)
}

/// clipboard_set (main.go ~1942).
pub async fn clipboard_set(input: ClipboardSetInput) -> ToolOut {
    if let Some(msg) = windows_unsupported("clipboard_set") {
        return ToolOut::Text(msg);
    }
    let err = clipboard_write(&input.text).await;
    if let Some(e) = err {
        return ToolOut::Text(format!("clipboard_error: {e}"));
    }
    ToolOut::Text(format!("clipboard set ({} bytes)", input.text.len()))
}

/// mouse_click (main.go ~1953): darwin via cliclick, Linux via xdotool.
pub async fn mouse_click(input: ClickInput) -> ToolOut {
    if let Some(msg) = windows_unsupported("mouse_click") {
        return ToolOut::Text(msg);
    }
    let button = if input.button == 0 { 1 } else { input.button };
    #[cfg(target_os = "macos")]
    {
        let (res, err) =
            crate::platform::darwin::mac_mouse_click(input.x, input.y, button, input.double).await;
        if let Some(e) = err {
            return ToolOut::Text(format!("mouse_click_error: {e}"));
        }
        ToolOut::Text(res)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut args: Vec<String> = vec![
            "mousemove".into(),
            "--sync".into(),
            input.x.to_string(),
            input.y.to_string(),
            "click".into(),
        ];
        if input.double {
            args.extend([
                "--repeat".into(),
                "2".into(),
                "--delay".into(),
                "120".into(),
            ]);
        }
        args.push(button.to_string());
        let (out, err) = run_x_tool(Duration::from_secs(5), "xdotool", &args).await;
        if let Some(e) = err {
            return ToolOut::Text(format!("mouse_click_error: {e}\n{out}"));
        }
        let kind = if input.double {
            "double-click"
        } else {
            "click"
        };
        ToolOut::Text(format!(
            "{kind} at ({},{}) button {button}",
            input.x, input.y
        ))
    }
}

/// keyboard_type (main.go ~1984).
pub async fn keyboard_type(input: TypeTextInput) -> ToolOut {
    if let Some(msg) = windows_unsupported("keyboard_type") {
        return ToolOut::Text(msg);
    }
    #[cfg(target_os = "macos")]
    {
        let (out, err) = crate::platform::darwin::mac_type_text(&input.text).await;
        if let Some(e) = err {
            return ToolOut::Text(format!("keyboard_type_error: {e}\n{out}"));
        }
        ToolOut::Text(format!("typed {} character(s)", input.text.len()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Go: delay <= 0 → 12ms between keystrokes.
        let delay = if input.delay_ms <= 0 {
            12
        } else {
            input.delay_ms
        };
        let args = vec![
            "type".to_string(),
            "--delay".to_string(),
            delay.to_string(),
            "--".to_string(),
            input.text.clone(),
        ];
        let (out, err) = run_x_tool(Duration::from_secs(30), "xdotool", &args).await;
        if let Some(e) = err {
            return ToolOut::Text(format!("keyboard_type_error: {e}\n{out}"));
        }
        ToolOut::Text(format!("typed {} character(s)", input.text.len()))
    }
}

/// key_press (main.go ~2005).
pub async fn key_press(input: KeyPressInput) -> ToolOut {
    if let Some(msg) = windows_unsupported("key_press") {
        return ToolOut::Text(msg);
    }
    #[cfg(target_os = "macos")]
    {
        let (_, err) = crate::platform::darwin::mac_press_keys(&input.keys).await;
        if let Some(e) = err {
            return ToolOut::Text(format!("key_press_error: {e}"));
        }
        ToolOut::Text(format!("sent key(s): {}", input.keys))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let args = vec!["key".to_string(), "--".to_string(), input.keys.clone()];
        let (out, err) = run_x_tool(Duration::from_secs(5), "xdotool", &args).await;
        if let Some(e) = err {
            return ToolOut::Text(format!("key_press_error: {e}\n{out}"));
        }
        ToolOut::Text(format!("sent key(s): {}", input.keys))
    }
}

/// get_active_window (main.go ~2022).
pub async fn get_active_window(_input: NoInput) -> ToolOut {
    if let Some(msg) = windows_unsupported("get_active_window") {
        return ToolOut::Text(msg);
    }
    #[cfg(target_os = "macos")]
    {
        ToolOut::Text(crate::platform::darwin::mac_active_window().await)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let args = vec!["getactivewindow".to_string()];
        let (id_out, err) = run_x_tool(Duration::from_secs(5), "xdotool", &args).await;
        if let Some(e) = err {
            return ToolOut::Text(format!("get_active_window_error: {e}\n{id_out}"));
        }
        let id = id_out.trim().to_string();
        let (name_out, _) = run_x_tool(
            Duration::from_secs(5),
            "xdotool",
            &["getwindowname".to_string(), id.clone()],
        )
        .await;
        let (class_out, _) = run_x_tool(
            Duration::from_secs(5),
            "xdotool",
            &["getwindowclassname".to_string(), id.clone()],
        )
        .await;
        ToolOut::Text(format!(
            "id: {id}\ntitle: {}\nclass: {}",
            name_out.trim(),
            class_out.trim()
        ))
    }
}

/// list_windows (main.go ~2039).
pub async fn list_windows(_input: NoInput) -> ToolOut {
    if let Some(msg) = windows_unsupported("list_windows") {
        return ToolOut::Text(msg);
    }
    #[cfg(target_os = "macos")]
    {
        ToolOut::Text(crate::platform::darwin::mac_list_windows().await)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let args = vec![
            "search".to_string(),
            "--onlyvisible".to_string(),
            "--name".to_string(),
            ".*".to_string(),
        ];
        let (out, err) = run_x_tool(Duration::from_secs(5), "xdotool", &args).await;
        if let Some(e) = err {
            return ToolOut::Text(format!("list_windows_error: {e}\n{out}"));
        }
        let ids: Vec<&str> = out.split_whitespace().collect();
        let mut sb = String::new();
        for id in ids {
            let (name_out, e) = run_x_tool(
                Duration::from_secs(3),
                "xdotool",
                &["getwindowname".to_string(), id.to_string()],
            )
            .await;
            if e.is_some() {
                continue;
            }
            sb.push_str(&format!("{id}\t{}\n", name_out.trim()));
        }
        if sb.is_empty() {
            return ToolOut::Text("(no windows found)".to_string());
        }
        ToolOut::Text(sb)
    }
}

/// notify_send (main.go ~2065).
pub async fn notify_send(input: NotifyInput) -> ToolOut {
    if let Some(msg) = windows_unsupported("notify_send") {
        return ToolOut::Text(msg);
    }
    #[cfg(target_os = "macos")]
    {
        let err = crate::platform::darwin::mac_notify(&input.title, &input.body).await;
        if let Some(e) = err {
            return ToolOut::Text(format!("notify_error: {e}"));
        }
        ToolOut::Text("notification sent".to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut urgency = input.urgency;
        if urgency.is_empty() {
            urgency = "normal".to_string();
        }
        let mut args: Vec<String> = vec!["-u".into(), urgency, input.title.clone()];
        if !input.body.is_empty() {
            args.push(input.body);
        }
        let (out, err) = run_x_tool(Duration::from_secs(5), "notify-send", &args).await;
        if let Some(e) = err {
            return ToolOut::Text(format!("notify_error: {e}\n{out}"));
        }
        ToolOut::Text("notification sent".to_string())
    }
}

/// ocr_screenshot (main.go ~2090): screencapture/import + tesseract.
pub async fn ocr_screenshot(input: OcrInput) -> ToolOut {
    if let Some(msg) = windows_unsupported("ocr_screenshot") {
        return ToolOut::Text(msg);
    }
    let img = match temp_file("ryns-ocr", "png") {
        Ok(t) => t,
        Err(e) => return ToolOut::Text(format!("ocr_error: {e}")),
    };
    let (cap_out, cap_err) = capture_screen(input.window.as_deref().unwrap_or(""), &img).await;
    if let Some(e) = cap_err {
        return ToolOut::Text(format!("ocr_error: screenshot failed: {e}\n{cap_out}"));
    }
    // Go: `tesseract img outBase -l eng` writes <outBase>.txt (outBase is the
    // img path minus .png).
    let out_base = img.trim_end_matches(".png").to_string();
    let args = vec![
        img.clone(),
        out_base.clone(),
        "-l".to_string(),
        "eng".to_string(),
    ];
    let (out, err) = run_x_tool(Duration::from_secs(20), "tesseract", &args).await;
    let _ = tokio::fs::remove_file(&img).await;
    if let Some(e) = err {
        let _ = tokio::fs::remove_file(&format!("{out_base}.txt")).await;
        return ToolOut::Text(format!("ocr_error: {e}\n{out}"));
    }
    let text_bytes = match tokio::fs::read(&format!("{out_base}.txt")).await {
        Ok(b) => b,
        Err(e) => {
            let _ = tokio::fs::remove_file(&format!("{out_base}.txt")).await;
            return ToolOut::Text(format!("ocr_error: {e}"));
        }
    };
    let _ = tokio::fs::remove_file(&format!("{out_base}.txt")).await;
    let text = String::from_utf8_lossy(&text_bytes).trim().to_string();
    if text.is_empty() {
        return ToolOut::Text("(no text detected)".to_string());
    }
    ToolOut::Text(text)
}

/// camera_capture (main.go ~2124): macOS is unsupported in Go too; Linux
/// grabs one JPEG via ffmpeg v4l2.
pub async fn camera_capture(ctx: &Ctx, input: CameraInput) -> ToolOut {
    if let Some(msg) = windows_unsupported("camera_capture") {
        return ToolOut::Text(msg);
    }
    if cfg!(target_os = "macos") {
        return ToolOut::Text("camera_error: belum didukung di macOS (butuh ffmpeg avfoundation); saat ini khusus Linux".to_string());
    }
    let device = if input.device.is_empty() {
        "/dev/video0".to_string()
    } else {
        input.device.clone()
    };
    let tmp = match temp_file("ryns-camera", "jpg") {
        Ok(t) => t,
        Err(e) => return ToolOut::Text(format!("camera_error: {e}")),
    };
    let args = vec![
        "-y".to_string(),
        "-f".into(),
        "v4l2".into(),
        "-input_format".into(),
        "mjpeg".into(),
        "-video_size".into(),
        "1280x720".into(),
        "-i".into(),
        device.clone(),
        "-frames:v".into(),
        "1".into(),
        "-update".into(),
        "1".into(),
        tmp.clone(),
    ];
    // Go runs the camera ffmpeg with childEnv (no DISPLAY injection) —
    // run_plain, not run_x_tool.
    let (out, err) = run_plain("ffmpeg", &args, "", Duration::from_secs(15)).await;
    if let Some(e) = err {
        let _ = tokio::fs::remove_file(&tmp).await;
        return ToolOut::Text(format!("camera_error: {e}\n{out}"));
    }
    let data = match tokio::fs::read(&tmp).await {
        Ok(d) => d,
        Err(e) => return ToolOut::Text(format!("camera_error: {e}")),
    };
    let _ = tokio::fs::remove_file(&tmp).await;
    if data.is_empty() {
        return ToolOut::Text(
            "camera_error: capture produced an empty file — camera may be busy or unavailable"
                .to_string(),
        );
    }
    let mut saved_note = String::new();
    if let Some(save) = input.save.filter(|s| !s.is_empty()) {
        let dest = match safe_path(ctx, &save) {
            Ok(d) => d,
            Err(e) => return ToolOut::Text(format!("camera_error: {e}")),
        };
        crate::tools::mkdir_parent(&dest).await;
        if let Err(e) = tokio::fs::write(&dest, &data).await {
            return ToolOut::Text(format!("camera_error: {e}"));
        }
        saved_note = format!(" (saved to {dest})");
    }
    let text = format!(
        "photo captured from {device}, {} bytes{}",
        data.len(),
        saved_note
    );
    ToolOut::image(text, "image/jpeg", data)
}

/// record_video (main.go ~2182): ffmpeg screen/camera recording saved under
/// assets/, returns the public URL (no video bytes in the tool result).
pub async fn record_video(ctx: &Ctx, input: RecordInput) -> ToolOut {
    if let Some(msg) = windows_unsupported("record_video") {
        return ToolOut::Text(msg);
    }
    if cfg!(target_os = "macos") {
        return ToolOut::Text("record_error: belum didukung di macOS (butuh ffmpeg avfoundation); saat ini khusus Linux".to_string());
    }
    let mut source = input.source.clone();
    if source.is_empty() {
        source = "screen".to_string();
    }
    if source != "screen" && source != "camera" {
        return ToolOut::Text("record_error: source must be 'screen' or 'camera'".to_string());
    }
    let secs = input.duration_seconds;
    if secs <= 0 {
        return ToolOut::Text("record_error: duration_seconds must be greater than 0".to_string());
    }
    if secs > 120 {
        return ToolOut::Text("record_error: duration_seconds capped at 120".to_string());
    }
    let ws = ctx.workspace.to_string();
    let assets_dir = format!("{ws}/assets");
    if let Err(e) = tokio::fs::create_dir_all(&assets_dir).await {
        return ToolOut::Text(format!("record_error: {e}"));
    }
    let stamp = super::ts_filename();
    let filename = format!("record-{source}-{stamp}.mp4");
    let out_path = format!("{assets_dir}/{filename}");
    let args: Vec<String>;
    if source == "screen" {
        let (geom_out, e) = run_x_tool(
            Duration::from_secs(5),
            "xdotool",
            &["getdisplaygeometry".to_string()],
        )
        .await;
        let size = if e.is_none() {
            geom_out.trim().replace(' ', "x")
        } else {
            "1920x1080".to_string()
        };
        args = vec![
            "-y".into(),
            "-f".into(),
            "x11grab".into(),
            "-video_size".into(),
            size,
            "-framerate".into(),
            "30".into(),
            "-i".into(),
            ":0.0".into(),
            "-t".into(),
            secs.to_string(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            "-vcodec".into(),
            "libx264".into(),
            "-preset".into(),
            "veryfast".into(),
            out_path.clone(),
        ];
    } else {
        let device = if input.device.is_empty() {
            "/dev/video0".to_string()
        } else {
            input.device.clone()
        };
        args = vec![
            "-y".into(),
            "-f".into(),
            "v4l2".into(),
            "-input_format".into(),
            "mjpeg".into(),
            "-video_size".into(),
            "1280x720".into(),
            "-framerate".into(),
            "30".into(),
            "-i".into(),
            device,
            "-t".into(),
            secs.to_string(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            "-vcodec".into(),
            "libx264".into(),
            "-preset".into(),
            "veryfast".into(),
            out_path.clone(),
        ];
    }
    // Go: timeout secs+15s; screen capture runs with xEnv (cmd.Env = xEnv()).
    let deadline = Duration::from_secs(secs as u64 + 15);
    let (out, err) = run_x_tool(deadline, "ffmpeg", &args).await;
    if let Some(e) = err {
        return ToolOut::Text(format!("record_error: {e}\n{out}"));
    }
    let info = match tokio::fs::metadata(&out_path).await {
        Ok(i) => i,
        Err(_) => {
            return ToolOut::Text(
                "record_error: recording finished but the output file is missing".to_string(),
            );
        }
    };
    let url = format!("{}/assets/{}", super::public_base_url(), filename);
    ToolOut::Text(format!(
        "recorded {secs} second(s) from {source}: {} bytes\n{url}",
        info.len()
    ))
}
