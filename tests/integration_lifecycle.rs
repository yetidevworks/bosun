//! Integration test for `TmuxClient::kill_session` and `set_display_name`
//! against a real throwaway tmux server. Verifies that kill removes
//! the session from list-sessions, kill of a missing session is a
//! no-op (idempotent), and set_display_name propagates through to
//! the next list-sessions read of `@bosun_display`.

#![cfg(feature = "tmux-it")]

use std::time::{SystemTime, UNIX_EPOCH};

use bosun::tmux::{CreateSpec, SessionMetadata, TmuxClient, TokioTmuxClient};

fn unique_socket(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("bosun-life-{}-{}-{}", tag, std::process::id(), nanos)
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

#[tokio::test(flavor = "current_thread")]
async fn kill_session_removes_from_list() {
    let sock = unique_socket("kill");
    let client = TokioTmuxClient::with_socket(sock.clone());

    client
        .create_session(&CreateSpec {
            name: "bosun-zap-dead".into(),
            display_name: Some("zap".into()),
            path: "/tmp".into(),
            command: String::new(),
            metadata: None,
        })
        .await
        .expect("create ok");

    let before = client.list_sessions().await.unwrap();
    assert!(before.iter().any(|s| s.name == "bosun-zap-dead"));

    client
        .kill_session("bosun-zap-dead")
        .await
        .expect("kill ok");

    let after = client.list_sessions().await.unwrap();
    assert!(!after.iter().any(|s| s.name == "bosun-zap-dead"));

    kill_server(&sock);
}

#[tokio::test(flavor = "current_thread")]
async fn kill_session_missing_is_noop() {
    let sock = unique_socket("killmiss");
    let client = TokioTmuxClient::with_socket(sock.clone());

    // Bootstrap a server so we can get stderr from the missing-session
    // case rather than "no server running".
    client
        .create_session(&CreateSpec {
            name: "bosun-keep-alive".into(),
            display_name: Some("keep".into()),
            path: "/tmp".into(),
            command: String::new(),
            metadata: None,
        })
        .await
        .expect("seed ok");

    // Killing a nonexistent session should NOT return an error.
    client
        .kill_session("bosun-nope-feed")
        .await
        .expect("missing kill should be Ok");

    kill_server(&sock);
}

#[tokio::test(flavor = "current_thread")]
async fn set_display_name_updates_option_and_list() {
    let sock = unique_socket("rename");
    let client = TokioTmuxClient::with_socket(sock.clone());

    client
        .create_session(&CreateSpec {
            name: "bosun-abc-1234".into(),
            display_name: Some("Old Name".into()),
            path: "/tmp".into(),
            command: String::new(),
            metadata: None,
        })
        .await
        .expect("create ok");

    // Sanity: list shows the original display.
    let first = client.list_sessions().await.unwrap();
    let s = first
        .iter()
        .find(|s| s.name == "bosun-abc-1234")
        .expect("session");
    assert_eq!(s.display_name.as_deref(), Some("Old Name"));

    client
        .set_display_name("bosun-abc-1234", "Fresh Name")
        .await
        .expect("rename ok");

    let after = client.list_sessions().await.unwrap();
    let s = after
        .iter()
        .find(|s| s.name == "bosun-abc-1234")
        .expect("session still exists");
    assert_eq!(s.display_name.as_deref(), Some("Fresh Name"));
    // Internal name unchanged.
    assert_eq!(s.name, "bosun-abc-1234");

    kill_server(&sock);
}

#[tokio::test(flavor = "current_thread")]
async fn metadata_round_trips_through_tmux_options() {
    let sock = unique_socket("meta");
    let client = TokioTmuxClient::with_socket(sock.clone());

    let meta = SessionMetadata {
        display_name: "My Session".into(),
        path: "/tmp/my".into(),
        agent: "claude".into(),
        args: "--model=opus".into(),
        claude_session_mode: "Resume".into(),
        claude_skip_permissions: true,
        codex_yolo: false,
        kimi_session_mode: "New".into(),
        kimi_yolo: false,
        codex_session_mode: "New".into(),
        opencode_session_mode: "New".into(),
        opencode_auto: false,
        qwen_session_mode: "New".into(),
        qwen_yolo: false,
        container_id: None,
        worktree_path: Some("/tmp/.worktrees/feat".into()),
        branch: Some("feat".into()),
    };
    client
        .create_session(&CreateSpec {
            name: "bosun-meta-round".into(),
            display_name: Some("My Session".into()),
            path: "/tmp".into(),
            command: String::new(),
            metadata: Some(meta.clone()),
        })
        .await
        .expect("create with metadata ok");

    let got = client
        .get_session_metadata("bosun-meta-round")
        .await
        .expect("get_session_metadata ok")
        .expect("metadata should exist after create");
    assert_eq!(got.display_name, "My Session");
    assert_eq!(got.path, "/tmp/my");
    assert_eq!(got.agent, "claude");
    assert_eq!(got.args, "--model=opus");
    assert_eq!(got.claude_session_mode, "Resume");
    assert!(got.claude_skip_permissions);
    assert!(!got.codex_yolo);
    // Worktree fields round-trip through the widened read format/guard.
    assert_eq!(got.worktree_path.as_deref(), Some("/tmp/.worktrees/feat"));
    assert_eq!(got.branch.as_deref(), Some("feat"));

    kill_server(&sock);
}

#[tokio::test(flavor = "current_thread")]
async fn metadata_returns_none_when_agent_option_missing() {
    let sock = unique_socket("nometa");
    let client = TokioTmuxClient::with_socket(sock.clone());

    // Create a bare session with no @bosun_agent option set.
    client
        .create_session(&CreateSpec {
            name: "bosun-bare-meta".into(),
            display_name: Some("bare".into()),
            path: "/tmp".into(),
            command: String::new(),
            metadata: None,
        })
        .await
        .expect("bare create ok");

    let got = client
        .get_session_metadata("bosun-bare-meta")
        .await
        .expect("get_session_metadata ok");
    assert!(got.is_none(), "expected None when @bosun_agent is unset");

    kill_server(&sock);
}
