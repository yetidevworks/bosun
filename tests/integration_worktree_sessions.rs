//! End-to-end integration test for the git worktree session lifecycle
//! against REAL `git` and a REAL throwaway `tmux` server. This covers
//! the parts unit tests (tempdir git, no tmux server) can't reach:
//!   * the `@bosun_worktree_path` / `@bosun_branch` round-trip through
//!     an actual `tmux list-sessions` read (test 1),
//!   * the exact merge-path cleanup sequence `handle_kill_remove_worktree`
//!     performs against real git (test 2),
//!   * the dirty guard that stops a stray worktree removal (test 3),
//!   * the force-remove + branch-delete primitive the Task 4 create
//!     rollback relies on (test 4).
//!
//! Mirrors `tests/integration_lifecycle.rs`: same `#![cfg(feature =
//! "tmux-it")]` gate, `unique_socket` / `tmux` / `kill_server` helpers,
//! and `TokioTmuxClient::with_socket` on a unique `-L` socket so each
//! test gets an isolated tmux server it tears down itself.

#![cfg(feature = "tmux-it")]

use std::time::{SystemTime, UNIX_EPOCH};

use bosun::tmux::{CreateSpec, SessionMetadata, TmuxClient, TokioTmuxClient};

fn unique_socket(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("bosun-wt-{}-{}-{}", tag, std::process::id(), nanos)
}

fn tmux(socket: &str, args: &[&str]) -> std::process::Output {
    std::process::Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .args(args)
        .output()
        .expect("spawn tmux")
}

fn kill_server(socket: &str) {
    let _ = tmux(socket, &["kill-server"]);
}

/// Spawn `git -C <dir> <args>`, asserting the command succeeds.
fn run_git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Capture `git -C <dir> <args>` stdout (trimmed), asserting success.
fn git_stdout(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Create an initialised repo with one empty commit at `dir/repo`.
fn init_repo(dir: &std::path::Path) -> std::path::PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir(&repo).unwrap();
    run_git(&repo, &["init", "-q"]);
    run_git(&repo, &["config", "user.email", "t@t"]);
    run_git(&repo, &["config", "user.name", "t"]);
    run_git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    repo
}

/// 1. `worktree_add` creates the dir + branch, and the two worktree
/// `@bosun_*` options round-trip back through `list_sessions()` as
/// `TmuxSession { worktree_path, branch }`. This is the tmux-touching
/// test — the option round-trip through a real `tmux list-sessions`
/// read is exactly what the tempdir-only unit tests cannot cover.
#[tokio::test(flavor = "current_thread")]
async fn worktree_add_and_options_round_trip_through_list_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let wt = dir.path().join("wt");
    let wt_str = wt.to_str().unwrap();

    let sock = unique_socket("round");
    let client = TokioTmuxClient::with_socket(sock.clone());

    // Real git worktree_add: dir + branch must appear.
    client
        .worktree_add(repo.to_str().unwrap(), "feat", wt_str)
        .await
        .expect("worktree_add ok");
    assert!(wt.join(".git").exists(), "worktree dir should exist");
    let branch_list = git_stdout(&repo, &["branch", "--list", "feat"]);
    assert!(
        branch_list.contains("feat"),
        "feat branch should exist, got {branch_list:?}"
    );

    // Create a real tmux session IN the worktree dir, carrying the two
    // worktree fields as metadata, so the create path writes
    // `@bosun_worktree_path` / `@bosun_branch` as tmux user options.
    let meta = SessionMetadata {
        display_name: "Feat Session".into(),
        path: wt_str.into(),
        agent: "claude".into(),
        args: String::new(),
        claude_session_mode: "New".into(),
        claude_skip_permissions: false,
        codex_yolo: false,
        kimi_session_mode: "New".into(),
        kimi_yolo: false,
        codex_session_mode: "New".into(),
        opencode_session_mode: "New".into(),
        opencode_auto: false,
        qwen_session_mode: "New".into(),
        qwen_yolo: false,
        container_id: None,
        worktree_path: Some(wt_str.into()),
        branch: Some("feat".into()),
    };
    client
        .create_session(&CreateSpec {
            name: "bosun-wt-round".into(),
            display_name: Some("Feat Session".into()),
            path: wt_str.into(),
            command: String::new(),
            metadata: Some(meta),
        })
        .await
        .expect("create_session ok");

    // Read the options back through an actual `tmux list-sessions`.
    let sessions = client.list_sessions().await.expect("list_sessions ok");
    let s = sessions
        .iter()
        .find(|s| s.name == "bosun-wt-round")
        .expect("session in list");
    assert_eq!(
        s.worktree_path.as_deref(),
        Some(wt_str),
        "worktree_path should round-trip through list_sessions"
    );
    assert_eq!(
        s.branch.as_deref(),
        Some("feat"),
        "branch should round-trip through list_sessions"
    );

    kill_server(&sock);
}

