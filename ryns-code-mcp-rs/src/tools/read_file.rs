//! `read_file` — mirrors Go main.go (~1518 handler). cat -n style numbered
//! output with the same caps and truncation notices as Go; image
//! extensions (png/jpg/jpeg/gif/webp/bmp) return inline image content via
//! `ToolOut::TextPlusImage`, exactly like Go.

use schemars::JsonSchema;
use serde::Deserialize;

use super::{Ctx, MAX_READ_LINES, ToolOut, go_io_error, number_lines, safe_path};
use crate::safety::GateResult;

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ReadInput {
    /// file path (absolute, or relative to workspace)
    pub path: String,
    #[serde(default)]
    pub start_line: Option<i64>,
    #[serde(default)]
    pub end_line: Option<i64>,
}

pub async fn run(ctx: &Ctx, input: ReadInput) -> ToolOut {
    let path = match safe_path(ctx, &input.path) {
        Ok(p) => p,
        Err(e) => return ToolOut::Text(format!("read_error: {e}")),
    };
    // Phase 3 gate: secret-path rules on read targets (all modes audit).
    if let GateResult::Blocked(text) = crate::safety::gate_paths(ctx, "read_file", &[&path]).await {
        return ToolOut::Text(text);
    }
    // Go's os.ReadFile fails either at Open (→ "open …") or at the read
    // loop (→ "read …"); mirror the two phases instead of one merged read.
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => return ToolOut::Text(format!("read_error: {}", go_io_error("open", &path, &e))),
    };
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    if let Err(e) = file.read_to_end(&mut bytes).await {
        return ToolOut::Text(format!("read_error: {}", go_io_error("read", &path, &e)));
    }

    // Image branch (main.go ~1523): the Go server returns inline image
    // content for known image extensions. Text keeps the byte-size notice.
    if let Some(mime) = image_mime(&path) {
        let text = format!("{} ({}, {} bytes)", path, mime, bytes.len());
        return ToolOut::image(text, mime, bytes);
    }

    // Mirrors Go: CRLF normalized BEFORE splitting into lines.
    let raw = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
    let mut lines: Vec<String> = raw.split('\n').map(str::to_string).collect();
    if raw.ends_with('\n') && lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }

    let total = lines.len() as i64;
    if total == 0 {
        return ToolOut::Text(format!("{path} (empty file)"));
    }

    let start = match input.start_line {
        Some(s) if s >= 1 => s,
        _ => 1,
    };
    if start > total {
        return ToolOut::Text(format!(
            "{path} ({total} lines total) — start_line {start} is past end of file"
        ));
    }
    let mut end = total;
    if let Some(e) = input.end_line.filter(|e| *e > 0 && *e < total) {
        end = e;
    }
    // Unscoped reads auto-cap at MAX_READ_LINES, exactly like Go.
    let mut auto_capped = false;
    if input.end_line.is_none_or(|e| e <= 0) && end - start + 1 > MAX_READ_LINES {
        end = start - 1 + MAX_READ_LINES;
        auto_capped = true;
    }

    let mut out = format!("{path} ({total} lines total)\n");
    out.push_str(&number_lines(&lines, start, end));
    if auto_capped {
        out.push_str(&format!(
            "...[showing lines {start}-{end} of {total}; pass start_line/end_line to read more]\n"
        ));
    }
    ToolOut::Text(out)
}

