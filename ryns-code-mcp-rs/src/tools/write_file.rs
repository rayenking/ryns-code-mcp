//! `write_file` — mirrors Go main.go (~1572 handler): creates parent
//! directories, writes as UTF-8 text, and reports created/overwritten with
//! the same message shapes.

use schemars::JsonSchema;
use serde::Deserialize;

use super::{Ctx, go_io_error, safe_path, write_file_mode};
use crate::safety::GateResult;

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct WriteInput {
    /// file path (absolute, or relative to workspace)
    pub path: String,
    pub content: String,
}

pub async fn run(ctx: &Ctx, input: WriteInput) -> String {
    let path = match safe_path(ctx, &input.path) {
        Ok(p) => p,
        Err(e) => return format!("write_error: {e}"),
    };
    // Phase 3 gate: secret-path rules on write targets (all modes audit).
    if let GateResult::Blocked(text) = crate::safety::gate_paths(ctx, "write_file", &[&path]).await
    {
        return text;
    }

    let existed_before = match tokio::fs::metadata(&path).await {
        Ok(m) => (true, m.len()),
        Err(_) => (false, 0u64),
    };
    let (existed_before, prev_size) = existed_before;

    if let Some(parent) = std::path::Path::new(&path).parent() {
        match tokio::fs::create_dir_all(parent).await {
            Ok(()) => {}
            Err(e) => {
                let parent_display = parent.to_string_lossy();
                return format!("write_error: {}", go_io_error("mkdir", &parent_display, &e));
            }
        }
    }

    // Go uses os.WriteFile(…, 0644); mirror the pre-umask mode on unix.
    let write_res = write_file_mode(&path, input.content.as_bytes()).await;
    if let Err(e) = write_res {
        return format!("write_error: {}", go_io_error("write", &path, &e));
    }

    // Go: strings.Count(content, "\n") + 1 (no trailing-newline adjustment).
    let line_count = input.content.matches('\n').count() + 1;
    if existed_before {
        return format!(
            "overwrote {path}: {prev_size} -> {} bytes ({line_count} lines)",
            input.content.len()
        );
    }
    format!(
        "created {path}: {} bytes ({line_count} lines)",
        input.content.len()
    )
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
            "ryns-rs-write-file-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn create_then_overwrite_messages() {
        let dir = tempdir("basic");
        let f = dir.join("nested").join("sub").join("b.txt");
        let out = run(
            &ctx(),
            WriteInput {
                path: f.to_string_lossy().into_owned(),
                content: "hello\nworld".to_string(),
            },
        )
        .await;
        assert_eq!(
            out,
            format!("created {}: 11 bytes (2 lines)", f.to_string_lossy())
        );
        let out = run(
            &ctx(),
            WriteInput {
                path: f.to_string_lossy().into_owned(),
                content: "hi\n".to_string(),
            },
        )
        .await;
        assert_eq!(
            out,
            format!("overwrote {}: 11 -> 3 bytes (2 lines)", f.to_string_lossy())
        );
        let body = std::fs::read_to_string(&f).unwrap();
        assert_eq!(body, "hi\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Effective create-mode for the tool's writes, compared against a
    /// umask-agnostic control file instead of hard-coding 0o644: a umask can
    /// only clear bits, and std's `File::create` is 0o666 pre-umask, so the
    /// expected result is exactly `0o644 ∩ (control & 0o777)`.
    #[cfg(unix)]
    #[tokio::test]
    async fn unix_write_mode_respects_umask_like_std_create() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir("mode");
        let f = dir.join("mode.txt");
        run(
            &ctx(),
            WriteInput {
                path: f.to_string_lossy().into_owned(),
                content: "x".into(),
            },
        )
        .await;
        let control = dir.join("umask-control");
        std::fs::File::create(&control).unwrap();
        let control_mode = std::fs::metadata(&control).unwrap().permissions().mode() & 0o777;
        let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o644 & control_mode,
            "mode bits: {mode:o} (control {control_mode:o})"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
