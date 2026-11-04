//! `list_directory` — mirrors Go main.go (~1699 handler): sorted entries,
//! `[DIR]  name` / `[FILE] name (N bytes)` rows, `(empty)` for nothing.

use schemars::JsonSchema;
use serde::Deserialize;

use super::{Ctx, default_string, go_io_error, safe_path};

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ListInput {
    /// directory path (absolute, or relative to workspace); defaults to "."
    #[serde(default)]
    pub path: Option<String>,
}

pub async fn run(ctx: &Ctx, input: ListInput) -> String {
    let dir = match safe_path(ctx, &default_string(input.path.as_deref(), ".")) {
        Ok(p) => p,
        Err(e) => return format!("list_error: {e}"),
    };
    let mut read_dir = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(e) => return format!("list_error: {}", go_io_error("open", &dir, &e)),
    };

    // Collect (name, is_dir, size), then byte-sort by name like Go. Enum
    // errors from the iteration loop rather than stopping silently: Go's
    // os.ReadDir surfaces a failure mid-iteration as `readdirent dir: …`.
    let mut entries: Vec<(String, bool, u64)> = Vec::new();
    loop {
        match read_dir.next_entry().await {
            Ok(Some(entry)) => {
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
                entries.push((name, is_dir, size));
            }
            Ok(None) => break,
            Err(e) => return format!("list_error: {}", go_io_error("readdirent", &dir, &e)),
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    if entries.is_empty() {
        return "(empty)".to_string();
    }
    entries
        .iter()
        .map(|(name, is_dir, size)| {
            if *is_dir {
                format!("[DIR]  {name}")
            } else {
                format!("[FILE] {name} ({size} bytes)")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
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
            "ryns-rs-list-{tag}-{}-{}",
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
    async fn rows_sorted_with_types_and_sizes() {
        let dir = tempdir("rows");
        std::fs::write(dir.join("b.txt"), "12345").unwrap(); // 5 bytes
        std::fs::write(dir.join("a.txt"), "1").unwrap(); // 1 byte
        std::fs::create_dir(dir.join("subdir")).unwrap();

        let out = run(
            &ctx(),
            ListInput {
                path: Some(dir.to_string_lossy().into_owned()),
            },
        )
        .await;
        assert_eq!(
            out,
            format!(
                "[FILE] {} (1 bytes)\n[FILE] {} (5 bytes)\n[DIR]  {}",
                "a.txt", "b.txt", "subdir"
            )
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn empty_and_missing() {
        let dir = tempdir("empty");
        let out = run(
            &ctx(),
            ListInput {
                path: Some(dir.to_string_lossy().into_owned()),
            },
        )
        .await;
        assert_eq!(out, "(empty)");

        let missing = run(
            &ctx(),
            ListInput {
                path: Some(dir.join("nope").to_string_lossy().into_owned()),
            },
        )
        .await;
        assert_eq!(
            missing,
            format!(
                "list_error: open {}: no such file or directory",
                dir.join("nope").to_string_lossy()
            )
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