/// 2. The merge-path cleanup sequence, end to end against real git —
/// exactly what `handle_kill_remove_worktree(merge = true)` runs:
/// `is_dirty` (false) → `branch_merge` → `worktree_remove(force=false)`
/// → `branch_delete`. Asserts the worktree and branch are gone and the
/// commit made on `feat` landed on the repo's checked-out branch.
#[tokio::test(flavor = "current_thread")]
async fn merge_path_cleanup_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let wt = dir.path().join("wt");
    let wt_str = wt.to_str().unwrap();
    let repo_str = repo.to_str().unwrap();

    let client = TokioTmuxClient::new();
    client
        .worktree_add(repo_str, "feat", wt_str)
        .await
        .expect("worktree_add ok");

    // Make a real commit ON `feat` inside the worktree.
    std::fs::write(wt.join("feature.txt"), "shipped").unwrap();
    run_git(&wt, &["add", "feature.txt"]);
    run_git(&wt, &["commit", "-q", "-m", "add feature"]);

    // Cleanup sequence, mirroring the actor's merge path.
    assert!(
        !client.is_dirty(wt_str).await.expect("is_dirty ok"),
        "committed worktree should be clean"
    );
    client
        .branch_merge(repo_str, "feat")
        .await
        .expect("branch_merge ok");
    client
        .worktree_remove(repo_str, wt_str, false)
        .await
        .expect("worktree_remove ok");
    client
        .branch_delete(repo_str, "feat")
        .await
        .expect("branch_delete ok");

    // Worktree dir gone.
    assert!(!wt.exists(), "worktree dir should be removed");
    // Branch gone.
    let branch_list = git_stdout(&repo, &["branch", "--list", "feat"]);
    assert!(
        branch_list.is_empty(),
        "feat branch should be deleted, got {branch_list:?}"
    );
    // The feat commit is now on the repo's checked-out branch (merged).
    assert!(
        repo.join("feature.txt").exists(),
        "merged file should be present in the main repo checkout"
    );
}

/// 3. Dirty guard: an untracked file makes `is_dirty` true, and a
/// non-force `worktree_remove` errors (git refuses to drop a dirty
/// tree). This is what stops the actor from silently discarding work.
#[tokio::test(flavor = "current_thread")]
async fn dirty_worktree_blocks_removal() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let wt = dir.path().join("wt");
    let wt_str = wt.to_str().unwrap();
    let repo_str = repo.to_str().unwrap();

    let client = TokioTmuxClient::new();
    client
        .worktree_add(repo_str, "feat", wt_str)
        .await
        .expect("worktree_add ok");

    // Untracked file dirties the worktree.
    std::fs::write(wt.join("scratch.txt"), "wip").unwrap();

    assert!(
        client.is_dirty(wt_str).await.expect("is_dirty ok"),
        "worktree with an untracked file should be dirty"
    );
    assert!(
        client
            .worktree_remove(repo_str, wt_str, false)
            .await
            .is_err(),
        "non-force remove of a dirty worktree should error"
    );
}

/// 4. Create rollback primitive (Task 4, commit 904c9e8). The actual
/// `create_session` free fn's rollback is private to the actor module
/// and not reachable from an integration test; making it `pub` just to
/// test it would weaken production code. Instead we assert the rollback
/// PRIMITIVE it relies on: after a pristine `worktree_add`, a
/// `worktree_remove(force = true)` cleanly removes the just-created
/// worktree AND the branch is then free to delete — proving the cleanup
/// path Task 4 drives on a failed create is sound.
#[tokio::test(flavor = "current_thread")]
async fn create_rollback_primitive_removes_pristine_worktree_and_branch() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let wt = dir.path().join("wt");
    let wt_str = wt.to_str().unwrap();
    let repo_str = repo.to_str().unwrap();

    let client = TokioTmuxClient::new();
    client
        .worktree_add(repo_str, "feat", wt_str)
        .await
        .expect("worktree_add ok");
    assert!(wt.exists(), "worktree created");

    // Rollback: force-remove the pristine worktree (the rollback path
    // uses force so it never wedges on a half-set-up tree), then delete
    // the branch it created.
    client
        .worktree_remove(repo_str, wt_str, true)
        .await
        .expect("force worktree_remove ok");
    assert!(!wt.exists(), "worktree removed on rollback");
    client
        .branch_delete(repo_str, "feat")
        .await
        .expect("branch_delete ok");
    let branch_list = git_stdout(&repo, &["branch", "--list", "feat"]);
    assert!(
        branch_list.is_empty(),
        "feat branch should be gone after rollback, got {branch_list:?}"
    );
}