/// The image extensions Go's read_file returns inline, with their MIME types
/// (main.go imageExt). Comparison is case-insensitive like Go.
fn image_mime(path: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(path)
        .extension()?
        .to_string_lossy()
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> Ctx {
        Ctx::plain(std::env::temp_dir().to_string_lossy())
    }

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ryns-rs-read-file-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Test helper: textual branch of the ToolOut envelope.
    async fn run_text(ctx: &Ctx, input: ReadInput) -> String {
        match run(ctx, input).await {
            ToolOut::Text(text) => text,
            ToolOut::TextPlusImage { text, .. } => text,
        }
    }

    #[tokio::test]
    async fn image_extensions_return_inline_image() {
        let dir = tempdir("image");
        let f = dir.join("tiny.png");
        let data: &[u8] = b"\x89PNG fake bytes";
        std::fs::write(&f, data).unwrap();
        let out = run(
            &ctx(),
            ReadInput {
                path: f.to_string_lossy().into_owned(),
                start_line: None,
                end_line: None,
            },
        )
        .await;
        match out {
            ToolOut::TextPlusImage {
                text,
                mime,
                data: d,
            } => {
                assert_eq!(
                    text,
                    format!("{} (image/png, {} bytes)", f.to_string_lossy(), data.len())
                );
                assert_eq!(mime, "image/png");
                assert_eq!(d, data);
            }
            ToolOut::Text(t) => panic!("expected image branch, got text: {t:?}"),
        }
        // Case-insensitive like Go.
        let f2 = dir.join("UPPER.JPG");
        std::fs::write(&f2, b"jpg").unwrap();
        let out = run(
            &ctx(),
            ReadInput {
                path: f2.to_string_lossy().into_owned(),
                start_line: None,
                end_line: None,
            },
        )
        .await;
        assert!(matches!(
            out,
            ToolOut::TextPlusImage {
                mime: "image/jpeg",
                ..
            }
        ));
        // Non-image extensions stay textual.
        let out = run(
            &ctx(),
            ReadInput {
                path: dir.join("plain.txt").to_string_lossy().into_owned(),
                start_line: None,
                end_line: None,
            },
        )
        .await;
        assert!(matches!(out, ToolOut::Text(_)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn reads_numbered_lines() {
        let dir = tempdir("numbered");
        let f = dir.join("a.txt");
        std::fs::write(&f, "alpha\nbeta\ngamma\n").unwrap();
        let out = run_text(
            &ctx(),
            ReadInput {
                path: f.to_string_lossy().into_owned(),
                start_line: None,
                end_line: None,
            },
        )
        .await;
        assert_eq!(
            out,
            format!(
                "{} (3 lines total)\n     1\talpha\n     2\tbeta\n     3\tgamma\n",
                f.to_string_lossy()
            )
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn scopes_and_caps() {
        let dir = tempdir("scoped");
        let f = dir.join("a.txt");
        let content: String = (1..=3000).map(|i| format!("l{i}\n")).collect();
        std::fs::write(&f, &content).unwrap();
        let out = run_text(
            &ctx(),
            ReadInput {
                path: f.to_string_lossy().into_owned(),
                start_line: Some(2999),
                end_line: None,
            },
        )
        .await;
        assert!(out.starts_with(&format!("{} (3000 lines total)\n", f.to_string_lossy())));
        assert!(out.contains("  2999\tl2999\n  3000\tl3000\n"));
        // Explicit start with a short remaining range is NOT auto-capped
        // (Go only caps when the range derives from defaults).
        assert!(!out.contains("showing lines"), "got: {out:?}");

        // Unscoped read from 1 → auto-capped at 2000 lines.
        let out = run_text(
            &ctx(),
            ReadInput {
                path: f.to_string_lossy().into_owned(),
                start_line: None,
                end_line: None,
            },
        )
        .await;
        assert!(out.contains(
            "...[showing lines 1-2000 of 3000; pass start_line/end_line to read more]\n"
        ));

        // start past end
        let out = run_text(
            &ctx(),
            ReadInput {
                path: f.to_string_lossy().into_owned(),
                start_line: Some(3001),
                end_line: None,
            },
        )
        .await;
        assert_eq!(
            out,
            format!(
                "{} (3000 lines total) — start_line 3001 is past end of file",
                f.to_string_lossy()
            )
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn empty_file_and_missing() {
        let dir = tempdir("edge");
        let f = dir.join("empty.txt");
        std::fs::write(&f, "").unwrap();
        // Go's empty-file branch ("(empty file)") is unreachable there too:
        // strings.Split("", "\n") yields [""], so a byte-empty file reports
        // one blank numbered line — mirrored exactly.
        let out = run_text(
            &ctx(),
            ReadInput {
                path: f.to_string_lossy().into_owned(),
                start_line: None,
                end_line: None,
            },
        )
        .await;
        assert_eq!(
            out,
            format!("{} (1 lines total)\n     1\t\n", f.to_string_lossy())
        );

        let missing = run_text(
            &ctx(),
            ReadInput {
                path: dir.join("nope.txt").to_string_lossy().into_owned(),
                start_line: None,
                end_line: None,
            },
        )
        .await;
        assert_eq!(
            missing,
            format!(
                "read_error: open {}: no such file or directory",
                dir.join("nope.txt").to_string_lossy()
            )
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn crocks_becomes_lf_numbering() {
        let dir = tempdir("crlf");
        let f = dir.join("crlf.txt");
        std::fs::write(&f, b"one\r\ntwo\r\n").unwrap();
        let out = run_text(
            &ctx(),
            ReadInput {
                path: f.to_string_lossy().into_owned(),
                start_line: None,
                end_line: None,
            },
        )
        .await;
        assert_eq!(
            out,
            format!(
                "{} (2 lines total)\n     1\tone\n     2\ttwo\n",
                f.to_string_lossy()
            )
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn long_line_truncation_notice() {
        let dir = tempdir("longline");
        let f = dir.join("long.txt");
        std::fs::write(&f, "y".repeat(2500)).unwrap();
        let out = run_text(
            &ctx(),
            ReadInput {
                path: f.to_string_lossy().into_owned(),
                start_line: None,
                end_line: None,
            },
        )
        .await;
        assert!(
            out.contains("...[line truncated, 2500 chars total]"),
            "got: {out:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod image_mime_tests {
    use super::image_mime;

    #[test]
    fn image_mime_table() {
        assert_eq!(image_mime("x.png"), Some("image/png"));
        assert_eq!(image_mime("x.jpg"), Some("image/jpeg"));
        assert_eq!(image_mime("x.JPEG"), Some("image/jpeg"));
        assert_eq!(image_mime("x.gif"), Some("image/gif"));
        assert_eq!(image_mime("x.webp"), Some("image/webp"));
        assert_eq!(image_mime("IMG.bmp"), Some("image/bmp"));
        assert_eq!(image_mime("x.txt"), None);
        assert_eq!(image_mime("x"), None);
    }
}
