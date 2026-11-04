//! Git tools — mirrors Go `registerGit` (main.go): four fixed `git`
//! invocations (`git_status`/`git_diff`/`git_log`/`git_branch`), each running
//! `git <args>` directly (NOT via `shellCommand`; Go uses
//! `exec.CommandContext("git", args...)` with `cmd.Dir = cwd` and
//! `cmd.Env = childEnv()`), capturing combined stdout+stderr.
//!
//! Output contract (byte-exact to Go):
//! - cwd resolution failure → `git_error: <err>` (safePath error text)
//! - any spawn/exit error → `git_error: <combined output>` — notably a
//!   non-repo cwd yields `git_error: fatal: not a git repository …\n` and a
//!   missing `git` binary yields exactly `git_error: ` (StartError leaves the
//!   combined buffer empty);
//! - empty combined output without an error → `(no output)`;
//! - otherwise the raw combined output, un-normalized (no CRLF mangling).
//!
//! Micro-divergence vs Go: Go's CombinedOutput interleaves the two pipes by
//! arrival; here stdout and stderr are separate pipes concatenated
//! (stdout-then-stderr) after completion. For these four commands one of the
//! two streams is empty in every practical case (stderr-only failure,
//! stdout-only success), so the text matches; a case that writes concurrently
//! to both streams could order lines differently.

use schemars::JsonSchema;
use serde::Deserialize;
use std::process::Stdio;

use super::{Ctx, default_string, safe_path};
use crate::auth::child_env;

pub const STATUS_ARGS: &[&str] = &["status", "--short", "--branch"];
pub const DIFF_ARGS: &[&str] = &["diff", "--no-ext-diff"];
pub const LOG_ARGS: &[&str] = &["log", "--oneline", "--decorate", "-20"];
pub const BRANCH_ARGS: &[&str] = &["branch", "-vv"];

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct GitInput {
    pub cwd: Option<String>,
}

/// Mirrors Go's registerGit handler body for one `<args>` invocation.
pub async fn run(ctx: &Ctx, args: &[&str], input: GitInput) -> String {
    let cwd = match safe_path(ctx, &default_string(input.cwd.as_deref(), ".")) {
        Ok(p) => p,
        Err(e) => return format!("git_error: {e}"),
    };
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .current_dir(&cwd)
        .env_clear()
        .envs(child_env())
        .stdin(Stdio::null());

    let output = match cmd.output().await {
        Ok(o) => o,
        // Go: StartError → CombinedOutput error with an empty buffer →
        // textResult("git_error: " + "") — trailing space, nothing else.
        Err(_spawn_err) => return "git_error: ".to_string(),
    };
    let combined: Vec<u8> = [output.stdout.as_slice(), output.stderr.as_slice()].concat();
    // Go: `if e != nil { return textResult("git_error: " + string(out)) }` —
    // any non-zero exit (e.g. `fatal: not a git repository …`) lands here.
    if !output.status.success() {
        return format!("git_error: {}", String::from_utf8_lossy(&combined));
    }
    if combined.is_empty() {
        return "(no output)".to_string();
    }
    String::from_utf8_lossy(&combined).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(workspace: &str) -> Ctx {
        Ctx::plain(workspace)
    }

    #[tokio::test]
    async fn status_in_a_real_repo() {
        // Run against this crate (a git worktree).
        let out = run(
            &ctx("/Users/macbookpro/Tools/ryns-code-mcp"),
            STATUS_ARGS,
            GitInput::default(),
        )
        .await;
        assert!(out.starts_with("## "), "unexpected: {out:?}");
        assert!(!out.starts_with("git_error"));
    }

    #[tokio::test]
    async fn non_repo_yields_git_error_fatal() {
        let dir = std::env::temp_dir().join(format!("ryns-git-nonrepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = run(
            &ctx(&dir.to_string_lossy()),
            STATUS_ARGS,
            GitInput::default(),
        )
        .await;
        assert_eq!(
            out,
            "git_error: fatal: not a git repository (or any of the parent directories): .git\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn quiet_success_yields_no_output_marker() {
        let repo = std::env::temp_dir().join(format!("ryns-git-quiet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        let ok = tokio::process::Command::new("git")
            .arg("init")
            .current_dir(&repo)
            .output()
            .await
            .unwrap();
        assert!(ok.status.success(), "git init failed: {ok:?}");
        // A fresh repo with no commits: diff produces nothing (exit 0).
        let out = run(
            &ctx(&repo.to_string_lossy()),
            DIFF_ARGS,
            GitInput::default(),
        )
        .await;
        assert_eq!(out, "(no output)");
        let _ = std::fs::remove_dir_all(&repo);
    }
}
