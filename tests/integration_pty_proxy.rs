//! Real-tmux checks for panes whose shell runs behind a PTY proxy
//! (#24). `script` stands in for Atuin's `pty-proxy`: both own the
//! pane's terminal and run the shell on a second PTY, so tmux reports
//! `script` as `#{pane_current_command}` at the prompt and while a
//! command runs alike.
//!
//! Each test spawns its own throwaway tmux server on a unique `-L`
//! socket and kills it at the end.

#![cfg(feature = "tmux-it")]

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bosun::tmux::{TmuxClient, TokioTmuxClient};

const SESSION: &str = "bosun-proxy";

fn tmux(socket: &str, args: &[&str]) -> String {
    let out = std::process::Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .args(args)
        .output()
        .expect("spawn tmux");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A tmux server whose one pane runs `sh` behind `script`.
struct Server(String);

impl Server {
    fn start(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let s = Server(format!(
            "bosun-proxy-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        // BSD `script` takes the command as trailing arguments,
        // util-linux's takes it with `-c`.
        let proxied = if cfg!(target_os = "macos") {
            "exec script -q /dev/null sh"
        } else {
            "exec script -q -c 'exec sh' /dev/null"
        };
        tmux(
            &s.0,
            &[
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                SESSION,
                proxied,
            ],
        );
        s.wait_for(|cmd| cmd == "script");
        s
    }

    fn raw_command(&self) -> String {
        tmux(
            &self.0,
            &[
                "display-message",
                "-p",
                "-t",
                SESSION,
                "#{pane_current_command}",
            ],
        )
    }

    fn wait_for(&self, ok: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ok(&self.raw_command()) {
            assert!(Instant::now() < deadline, "pane never settled");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        tmux(&self.0, &["kill-server"]);
    }
}

async fn pane_command(client: &TokioTmuxClient) -> Option<String> {
    let sessions = client.list_sessions().await.expect("list ok");
    sessions
        .into_iter()
        .find(|s| s.name == SESSION)
        .and_then(|s| s.pane_command)
}

/// Poll `list_sessions` until the pane reports `want` (the proxy cache
/// keeps a process table for up to a second).
async fn wait_for_command(client: &TokioTmuxClient, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let got = pane_command(client).await;
        if got.as_deref() == Some(want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pane command: wanted {want}, got {got:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn list_sessions_looks_through_the_proxy() {
    let server = Server::start("list");
    let client = TokioTmuxClient::with_socket(server.0.clone());

    // tmux itself only ever sees the proxy.
    assert_eq!(server.raw_command(), "script");
    wait_for_command(&client, "sh").await;

    tmux(
        &server.0,
        &["send-keys", "-t", SESSION, "sleep 30", "Enter"],
    );
    wait_for_command(&client, "sleep").await;
    assert_eq!(server.raw_command(), "script");

    // The status poll's rows get the same treatment.
    let rows = client.list_status_rows().await.expect("status rows");
    let row = rows.iter().find(|s| s.name == SESSION).unwrap();
    assert_eq!(row.pane_command.as_deref(), Some("sleep"));

    tmux(&server.0, &["send-keys", "-t", SESSION, "C-c"]);
    wait_for_command(&client, "sh").await;
}

/// Before #24 a restart behind a proxy never saw the shell come back,
/// so it sat out the whole 3.5 s shell timeout before typing.
#[tokio::test(flavor = "current_thread")]
async fn restart_behind_the_proxy_sees_the_shell_and_the_new_command() {
    let server = Server::start("restart");
    let client = TokioTmuxClient::with_socket(server.0.clone());
    wait_for_command(&client, "sh").await;
    tmux(
        &server.0,
        &["send-keys", "-t", SESSION, "sleep 300", "Enter"],
    );
    wait_for_command(&client, "sleep").await;

    let started = Instant::now();
    client
        .restart_in_place(SESSION, "sleep 301", true, true)
        .await
        .expect("restart ok");
    let took = started.elapsed();
    assert!(took < Duration::from_millis(2500), "restart took {took:?}");
    wait_for_command(&client, "sleep").await;

    // A launch into the idle shell (the deferred-launch half) doesn't
    // wait out the timeout either.
    tmux(&server.0, &["send-keys", "-t", SESSION, "C-c"]);
    wait_for_command(&client, "sh").await;
    let started = Instant::now();
    client
        .restart_in_place(SESSION, "sleep 302", true, false)
        .await
        .expect("launch ok");
    let took = started.elapsed();
    assert!(took < Duration::from_millis(2500), "launch took {took:?}");
    wait_for_command(&client, "sleep").await;
}
