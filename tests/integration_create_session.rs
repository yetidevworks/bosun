//! Integration test: create a session via TokioTmuxClient and verify
//! tmux sees it, that @bosun_display is set, and that list-sessions
//! round-trips the display name field.

#![cfg(feature = "tmux-it")]

use std::time::{SystemTime, UNIX_EPOCH};

use bosun::tmux::{CreateSpec, TmuxClient, TokioTmuxClient};

fn unique_socket(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("bosun-create-{}-{}-{}", tag, std::process::id(), nanos)
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
async fn create_session_sets_display_name_and_appears_in_list() {
    let sock = unique_socket("basic");
    // Start with an empty server — create_session starts the first one.
    let client = TokioTmuxClient::with_socket(sock.clone());

    let spec = CreateSpec {
        name: "bosun-rasterfox-a1b2c3d4".to_string(),
        display_name: Some("rasterfox".to_string()),
        path: "/tmp".to_string(),
        command: String::new(), // default shell
        metadata: None,
    };

    let created = client.create_session(&spec).await.expect("create ok");
    assert_eq!(created, "bosun-rasterfox-a1b2c3d4");

    // list-sessions should return the session with the display_name populated.
    let sessions = client.list_sessions().await.expect("list ok");
    let ours = sessions
        .iter()
        .find(|s| s.name == "bosun-rasterfox-a1b2c3d4")
        .expect("session should exist");
    assert_eq!(ours.display_name.as_deref(), Some("rasterfox"));
    assert_eq!(ours.display(), "rasterfox");

    // Also verify via raw tmux that @bosun_display was set.
    let opt = tmux(
        &sock,
        &[
            "show-options",
            "-qv",
            "-t",
            "bosun-rasterfox-a1b2c3d4",
            "@bosun_display",
        ],
    );
    let value = String::from_utf8_lossy(&opt.stdout).trim().to_string();
    assert_eq!(value, "rasterfox");

    kill_server(&sock);
}

#[tokio::test(flavor = "current_thread")]
async fn create_session_without_display_name_does_not_set_option() {
    let sock = unique_socket("nodisplay");
    let client = TokioTmuxClient::with_socket(sock.clone());

    let spec = CreateSpec {
        name: "bosun-bare-deadbeef".to_string(),
        display_name: None,
        path: "/tmp".to_string(),
        command: String::new(),
        metadata: None,
    };
    client.create_session(&spec).await.expect("create ok");

    let sessions = client.list_sessions().await.expect("list ok");
    let ours = sessions
        .iter()
        .find(|s| s.name == "bosun-bare-deadbeef")
        .expect("session should exist");
    assert!(ours.display_name.is_none());
    // display() falls back to the internal name.
    assert_eq!(ours.display(), "bosun-bare-deadbeef");

    kill_server(&sock);
}

/// Regression for issue #10: a session created with a `~/…` path used
/// to end up in `$HOME` without the subpath.
///
/// The trap is that tmux neither expands the tilde nor complains about
/// it — given a `-c` directory that doesn't exist it silently starts
/// the session in `$HOME`, so the mistake is invisible until you look
/// at where the session actually is. This test pins both halves: the
/// raw tilde really does misbehave, and the path bosun now hands tmux
/// (run through `expand_tilde`) lands in the right directory.
#[tokio::test(flavor = "current_thread")]
async fn tilde_path_is_expanded_before_reaching_tmux() {
    let Ok(home) = std::env::var("HOME") else {
        return; // no HOME to expand against; nothing to assert
    };
    let sock = unique_socket("tilde");
    let client = TokioTmuxClient::with_socket(sock.clone());

    // A real directory under $HOME to aim at, so "landed in the right
    // place" is distinguishable from tmux's $HOME fallback.
    let leaf = format!(".bosun-tilde-it-{}", std::process::id());
    let target = format!("{home}/{leaf}");
    std::fs::create_dir_all(&target).expect("create target dir");

    let mk = |name: &str, path: String| CreateSpec {
        name: name.to_string(),
        display_name: Some(name.to_string()),
        path,
        command: String::new(),
        metadata: None,
    };

    // What bosun sends today: expanded.
    client
        .create_session(&mk(
            "bosun-tilde-ok",
            bosun::util::path::expand_tilde(&format!("~/{leaf}")),
        ))
        .await
        .expect("create expanded");
    // What it used to send: the raw tilde.
    client
        .create_session(&mk("bosun-tilde-raw", format!("~/{leaf}")))
        .await
        .expect("create raw");

    let cwd = |session: &str| {
        let out = tmux(
            &sock,
            &[
                "display-message",
                "-p",
                "-t",
                session,
                "#{pane_current_path}",
            ],
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    assert_eq!(
        cwd("bosun-tilde-ok"),
        target,
        "expanded path should land in the subdirectory"
    );
    assert_ne!(
        cwd("bosun-tilde-raw"),
        target,
        "sanity: tmux does not expand a tilde itself — if this ever starts \
         passing, tmux learned to expand and the fix could be revisited"
    );

    kill_server(&sock);
    let _ = std::fs::remove_dir(&target);
}
