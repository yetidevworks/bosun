//! Esc typed into the embed must reach the pane as soon as tmux's
//! `escape-time` allows (issue #22). tmux holds a lone Esc for 500 ms
//! while any query it sent its client terminal is unanswered, so this
//! drives the real `EmbedTerminal` and its query answers.
#![cfg(feature = "tmux-it")]

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bosun::events::AppMsg;
use bosun::terminal_query::DefaultColors;
use bosun::ui::embed_terminal::{AttachMode, EmbedTerminal};
use tokio::sync::mpsc;

static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);

struct Server {
    socket: String,
    dir: tempfile::TempDir,
}

impl Server {
    fn new() -> Self {
        let socket = format!(
            "bosun-esc-{}-{}-{}",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let s = Self {
            socket,
            dir: tempfile::tempdir().unwrap(),
        };
        let script = s.dir.path().join("record.py");
        std::fs::write(
            &script,
            r#"import os, pathlib, tty
root = pathlib.Path(__file__).parent
tty.setraw(0)
(root / 'ready').touch()
with (root / 'input').open('ab', buffering=0) as out:
    while True:
        out.write(os.read(0, 4096))
"#,
        )
        .unwrap();
        // TempDir paths are generated locally; single-quote for spaces on macOS.
        let command = format!("python3 '{}'", script.display());
        s.tmux(&[
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "sink",
            "-x",
            "100",
            "-y",
            "30",
            &command,
        ]);
        s.tmux(&["set-option", "-s", "escape-time", "0"]);
        wait_for(Duration::from_secs(5), || {
            s.dir.path().join("ready").exists()
        });
        s
    }

    fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .arg("-L")
            .arg(&self.socket)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "tmux {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn input(&self) -> Vec<u8> {
        std::fs::read(self.dir.path().join("input")).unwrap_or_default()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-L")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
    }
}

fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    done()
}

/// Feed whatever the embed's reader thread delivered back into it, the
/// way the app's main loop does, so its query answers go out.
fn pump(embed: &mut EmbedTerminal, rx: &mut mpsc::UnboundedReceiver<AppMsg>, for_: Duration) {
    let end = Instant::now() + for_;
    while Instant::now() < end {
        while let Ok(msg) = rx.try_recv() {
            if let AppMsg::EmbedBytes { bytes, .. } = msg {
                embed.feed(&bytes);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn esc_reaches_the_pane_without_the_query_delay() {
    let server = Server::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let colors = DefaultColors {
        fg: (0, 0, 0),
        bg: (0xffff, 0xffff, 0xffff),
        cursor: (0, 0, 0),
    };
    let mut embed = EmbedTerminal::spawn(
        Some(&server.socket),
        "sink",
        30,
        100,
        AttachMode::Focused,
        None,
        colors,
        tx,
    )
    .unwrap();
    // Long enough for tmux's startup queries and our answers, well
    // short of the 5 s after which tmux gives up on most of them.
    pump(&mut embed, &mut rx, Duration::from_millis(800));
    assert!(
        server
            .tmux(&["list-clients", "-F", "#{client_termtype}"])
            .contains("bosun"),
        "tmux never got the XTVERSION answer"
    );

    for _ in 0..3 {
        let before = server.input().len();
        embed.write(b"\x1b").unwrap();
        let sent = Instant::now();
        let arrived = wait_for(Duration::from_millis(400), || {
            pump(&mut embed, &mut rx, Duration::ZERO);
            server.input().len() > before
        });
        let took = sent.elapsed();
        assert!(arrived, "Esc never reached the pane");
        assert!(
            took < Duration::from_millis(250),
            "Esc took {took:?}: tmux is still waiting on a query"
        );
        assert_eq!(&server.input()[before..], b"\x1b");
        pump(&mut embed, &mut rx, Duration::from_millis(100));
    }
}
