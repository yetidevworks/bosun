//! End to end: `bosun serve` against a throwaway tmux socket, with a
//! throwaway HOME so the config, store and daemon socket never touch
//! the real ones. A plain blocking client speaks raw JSON frames, the
//! way a GUI would.

#![cfg(feature = "tmux-it")]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_bosun");

/// How long any wait for the daemon, tmux or the TUI may take before
/// the test fails. Generous: a loaded 2-core CI runner can take seconds
/// for what takes milliseconds here, and every wait ends as soon as its
/// condition holds.
const WAIT: Duration = Duration::from_secs(30);

/// A tmux socket name no other fixture uses. The tests in this file run
/// in parallel threads of one process, so the pid alone doesn't tell
/// them apart, and neither does the clock: macOS's `SystemTime` only
/// moves in whole microseconds, and two fixtures created in the same
/// microsecond used to share a tmux server (one test's teardown killed
/// the other's sessions). The counter makes each name unique within the
/// process; pid and time keep it unique across runs.
fn unique_socket() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("bosun-serve-it-{}-{}-{}", std::process::id(), n, nanos)
}

/// The daemon process, its tmux server and its HOME, torn down on drop.
struct Fixture {
    home: tempfile::TempDir,
    sock: String,
    child: Option<Child>,
}

impl Fixture {
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env("HOME", self.home.path())
            .env("BOSUN_TMUX_SOCKET", &self.sock)
            .env_remove("TMUX")
            .stdin(Stdio::null());
        for var in [
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "BOSUN_PREFIX",
            "BOSUN_THEME",
            "BOSUN_DEFAULT_AGENT",
            "BOSUN_REMOVE_DEAD_SESSIONS",
            "BOSUN_SHOW_GROUP_IN_TITLE",
            "BOSUN_HIDE_SINGLE_TAB_STRIP",
            "BOSUN_EMBED",
            "BOSUN_TMUX_CONTROL",
        ] {
            c.env_remove(var);
        }
        c
    }

    fn socket_path(&self) -> PathBuf {
        let out = self
            .command(&["serve", "--print-socket"])
            .output()
            .expect("print socket");
        PathBuf::from(String::from_utf8(out.stdout).unwrap().trim())
    }

    /// `config.toml` under the fixture's HOME. On macOS Bosun's config
    /// and data directories are the same (`~/Library/Application
    /// Support/dev.yetidevworks.bosun`); elsewhere it's `~/.config/bosun`
    /// (the fixture clears `XDG_CONFIG_HOME`).
    fn config_path(&self) -> PathBuf {
        let dir = if cfg!(target_os = "macos") {
            self.socket_path().parent().unwrap().to_path_buf()
        } else {
            self.home.path().join(".config").join("bosun")
        };
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    fn start(&mut self) -> PathBuf {
        let path = self.socket_path();
        // The idle timer runs from startup until the first client
        // connects, so leave room for a slow first connect. The daemon
        // is killed on drop either way.
        self.child = Some(
            self.command(&["serve", "--idle-timeout", "30"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn bosun serve"),
        );
        path
    }

    fn tmux(&self, args: &[&str]) -> std::process::Output {
        Command::new("tmux")
            .arg("-L")
            .arg(&self.sock)
            .args(args)
            .output()
            .expect("tmux")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = self.tmux(&["kill-server"]);
    }
}

struct Conn {
    stream: UnixStream,
    last_seq: Option<u64>,
}

impl Conn {
    fn connect(path: &PathBuf) -> Self {
        let deadline = Instant::now() + WAIT;
        loop {
            match UnixStream::connect(path) {
                Ok(stream) => {
                    stream.set_read_timeout(Some(WAIT)).unwrap();
                    return Self {
                        stream,
                        last_seq: None,
                    };
                }
                Err(e) => {
                    assert!(Instant::now() < deadline, "daemon never listened: {e}");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    fn send(&mut self, v: Value) {
        let body = serde_json::to_vec(&v).unwrap();
        self.stream
            .write_all(&(body.len() as u32).to_be_bytes())
            .unwrap();
        self.stream.write_all(&body).unwrap();
    }

    /// Next message, or `None` at EOF. Checks that every `seq` is
    /// exactly one more than the last.
    fn recv(&mut self) -> Option<Value> {
        let mut len = [0u8; 4];
        if self.stream.read_exact(&mut len).is_err() {
            return None;
        }
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
        self.stream.read_exact(&mut body).unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        if let Some(seq) = v.get("seq").and_then(Value::as_u64) {
            if v["t"] != "snapshot" {
                if let Some(last) = self.last_seq {
                    assert_eq!(seq, last + 1, "seq gap before {v}");
                }
            }
            self.last_seq = Some(seq);
        }
        Some(v)
    }

    /// Read until `pred` matches, returning everything seen.
    fn until(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let v = self
                .recv()
                .unwrap_or_else(|| panic!("EOF waiting for {what}; saw {seen:#?}"));
            let done = pred(&v);
            seen.push(v);
            if done {
                return seen;
            }
        }
    }
}

fn is_ack(v: &Value, id: u64) -> bool {
    v["t"] == "ack" && v["id"] == id
}

#[test]
fn serve_hello_snapshot_create_delta_kill() {
    let home = tempfile::Builder::new()
        .prefix("bsv")
        .tempdir_in("/tmp")
        .unwrap();
    let mut fx = Fixture {
        home,
        sock: unique_socket(),
        child: None,
    };
    let path = fx.socket_path();
    assert!(
        path.starts_with(fx.home.path()),
        "socket {path:?} outside HOME"
    );
    assert_eq!(path.file_name().unwrap(), "bosun.sock");

    fx.child = Some(
        fx.command(&["serve", "--idle-timeout", "3"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn bosun serve"),
    );
    let mut c = Conn::connect(&path);

    // A second daemon refuses to start.
    let second = fx.command(&["serve"]).output().expect("second serve");
    assert!(!second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("already running"),
        "{second:?}"
    );

    // hello → snapshot
    c.send(json!({"t": "hello", "protocol": 1, "client": "integration-test"}));
    let snap = c.recv().expect("snapshot");
    assert_eq!(snap["t"], "snapshot", "{snap}");
    assert_eq!(snap["server"]["protocol"], 1);
    assert_eq!(snap["server"]["tmux_socket"], fx.sock.as_str());
    assert!(snap["sessions"].is_array());
    assert!(snap["statuses"].is_object());
    assert!(snap["sidebar"]["ungrouped"].is_array());
    assert!(snap["recents"].is_array());
    let settings = snap["config"]["settings"].as_array().unwrap();
    let theme_row = settings.iter().find(|s| s["key"] == "theme").unwrap();
    assert_eq!(theme_row["read_only"], false);
    assert!(settings
        .iter()
        .any(|s| s["key"] == "tmux_socket" && s["read_only"] == true));
    assert_eq!(snap["theme"]["name"], "opencode");
    assert!(snap["theme"]["status_done"]
        .as_str()
        .unwrap()
        .starts_with('#'));
    let themes = snap["themes"].as_array().unwrap();
    assert!(themes.len() >= 15);
    assert!(themes
        .iter()
        .all(|t| t["source"] == "bundled" || t["source"] == "user"));

    // Wrong protocol on another connection: readable error, then EOF.
    let mut bad = Conn::connect(&path);
    bad.send(json!({"t": "hello", "protocol": 99}));
    let err = bad.recv().expect("error frame");
    assert_eq!(err["t"], "error");
    assert_eq!(err["code"], "protocol_mismatch");
    assert!(err["message"].as_str().unwrap().contains("update bosun"));
    assert!(bad.recv().is_none(), "connection should close");

    // create → ack with the session name, and a sessions delta with it.
    c.send(json!({
        "t": "cmd", "id": 1, "op": "create",
        "spec": {"name": "itest", "path": "/tmp", "agent": "terminal"}
    }));
    let seen = c.until("create ack", |v| is_ack(v, 1));
    let ack = seen.last().unwrap();
    assert_eq!(ack["ok"], true, "{ack}");
    let name = ack["session"].as_str().expect("session in ack").to_string();
    assert!(name.starts_with("bosun-itest-"), "{name}");
    let has_row = |seen: &[Value]| {
        seen.iter().any(|v| {
            v["t"] == "sessions"
                && v["upsert"]
                    .as_array()
                    .is_some_and(|u| u.iter().any(|s| s["name"] == name.as_str()))
        })
    };
    if !has_row(&seen) {
        let more = c.until("sessions delta", |v| v["t"] == "sessions");
        assert!(has_row(&more), "{more:#?}");
    }
    let ls = fx.tmux(&["list-sessions", "-F", "#{session_name}"]);
    assert!(String::from_utf8_lossy(&ls.stdout)
        .lines()
        .any(|l| l == name));

    // rename → the row's display changes.
    c.send(json!({"t": "cmd", "id": 2, "op": "rename", "session": name, "display": "Renamed"}));
    let seen = c.until("rename delta", |v| {
        v["t"] == "sessions"
            && v["upsert"]
                .as_array()
                .is_some_and(|u| u.iter().any(|s| s["display"] == "Renamed"))
    });
    if !seen.iter().any(|v| is_ack(v, 2)) {
        let more = c.until("rename ack", |v| is_ack(v, 2));
        assert_eq!(more.last().unwrap()["ok"], true);
    }

    // A command the daemon refuses up front.
    c.send(json!({"t": "cmd", "id": 3, "op": "rename", "session": name, "display": "a\nb"}));
    let seen = c.until("bad rename ack", |v| is_ack(v, 3));
    assert_eq!(seen.last().unwrap()["ok"], false);

    // kill → ack, and the row is removed.
    c.send(json!({"t": "cmd", "id": 4, "op": "kill", "session": name}));
    let mut seen = c.until("kill ack", |v| is_ack(v, 4));
    assert_eq!(seen.last().unwrap()["ok"], true);
    let removed = |seen: &[Value]| {
        seen.iter().any(|v| {
            v["t"] == "sessions"
                && v["removed"]
                    .as_array()
                    .is_some_and(|r| r.iter().any(|s| s == name.as_str()))
        })
    };
    if !removed(&seen) {
        seen = c.until("removal delta", |v| v["t"] == "sessions");
        assert!(removed(&seen), "{seen:#?}");
    }
    let ls = fx.tmux(&["list-sessions", "-F", "#{session_name}"]);
    assert!(!String::from_utf8_lossy(&ls.stdout)
        .lines()
        .any(|l| l == name));

    // Last client gone → the daemon exits after its idle timeout and
    // removes its socket.
    drop(c);
    let mut child = fx.child.take().unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "daemon exit: {status:?}");
            break;
        }
        assert!(Instant::now() < deadline, "daemon did not exit when idle");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!path.exists(), "socket left behind");
}

fn fixture() -> Fixture {
    let home = tempfile::Builder::new()
        .prefix("bsv")
        .tempdir_in("/tmp")
        .unwrap();
    Fixture {
        home,
        sock: unique_socket(),
        child: None,
    }
}

/// Write `config.toml` so its mtime moves even on a filesystem with
/// coarse timestamps; the daemon notices a changed mtime.
fn write_config(path: &PathBuf, contents: &str) {
    let before = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    loop {
        std::fs::write(path, contents).unwrap();
        let after = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if after != before {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn serve_keybindings_follow_config_toml() {
    let mut fx = fixture();
    let config = fx.config_path();
    write_config(
        &config,
        "[keybindings]\nnext_tab = 'ctrl+shift+right'\nprevious_tab = 'none'\n",
    );
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    let snap = c.recv().expect("snapshot");
    assert_eq!(snap["t"], "snapshot", "{snap}");
    assert_eq!(
        snap["keybindings"],
        json!({
            "previous_tab": null, "next_tab": "Ctrl+Shift+Right",
            "previous_session": "Shift+Up", "next_session": "Shift+Down",
            "send_next_key": "Ctrl+v"
        })
    );

    // An edit made elsewhere arrives as a `keybindings` delta.
    write_config(&config, "[keybindings]\nnext_session = 'Alt+n'\n");
    let seen = c.until("keybindings delta", |v| v["t"] == "keybindings");
    assert_eq!(
        seen.last().unwrap()["keybindings"],
        json!({
            "previous_tab": "Shift+Left", "next_tab": "Shift+Right",
            "previous_session": "Shift+Up", "next_session": "Alt+n",
            "send_next_key": "Ctrl+v"
        })
    );

    // An invalid table falls back to the defaults, as the TUI does.
    write_config(&config, "[keybindings]\nnext_tab = 'Ctrl+q'\n");
    let seen = c.until("keybindings delta", |v| v["t"] == "keybindings");
    assert_eq!(
        seen.last().unwrap()["keybindings"],
        json!({
            "previous_tab": "Shift+Left", "next_tab": "Shift+Right",
            "previous_session": "Shift+Up", "next_session": "Shift+Down",
            "send_next_key": "Ctrl+v"
        })
    );

    // A config edit that leaves the shortcuts alone sends no delta: the
    // next one to arrive is for the edit after it.
    write_config(&config, "theme = 'nord'\n");
    let seen = c.until("theme delta", |v| v["t"] == "theme");
    assert!(!seen.iter().any(|v| v["t"] == "keybindings"), "{seen:#?}");
    write_config(
        &config,
        "theme = 'nord'\n[keybindings]\nnext_tab = 'Alt+Right'\n",
    );
    let seen = c.until("keybindings delta", |v| v["t"] == "keybindings");
    assert_eq!(
        seen.last().unwrap()["keybindings"]["next_tab"],
        "Alt+Right",
        "{seen:#?}"
    );
}

/// `editor` (what the TUI's `e` key runs, and what the Mac app opens
/// file links in): in the settings, set and unset through
/// `apply_setting`, and picked up from an edit made elsewhere.
#[test]
fn serve_editor_setting() {
    let mut fx = fixture();
    let config = fx.config_path();
    write_config(&config, "editor = 'zed'\n");
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    let snap = c.recv().expect("snapshot");
    let editor = |config: &Value| -> Value {
        config["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["key"] == "editor")
            .unwrap_or_else(|| panic!("no editor in {config}"))["value"]
            .clone()
    };
    assert_eq!(editor(&snap["config"]), "zed");

    let seen = command(
        &mut c,
        1,
        json!({"op": "apply_setting", "key": "editor", "value": " code "}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    let cfg = seen
        .iter()
        .find(|v| v["t"] == "config")
        .expect("config delta");
    assert_eq!(editor(&cfg["config"]), "code");
    let body = std::fs::read_to_string(&config).unwrap();
    assert!(body.contains("editor = \"code\""), "{body}");

    let seen = command(
        &mut c,
        2,
        json!({"op": "apply_setting", "key": "editor", "value": null}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    let cfg = seen
        .iter()
        .find(|v| v["t"] == "config")
        .expect("config delta");
    assert_eq!(editor(&cfg["config"]), Value::Null);
    let body = std::fs::read_to_string(&config).unwrap();
    assert!(!body.contains("editor"), "{body}");

    for (id, value) in [(3, json!(true)), (4, json!("zed\u{7}"))] {
        let seen = command(
            &mut c,
            id,
            json!({"op": "apply_setting", "key": "editor", "value": value}),
        );
        assert_eq!(seen.last().unwrap()["ok"], false, "{seen:#?}");
    }

    write_config(&config, "editor = 'subl'\n");
    let seen = c.until("config delta", |v| {
        v["t"] == "config" && editor(&v["config"]) == "subl"
    });
    assert!(!seen.is_empty());
}

/// The Mac app's Light/Dark pair: palettes in the snapshot and in
/// `theme_pair` deltas, set through `apply_setting` and by editing
/// `config.toml`, and never touching `theme` (the TUI's).
#[test]
fn serve_theme_pair_palettes_and_settings() {
    let mut fx = fixture();
    let config = fx.config_path();
    write_config(&config, "theme = 'nord'\ntheme_light = 'github-light'\n");
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    let snap = c.recv().expect("snapshot");
    assert_eq!(snap["t"], "snapshot", "{snap}");
    assert_eq!(snap["theme"]["name"], "nord");
    assert_eq!(snap["theme_light"]["name"], "github-light");
    assert!(snap["theme_light"]["bg"].as_str().unwrap().starts_with('#'));
    assert!(snap.get("theme_dark").is_none(), "{snap}");
    let setting = |config: &Value, key: &str| -> Value {
        config["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["key"] == key)
            .unwrap_or_else(|| panic!("no {key} in {config}"))
            .clone()
    };
    let light = setting(&snap["config"], "theme_light");
    assert_eq!(light["value"], "github-light");
    assert_eq!(light["read_only"], false);
    assert!(light["choices"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t == "tokyonight"));
    assert_eq!(setting(&snap["config"], "theme_dark")["value"], Value::Null);

    // Set the dark side: a theme_pair delta with both palettes, a config
    // delta, and `theme` untouched in the daemon and on disk.
    let seen = command(
        &mut c,
        1,
        json!({"op": "apply_setting", "key": "theme_dark", "value": "tokyonight"}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    let pair = seen
        .iter()
        .find(|v| v["t"] == "theme_pair")
        .unwrap_or_else(|| panic!("no theme_pair delta: {seen:#?}"));
    assert_eq!(pair["theme_light"]["name"], "github-light");
    assert_eq!(pair["theme_dark"]["name"], "tokyonight");
    let cfg = seen
        .iter()
        .find(|v| v["t"] == "config")
        .expect("config delta");
    assert_eq!(setting(&cfg["config"], "theme_dark")["value"], "tokyonight");
    assert!(!seen.iter().any(|v| v["t"] == "theme"), "{seen:#?}");
    let body = std::fs::read_to_string(&config).unwrap();
    assert!(body.contains("theme = \"nord\""), "{body}");
    assert!(body.contains("theme_dark = \"tokyonight\""), "{body}");

    // Unset the light side with null.
    let seen = command(
        &mut c,
        2,
        json!({"op": "apply_setting", "key": "theme_light", "value": null}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    let pair = seen.iter().find(|v| v["t"] == "theme_pair").expect("delta");
    assert!(pair.get("theme_light").is_none(), "{pair}");
    assert_eq!(pair["theme_dark"]["name"], "tokyonight");
    let body = std::fs::read_to_string(&config).unwrap();
    assert!(!body.contains("theme_light"), "{body}");

    // get_theme reads any theme's palette for a preview, and changes nothing.
    let before = std::fs::read_to_string(&config).unwrap();
    let seen = command(
        &mut c,
        20,
        json!({"op": "get_theme", "name": "flexoki-light"}),
    );
    let ack = seen.last().unwrap();
    assert_eq!(ack["ok"], true, "{seen:#?}");
    assert_eq!(ack["theme"]["name"], "flexoki-light", "{ack}");
    assert!(
        ack["theme"]["bg"].as_str().unwrap().starts_with('#'),
        "{ack}"
    );
    assert_eq!(
        ack["theme"]["terminal"]["palette"]
            .as_array()
            .map(|p| p.len()),
        Some(16),
        "{ack}"
    );
    assert!(
        !seen
            .iter()
            .any(|v| v["t"] == "theme" || v["t"] == "theme_pair"),
        "{seen:#?}"
    );
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    let seen = command(
        &mut c,
        21,
        json!({"op": "get_theme", "name": "no-such-theme"}),
    );
    assert_eq!(seen.last().unwrap()["ok"], false, "{seen:#?}");

    // A name no theme has, or a non-string, is refused.
    for (id, value) in [(3, json!("no-such-theme")), (4, json!(true))] {
        let seen = command(
            &mut c,
            id,
            json!({"op": "apply_setting", "key": "theme_light", "value": value}),
        );
        assert_eq!(seen.last().unwrap()["ok"], false, "{seen:#?}");
    }

    // Changing `theme` keeps the pair in config.toml.
    let seen = command(
        &mut c,
        5,
        json!({"op": "set_theme", "name": "dracula", "persist": true}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    assert!(!seen.iter().any(|v| v["t"] == "theme_pair"), "{seen:#?}");
    let body = std::fs::read_to_string(&config).unwrap();
    assert!(body.contains("theme = \"dracula\""), "{body}");
    assert!(body.contains("theme_dark = \"tokyonight\""), "{body}");

    // An edit made elsewhere arrives as a theme_pair delta.
    write_config(
        &config,
        "theme = 'dracula'\ntheme_light = 'one-light'\ntheme_dark = 'tokyonight'\n",
    );
    let seen = c.until("theme_pair delta", |v| v["t"] == "theme_pair");
    let pair = seen.last().unwrap();
    assert_eq!(pair["theme_light"]["name"], "one-light");
    assert_eq!(pair["theme_dark"]["name"], "tokyonight");
    assert!(!seen.iter().any(|v| v["t"] == "theme"), "{seen:#?}");
}

/// A deferred create keeps the spec's one-shot `resume` for its launch:
/// the session's metadata doesn't store it, so `pending_launch` carries
/// it to the client, which passes it back with `launch_agent`.
#[test]
fn serve_deferred_create_keeps_resume() {
    let mut fx = fixture();
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    assert_eq!(c.recv().expect("snapshot")["t"], "snapshot");
    for (id, resume) in [(1, true), (2, false)] {
        c.send(json!({
            "t": "cmd", "id": id, "op": "create", "defer_launch": true,
            "spec": {"name": format!("resume-{resume}"), "path": "/tmp", "agent": "claude",
                     "resume": resume}
        }));
        let seen = c.until("create ack", |v| is_ack(v, id));
        let ack = seen.last().unwrap();
        assert_eq!(ack["ok"], true, "{ack}");
        let launch = &ack["pending_launch"];
        assert_eq!(launch["session"], ack["session"], "{ack}");
        if resume {
            assert_eq!(launch["resume"], true, "{ack}");
        } else {
            assert!(launch.get("resume").is_none_or(|r| r.is_null()), "{ack}");
        }
        let name = ack["session"].as_str().unwrap().to_string();
        fx.tmux(&["kill-session", "-t", &name]);
    }
}

#[test]
fn serve_hook_state_reaches_clients_within_the_poll() {
    let mut fx = fixture();
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    assert_eq!(c.recv().expect("snapshot")["t"], "snapshot");

    // A session whose foreground process isn't a shell, the way an
    // agent's is (hook state is ignored at a bare shell). The process is
    // started with send-keys rather than `launch_agent`: the actor spends
    // about 2 s after a launch refreshing on its own schedule, and the
    // poll waits for that, which isn't what this test measures.
    c.send(json!({
        "t": "cmd", "id": 1, "op": "create", "defer_launch": true,
        "spec": {"name": "hooked", "path": "/tmp", "agent": "terminal"}
    }));
    let seen = c.until("create ack", |v| is_ack(v, 1));
    let ack = seen.last().unwrap();
    assert_eq!(ack["ok"], true, "{ack}");
    let name = ack["session"].as_str().unwrap().to_string();
    fx.tmux(&["send-keys", "-t", &name, "exec sleep 600", "Enter"]);
    let running_sleep = |v: &Value| {
        v["t"] == "sessions"
            && v["upsert"].as_array().is_some_and(|u| {
                u.iter()
                    .any(|s| s["name"] == name.as_str() && s["pane_command"] == "sleep")
            })
    };
    if !seen.iter().any(running_sleep) {
        c.until("sleep in the pane", running_sleep);
    }

    let display = |format: &str| {
        let out = fx.tmux(&["display", "-p", "-t", &name, format]);
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    let pane = display("#{pane_id}");
    let server = display("#{socket_path},#{pid},0");

    // Each report lands well inside the 1 s refresh: the 250 ms status
    // poll picks it up. Five in a row all under 600 ms would happen by
    // chance about 8 % of the time on a 1 s cadence. Timed from the
    // hook's exit (the pane options are written by then), so starting
    // the hook process on a loaded machine doesn't count.
    let mut took = Vec::new();
    for i in 0..5u64 {
        let tool = format!("Tool{i}");
        let mut hook = Command::new(BIN)
            .args(["hook", "claude", "PermissionRequest"])
            .env("TMUX", &server)
            .env("TMUX_PANE", &pane)
            .env_remove("BOSUN_HOOK_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn bosun hook");
        hook.stdin
            .take()
            .unwrap()
            .write_all(json!({ "tool_name": tool }).to_string().as_bytes())
            .unwrap();
        assert!(hook.wait().unwrap().success());
        let start = Instant::now();
        let detail = format!("permission: {tool}");
        c.until("hook status delta", |v| {
            v["t"] == "status"
                && v["session"] == name.as_str()
                && v["state"] == "waiting"
                && v["source"] == "hook"
                && v["detail"] == detail.as_str()
        });
        took.push(start.elapsed());
        // Land the next report at a different point in the poll cycle.
        std::thread::sleep(Duration::from_millis(150 + 70 * i));
    }
    assert!(
        took.iter().all(|t| *t < Duration::from_millis(600)),
        "hook → delta took {took:?}"
    );

    c.send(json!({"t": "cmd", "id": 2, "op": "kill", "session": name}));
    c.until("kill ack", |v| is_ack(v, 2));
}

/// Run `bosun hook claude <event>` as Claude Code would from inside
/// `session`'s pane, with `payload` on stdin.
fn run_claude_hook(fx: &Fixture, session: &str, event: &str, payload: Value) {
    let display = |format: &str| {
        let out = fx.tmux(&["display", "-p", "-t", session, format]);
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    let mut hook = Command::new(BIN)
        .args(["hook", "claude", event])
        .env("TMUX", display("#{socket_path},#{pid},0"))
        .env("TMUX_PANE", display("#{pane_id}"))
        .env_remove("BOSUN_HOOK_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bosun hook");
    hook.stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    assert!(hook.wait().unwrap().success());
}

#[test]
fn serve_hook_done_carries_done_at_until_the_next_prompt() {
    let mut fx = fixture();
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    assert_eq!(c.recv().expect("snapshot")["t"], "snapshot");

    // A non-shell foreground process, as in the latency test above.
    c.send(json!({
        "t": "cmd", "id": 1, "op": "create", "defer_launch": true,
        "spec": {"name": "turns", "path": "/tmp", "agent": "terminal"}
    }));
    let seen = c.until("create ack", |v| is_ack(v, 1));
    let ack = seen.last().unwrap();
    assert_eq!(ack["ok"], true, "{ack}");
    let name = ack["session"].as_str().unwrap().to_string();
    fx.tmux(&["send-keys", "-t", &name, "exec sleep 600", "Enter"]);
    let running_sleep = |v: &Value| {
        v["t"] == "sessions"
            && v["upsert"].as_array().is_some_and(|u| {
                u.iter()
                    .any(|s| s["name"] == name.as_str() && s["pane_command"] == "sleep")
            })
    };
    if !seen.iter().any(running_sleep) {
        c.until("sleep in the pane", running_sleep);
    }
    let pane_at = || {
        let out = fx.tmux(&["show-options", "-pqv", "-t", &name, "@bosun_state_at"]);
        String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .parse::<u64>()
            .expect("@bosun_state_at")
    };
    let done_delta = |v: &Value| {
        v["t"] == "status"
            && v["session"] == name.as_str()
            && v["source"] == "hook"
            && v["done_at"].is_u64()
    };

    // The turn finishes: `idle` from the hook, with `done_at` set to the
    // hook's own timestamp.
    run_claude_hook(&fx, &name, "Stop", json!({"stop_hook_active": false}));
    let first_at = pane_at();
    let seen = c.until("status with done_at", done_delta);
    let first = seen.last().unwrap();
    assert_eq!(first["state"], "idle", "{first}");
    assert_eq!(first["done_at"].as_u64(), Some(first_at), "{first}");

    // A snapshot taken now has it too.
    let mut c2 = Conn::connect(&path);
    c2.send(json!({"t": "hello", "protocol": 1}));
    let snap = c2.recv().expect("snapshot");
    assert_eq!(snap["statuses"][&name]["done_at"].as_u64(), Some(first_at));
    drop(c2);

    // The next prompt clears it.
    run_claude_hook(&fx, &name, "UserPromptSubmit", json!({"prompt": "go on"}));
    let seen = c.until("running from the hook", |v| {
        v["t"] == "status" && v["session"] == name.as_str() && v["state"] == "running"
    });
    let running = seen.last().unwrap();
    assert_eq!(running["source"], "hook", "{running}");
    assert!(running.get("done_at").is_none(), "{running}");

    // A second turn finishing gives a newer `done_at`.
    std::thread::sleep(Duration::from_millis(20));
    run_claude_hook(&fx, &name, "Stop", json!({"stop_hook_active": false}));
    let second_at = pane_at();
    assert!(second_at > first_at);
    let seen = c.until("second done_at", |v| {
        done_delta(v) && v["done_at"].as_u64() == Some(second_at)
    });
    assert_eq!(seen.last().unwrap()["state"], "idle");

    c.send(json!({"t": "cmd", "id": 2, "op": "kill", "session": name}));
    c.until("kill ack", |v| is_ack(v, 2));
}

#[test]
fn serve_background_agents_keep_the_turn_going_until_they_report_back() {
    let mut fx = fixture();
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    assert_eq!(c.recv().expect("snapshot")["t"], "snapshot");

    // A non-shell foreground process, as in the tests above.
    c.send(json!({
        "t": "cmd", "id": 1, "op": "create", "defer_launch": true,
        "spec": {"name": "bg", "path": "/tmp", "agent": "terminal"}
    }));
    let seen = c.until("create ack", |v| is_ack(v, 1));
    let ack = seen.last().unwrap();
    assert_eq!(ack["ok"], true, "{ack}");
    let name = ack["session"].as_str().unwrap().to_string();
    fx.tmux(&["send-keys", "-t", &name, "exec sleep 600", "Enter"]);
    let running_sleep = |v: &Value| {
        v["t"] == "sessions"
            && v["upsert"].as_array().is_some_and(|u| {
                u.iter()
                    .any(|s| s["name"] == name.as_str() && s["pane_command"] == "sleep")
            })
    };
    if !seen.iter().any(running_sleep) {
        c.until("sleep in the pane", running_sleep);
    }
    let status = |v: &Value| v["t"] == "status" && v["session"] == name.as_str();

    run_claude_hook(
        &fx,
        &name,
        "UserPromptSubmit",
        json!({"prompt": "build it"}),
    );
    let seen = c.until("running with a start", |v| {
        status(v) && v["state"] == "running" && v["started_at"].is_u64()
    });
    let started = seen.last().unwrap()["started_at"].as_u64().unwrap();
    std::thread::sleep(Duration::from_millis(600));

    // The turn ends with an agent still out (as Claude Code 2.1.303 sends
    // it): still running, the same start, and a note saying why.
    run_claude_hook(
        &fx,
        &name,
        "Stop",
        json!({"stop_hook_active": false, "background_tasks": [
            {"id": "a1", "type": "subagent", "status": "running", "agent_type": "general-purpose"},
            {"id": "s1", "type": "shell", "status": "running", "command": "npm run dev"}
        ], "session_crons": []}),
    );
    let seen = c.until("the background note", |v| {
        status(v) && v["detail"] == "background agent"
    });
    let bg = seen.last().unwrap();
    assert_eq!(bg["state"], "running", "{bg}");
    assert_eq!(bg["started_at"].as_u64(), Some(started), "{bg}");
    assert!(bg.get("done_at").is_none(), "{bg}");
    assert_eq!(
        String::from_utf8(
            fx.tmux(&["show-options", "-pqv", "-t", &name, "@bosun_background"])
                .stdout
        )
        .unwrap()
        .trim(),
        "1"
    );

    // The agent's own tool calls and its report back don't start a new
    // turn.
    run_claude_hook(
        &fx,
        &name,
        "PostToolUse",
        json!({"agent_id": "a1", "agent_type": "general-purpose", "tool_name": "Bash"}),
    );
    run_claude_hook(
        &fx,
        &name,
        "UserPromptSubmit",
        json!({"prompt": "<task-notification>"}),
    );
    std::thread::sleep(Duration::from_millis(600));
    let mut c2 = Conn::connect(&path);
    c2.send(json!({"t": "hello", "protocol": 1}));
    let snap = c2.recv().expect("snapshot");
    let info = &snap["statuses"][&name];
    assert_eq!(info["state"], "running", "{info}");
    assert_eq!(info["started_at"].as_u64(), Some(started), "{info}");
    assert_eq!(info["detail"], "background agent", "{info}");
    drop(c2);

    // The turn that follows the report ends with nothing out: done, and
    // the count is gone.
    run_claude_hook(
        &fx,
        &name,
        "Stop",
        json!({"stop_hook_active": false, "background_tasks": [], "session_crons": []}),
    );
    let seen = c.until("done", |v| status(v) && v["done_at"].is_u64());
    let done = seen.last().unwrap();
    assert_eq!(done["state"], "idle", "{done}");
    assert!(done.get("started_at").is_none(), "{done}");
    assert!(fx
        .tmux(&["show-options", "-pqv", "-t", &name, "@bosun_background"])
        .stdout
        .is_empty());

    c.send(json!({"t": "cmd", "id": 2, "op": "kill", "session": name}));
    c.until("kill ack", |v| is_ack(v, 2));
}

#[test]
fn serve_turn_timers_survive_a_daemon_restart() {
    let mut fx = fixture();
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    assert_eq!(c.recv().expect("snapshot")["t"], "snapshot");

    // A non-shell foreground process, as in the hook tests above.
    c.send(json!({
        "t": "cmd", "id": 1, "op": "create", "defer_launch": true,
        "spec": {"name": "long", "path": "/tmp", "agent": "terminal"}
    }));
    let seen = c.until("create ack", |v| is_ack(v, 1));
    let name = seen.last().unwrap()["session"]
        .as_str()
        .unwrap()
        .to_string();
    fx.tmux(&["send-keys", "-t", &name, "exec sleep 600", "Enter"]);
    wait_for("sleep in the pane", WAIT, || {
        let out = fx.tmux(&["display", "-p", "-t", &name, "#{pane_current_command}"]);
        String::from_utf8_lossy(&out.stdout).trim() == "sleep"
    });
    let turn_at = |fx: &Fixture| {
        let out = fx.tmux(&["show-options", "-qv", "-t", &name, "@bosun_turn_at"]);
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u64>()
            .ok()
    };

    run_claude_hook(&fx, &name, "UserPromptSubmit", json!({"prompt": "go"}));
    let seen = c.until("running with a start", |v| {
        v["t"] == "status" && v["session"] == name.as_str() && v["started_at"].is_u64()
    });
    let started = seen.last().unwrap()["started_at"].as_u64().unwrap();
    wait_for("the start kept on the session", WAIT, || {
        turn_at(&fx) == Some(started)
    });
    drop(c);

    // A new daemon, a second later: the same turn, the same start.
    std::thread::sleep(Duration::from_millis(1100));
    if let Some(mut child) = fx.child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    let snap = c.recv().expect("snapshot");
    let mut info = snap["statuses"][&name].clone();
    if !info["started_at"].is_u64() {
        let seen = c.until("the turn's start", |v| {
            v["t"] == "status" && v["session"] == name.as_str() && v["started_at"].is_u64()
        });
        info = seen.last().unwrap().clone();
    }
    assert_eq!(info["started_at"].as_u64(), Some(started), "{info}");

    // The turn ends: the stored start goes.
    run_claude_hook(&fx, &name, "Stop", json!({"stop_hook_active": false}));
    wait_for("the stored start cleared", WAIT, || turn_at(&fx).is_none());

    c.send(json!({"t": "cmd", "id": 2, "op": "kill", "session": name}));
    c.until("kill ack", |v| is_ack(v, 2));
}

/// A session whose shell exits as it starts (as with a tmux server the
/// installed tmux can't use, issue #21) gets a notice pointing at the
/// troubleshooting page instead of just vanishing; one that stays up
/// gets none.
#[test]
fn serve_warns_when_a_new_session_closes_by_itself() {
    let mut fx = fixture();
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    assert_eq!(c.recv().expect("snapshot")["t"], "snapshot");

    fx.tmux(&["set-option", "-g", "default-command", "exit 0"]);
    create_terminal(&mut c, 1, "gone", "");
    let seen = c.until("the closed notice", |v| {
        v["t"] == "notice"
            && v["message"]
                .as_str()
                .is_some_and(|m| m.contains("closed as soon as it started"))
    });
    let message = seen.last().unwrap()["message"].as_str().unwrap();
    assert!(message.starts_with("gone closed"), "{message}");
    assert!(message.contains("docs/troubleshooting.md"), "{message}");

    fx.tmux(&["set-option", "-gu", "default-command"]);
    let name = create_terminal(&mut c, 2, "stays", "");
    std::thread::sleep(Duration::from_millis(2600));
    c.send(json!({"t": "cmd", "id": 3, "op": "kill", "session": name}));
    let seen = c.until("kill ack", |v| is_ack(v, 3));
    assert!(
        !seen.iter().any(|v| v["t"] == "notice"
            && v["message"]
                .as_str()
                .is_some_and(|m| m.contains("closed as soon as it started"))),
        "{seen:?}"
    );
}

/// Create a `terminal` session through the daemon and return its tmux
/// name. `args` is the command typed into its shell (empty: none).
fn create_terminal(c: &mut Conn, id: u64, name: &str, args: &str) -> String {
    c.send(json!({
        "t": "cmd", "id": id, "op": "create",
        "spec": {"name": name, "path": "/tmp", "agent": "terminal", "args": args}
    }));
    let seen = c.until("create ack", |v| is_ack(v, id));
    let ack = seen.last().unwrap();
    assert_eq!(ack["ok"], true, "{ack}");
    ack["session"].as_str().unwrap().to_string()
}

fn wait_for(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn serve_launch_types_only_the_command() {
    let mut fx = fixture();
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    assert_eq!(c.recv().expect("snapshot")["t"], "snapshot");

    // `cat` reads its terminal in cooked mode, the way an agent's
    // terminal is until it switches to raw mode, so any key Bosun sends
    // after the launch is echoed where this test can see it (a Ctrl-L
    // shows as `^L`).
    let name = create_terminal(&mut c, 1, "launch", "cat");
    let display = |format: &str| {
        let out = fx.tmux(&["display", "-p", "-t", &name, format]);
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    wait_for("cat in the pane", WAIT, || {
        display("#{pane_current_command}") == "cat"
    });
    std::thread::sleep(Duration::from_millis(1500));
    let out = fx.tmux(&["capture-pane", "-p", "-t", &name]);
    let pane = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !pane.contains("^L"),
        "something was typed after the launch:\n{pane}"
    );
    assert!(pane.contains("cat"), "{pane}");
}

/// A terminfo directory holding `xterm-ghostty-bosun`, the terminal type
/// Bosun for Mac's terminals attach with (built from `xterm-256color`,
/// which every system has; only the name matters here).
fn app_terminfo(dir: &std::path::Path) {
    let src = Command::new("infocmp")
        .args(["-x", "xterm-256color"])
        .output()
        .expect("infocmp");
    assert!(src.status.success(), "{src:?}");
    let text = String::from_utf8(src.stdout).unwrap();
    let renamed: String = text
        .lines()
        .map(|l| {
            if l.starts_with("xterm-256color|") {
                "xterm-ghostty-bosun|bosun app test terminal,".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let file = dir.join("app.src");
    std::fs::write(&file, renamed + "\n").unwrap();
    let out = Command::new("tic")
        .args(["-x", "-o"])
        .arg(dir)
        .arg(&file)
        .output()
        .expect("tic");
    assert!(out.status.success(), "{out:?}");
}

/// A terminal client (`tmux attach` in a pty from `script`) with the
/// given TERM. Killed on drop.
struct TermClient(Child);

impl TermClient {
    fn attach(fx: &Fixture, session: &str, term: &str, terminfo: &std::path::Path) -> Self {
        let tmux = format!("tmux -L {} attach -t ={}", fx.sock, session);
        let mut cmd = Command::new("script");
        if cfg!(target_os = "macos") {
            cmd.args(["-q", "/dev/null", "sh", "-c", &tmux]);
        } else {
            cmd.args(["-qfec", &tmux, "/dev/null"]);
        }
        let child = cmd
            .env("TERM", term)
            .env("TERMINFO", terminfo)
            .env_remove("TMUX")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("script");
        Self(child)
    }
}

impl Drop for TermClient {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn serve_reports_client_moves_and_hides_the_status_line_for_app_clients() {
    let mut fx = fixture();
    let terminfo = tempfile::tempdir().unwrap();
    app_terminfo(terminfo.path());
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    assert_eq!(c.recv().expect("snapshot")["t"], "snapshot");
    let a = create_terminal(&mut c, 1, "first", "");
    let b = create_terminal(&mut c, 2, "second", "");

    let clients = || {
        let out = fx.tmux(&[
            "list-clients",
            "-F",
            "#{client_tty}|||#{client_session}|||#{client_termname}|||#{E:status-right}",
        ]);
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.split("|||").map(str::to_string).collect::<Vec<_>>())
            .collect::<Vec<_>>()
    };
    let status = |session: &str| {
        let out = fx.tmux(&["display", "-p", "-t", session, "#{status}"]);
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    wait_for("status lines on", WAIT, || {
        status(&a) == "on" && status(&b) == "on"
    });

    // The app's terminal attaches to A: reported, and A's status line goes.
    let app = TermClient::attach(&fx, &a, "xterm-ghostty-bosun", terminfo.path());
    let seen = c.until("client_session for A", |v| {
        v["t"] == "client_session" && v["session"] == a.as_str()
    });
    let tty = seen.last().unwrap()["tty"].as_str().unwrap().to_string();
    assert!(tty.starts_with("/dev/"), "{tty}");
    let row = clients()
        .into_iter()
        .find(|r| r[0] == tty)
        .expect("app client");
    assert_eq!(row[2], "xterm-ghostty-bosun");
    assert_eq!(row[3], "", "TUI hints on an app client");
    wait_for("A's status line off", WAIT, || status(&a) == "off");

    // Something moves it to B (a jump key typed inside it does the same).
    fx.tmux(&["switch-client", "-c", &tty, "-t", &format!("={b}")]);
    let seen = c.until("client_session for B", |v| v["t"] == "client_session");
    let moved = seen.last().unwrap();
    assert_eq!(moved["tty"], tty.as_str());
    assert_eq!(moved["session"], b.as_str());
    wait_for("status lines follow the move", WAIT, || {
        status(&a) == "on" && status(&b) == "off"
    });

    // Another kind of client on B brings the line back, with its hints.
    let other = TermClient::attach(&fx, &b, "xterm-256color", terminfo.path());
    wait_for("B's status line back", WAIT, || status(&b) == "on");
    let rows = clients();
    let hint = rows
        .iter()
        .find(|r| r[0] != tty && r[0].starts_with('/'))
        .map(|r| r[3].clone())
        .unwrap_or_default();
    assert!(
        hint.contains("detach"),
        "hints missing for a TUI client: {rows:?}"
    );
    drop(other);
    wait_for("B's status line off again", WAIT, || status(&b) == "off");

    // A detach is reported too.
    fx.tmux(&["detach-client", "-t", &tty]);
    // (The other client's detach, from dropping it above, comes first.)
    c.until("client_detached", |v| {
        v["t"] == "client_detached" && v["tty"] == tty.as_str()
    });
    wait_for("B's status line on with nobody attached", WAIT, || {
        status(&b) == "on"
    });
    drop(app);
}

/// Every row of a wire sidebar, ungrouped first, as the client sees it.
fn rows(sidebar: &Value) -> Vec<Value> {
    let mut out: Vec<Value> = sidebar["ungrouped"].as_array().unwrap().clone();
    for s in sidebar["sections"].as_array().unwrap() {
        out.extend(s["members"].as_array().unwrap().iter().cloned());
    }
    out
}

fn row_tabs(row: &Value) -> Vec<String> {
    row["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["session"].as_str().unwrap().to_string())
        .collect()
}

/// The id of the row holding `session` in the latest sidebar seen.
fn row_of(sidebar: &Value, session: &str) -> Option<String> {
    rows(sidebar)
        .into_iter()
        .find(|r| row_tabs(r).iter().any(|t| t == session))
        .map(|r| r["id"].as_str().unwrap().to_string())
}

/// A fresh snapshot's sidebar once it has a row for each of `sessions`.
fn sidebar_with(c: &mut Conn, sessions: &[&str]) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        c.send(json!({"t": "resync"}));
        let seen = c.until("snapshot", |v| v["t"] == "snapshot");
        let sidebar = seen.last().unwrap()["sidebar"].clone();
        if sessions.iter().all(|s| row_of(&sidebar, s).is_some()) {
            return sidebar;
        }
        assert!(Instant::now() < deadline, "rows never appeared: {sidebar}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Send a command and return everything up to and including its ack.
fn command(c: &mut Conn, id: u64, cmd: Value) -> Vec<Value> {
    let mut v = cmd;
    v["t"] = json!("cmd");
    v["id"] = json!(id);
    c.send(v);
    c.until("ack", |v| is_ack(v, id))
}

fn container_option(fx: &Fixture, session: &str) -> String {
    let out = fx.tmux(&["show-options", "-qv", "-t", session, "@bosun_container_id"]);
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[test]
fn serve_merge_rows_and_move_tab() {
    let mut fx = fixture();
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    assert_eq!(c.recv().expect("snapshot")["t"], "snapshot");
    let alpha = create_terminal(&mut c, 1, "alpha", "");
    let beta = create_terminal(&mut c, 2, "beta", "");
    let sidebar = sidebar_with(&mut c, &[&alpha, &beta]);
    let a_row = row_of(&sidebar, &alpha).unwrap();
    let b_row = row_of(&sidebar, &beta).unwrap();
    assert_ne!(a_row, b_row);

    // merge_rows: one sidebar delta, sent before the ack, with one row
    // holding both tabs and alpha still active.
    let seen = command(
        &mut c,
        10,
        json!({"op": "merge_rows", "from": b_row, "into": a_row}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    let deltas: Vec<&Value> = seen.iter().filter(|v| v["t"] == "sidebar").collect();
    assert_eq!(deltas.len(), 1, "one sidebar delta: {seen:#?}");
    let merged = deltas[0]["sidebar"].clone();
    let all = rows(&merged);
    assert_eq!(all.len(), 1, "{merged}");
    assert_eq!(all[0]["id"], a_row.as_str());
    assert_eq!(row_tabs(&all[0]), vec![alpha.clone(), beta.clone()]);
    assert_eq!(all[0]["active"], alpha.as_str());
    // The tmux option follows, for both tabs of the row.
    assert_eq!(container_option(&fx, &beta), a_row);
    assert_eq!(container_option(&fx, &alpha), a_row);
    // Both sessions are still the same tmux sessions.
    let ls = fx.tmux(&["list-sessions", "-F", "#{session_name}"]);
    let ls = String::from_utf8_lossy(&ls.stdout).into_owned();
    assert!(ls.lines().any(|l| l == alpha) && ls.lines().any(|l| l == beta));

    // Into the row it's already in: acked, nothing changes.
    let seen = command(
        &mut c,
        11,
        json!({"op": "move_tab", "session": beta, "container_id": a_row}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true);
    assert!(!seen.iter().any(|v| v["t"] == "sidebar"), "{seen:#?}");

    // Unknown session or row: refused.
    let seen = command(
        &mut c,
        12,
        json!({"op": "move_tab", "session": "bosun-nope-0", "container_id": a_row}),
    );
    assert_eq!(seen.last().unwrap()["ok"], false);
    let seen = command(
        &mut c,
        13,
        json!({"op": "move_tab", "session": beta, "container_id": "con-nope"}),
    );
    let ack = seen.last().unwrap();
    assert_eq!(ack["ok"], false);
    assert!(ack["error"].as_str().unwrap().contains("con-nope"), "{ack}");
    let seen = command(
        &mut c,
        14,
        json!({"op": "merge_rows", "from": "con-nope", "into": a_row}),
    );
    assert_eq!(seen.last().unwrap()["ok"], false);

    // The merge survives a daemon restart (config.toml).
    drop(c);
    let mut child = fx.child.take().unwrap();
    // SIGTERM, the way a user stops the daemon.
    // SAFETY: signalling a child we spawned and haven't reaped.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    child.wait().unwrap();
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    let snap = c.recv().expect("snapshot");
    let all = rows(&snap["sidebar"]);
    assert_eq!(all.len(), 1, "{}", snap["sidebar"]);
    assert_eq!(all[0]["id"], a_row.as_str());
    assert_eq!(row_tabs(&all[0]), vec![alpha.clone(), beta.clone()]);

    // move_tab with null: its own row again, right after the old one,
    // and the option points at the new row.
    let seen = command(
        &mut c,
        20,
        json!({"op": "move_tab", "session": beta, "container_id": null}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    let split = seen
        .iter()
        .rev()
        .find(|v| v["t"] == "sidebar")
        .unwrap_or_else(|| panic!("no sidebar delta: {seen:#?}"))["sidebar"]
        .clone();
    let all = rows(&split);
    assert_eq!(all.len(), 2, "{split}");
    assert_eq!(all[0]["id"], a_row.as_str());
    assert_eq!(row_tabs(&all[0]), vec![alpha.clone()]);
    assert_eq!(row_tabs(&all[1]), vec![beta.clone()]);
    let new_row = all[1]["id"].as_str().unwrap().to_string();
    assert_ne!(new_row, a_row);
    assert_eq!(container_option(&fx, &beta), new_row);

    // And back in, at the front.
    let seen = command(
        &mut c,
        21,
        json!({"op": "move_tab", "session": beta, "container_id": a_row, "index": 0}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    let back = seen
        .iter()
        .rev()
        .find(|v| v["t"] == "sidebar")
        .unwrap_or_else(|| panic!("no sidebar delta: {seen:#?}"))["sidebar"]
        .clone();
    let all = rows(&back);
    assert_eq!(all.len(), 1, "{back}");
    assert_eq!(row_tabs(&all[0]), vec![beta.clone(), alpha.clone()]);
    assert_eq!(all[0]["active"], alpha.as_str());
    assert_eq!(container_option(&fx, &beta), a_row);
}

/// `create` with a `section` puts the new row at the end of that
/// section; an unknown section is refused before anything is created.
#[test]
fn serve_create_into_section() {
    let mut fx = fixture();
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    assert_eq!(c.recv().expect("snapshot")["t"], "snapshot");
    let seen = command(&mut c, 1, json!({"op": "insert_section", "name": "work"}));
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    let alpha = create_terminal(&mut c, 2, "alpha", "");
    let sidebar = sidebar_with(&mut c, &[&alpha]);
    let section = sidebar["sections"][0]["id"].as_str().unwrap().to_string();

    let seen = command(
        &mut c,
        3,
        json!({"op": "create", "section": section,
               "spec": {"name": "beta", "path": "/tmp", "agent": "terminal"}}),
    );
    let ack = seen.last().unwrap();
    assert_eq!(ack["ok"], true, "{ack}");
    let beta = ack["session"].as_str().unwrap().to_string();
    let deadline = Instant::now() + WAIT;
    loop {
        let sidebar = sidebar_with(&mut c, &[&alpha, &beta]);
        let members = sidebar["sections"][0]["members"]
            .as_array()
            .unwrap()
            .clone();
        if members.len() == 1 {
            assert_eq!(row_tabs(&members[0]), vec![beta.clone()]);
            let ungrouped = sidebar["ungrouped"].as_array().unwrap();
            assert_eq!(ungrouped.len(), 1, "{sidebar}");
            assert_eq!(row_tabs(&ungrouped[0]), vec![alpha.clone()]);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "beta never moved into the section: {sidebar}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let seen = command(
        &mut c,
        4,
        json!({"op": "create", "section": "sec-nope",
               "spec": {"name": "gamma", "path": "/tmp", "agent": "terminal"}}),
    );
    let ack = seen.last().unwrap();
    assert_eq!(ack["ok"], false, "{ack}");
    assert!(ack["error"].as_str().unwrap().contains("sec-nope"), "{ack}");
    let ls = fx.tmux(&["list-sessions", "-F", "#{session_name}"]);
    let ls = String::from_utf8_lossy(&ls.stdout).into_owned();
    assert!(!ls.contains("gamma"), "{ls}");
}

/// Restarting an exited tab (`create` with `replaces`, as Bosun for Mac
/// does after a reboot ended every session) puts the new session in the
/// exited tab's place: same row, same spot in its section, no new row.
#[test]
fn serve_create_replacing_an_exited_tab() {
    let mut fx = fixture();
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    assert_eq!(c.recv().expect("snapshot")["t"], "snapshot");
    let seen = command(&mut c, 1, json!({"op": "insert_section", "name": "work"}));
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    let sidebar = sidebar_until(&mut c, "the section", |sb| {
        section_named(sb, "work").is_some()
    });
    let section = section_named(&sidebar, "work").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut made = Vec::new();
    for (id, name) in [(2, "alpha"), (3, "beta")] {
        let seen = command(
            &mut c,
            id,
            json!({"op": "create", "section": section,
                   "spec": {"name": name, "path": "/tmp", "agent": "terminal", "args": "sleep 600"}}),
        );
        let ack = seen.last().unwrap();
        assert_eq!(ack["ok"], true, "{ack}");
        made.push(ack["session"].as_str().unwrap().to_string());
    }
    let (alpha, beta) = (made[0].clone(), made[1].clone());
    let sb = sidebar_until(&mut c, "both rows in the section", |sb| {
        section_named(sb, "work").is_some_and(|s| s["members"].as_array().unwrap().len() == 2)
    });
    let alpha_row = row_of(&sb, &alpha).unwrap();

    // The session ends behind the daemon's back, as on a reboot.
    fx.tmux(&["kill-session", "-t", &alpha]);
    sidebar_until(&mut c, "alpha exited", |sb| {
        section_named(sb, "work").is_some_and(|s| s["members"][0]["tabs"][0]["alive"] == false)
    });

    let seen = command(
        &mut c,
        4,
        json!({"op": "create", "replaces": alpha,
               "spec": {"name": "alpha", "path": "/tmp", "agent": "terminal", "args": "sleep 600"}}),
    );
    let ack = seen.last().unwrap();
    assert_eq!(ack["ok"], true, "{ack}");
    let fresh = ack["session"].as_str().unwrap().to_string();
    let sb = sidebar_until(&mut c, "the restart in alpha's place", |sb| {
        row_of(sb, &fresh).is_some()
    });
    assert_eq!(rows(&sb).len(), 2, "no new row: {sb}");
    let members = section_named(&sb, "work").unwrap()["members"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(members[0]["id"], alpha_row.as_str(), "{sb}");
    assert_eq!(row_tabs(&members[0]), vec![fresh.clone()], "{sb}");
    assert_eq!(row_tabs(&members[1]), vec![beta.clone()], "{sb}");
    assert_eq!(container_option(&fx, &fresh), alpha_row);

    // `replaces` naming a live session is ignored: an ordinary create.
    let seen = command(
        &mut c,
        5,
        json!({"op": "create", "replaces": beta,
               "spec": {"name": "gamma", "path": "/tmp", "agent": "terminal"}}),
    );
    let ack = seen.last().unwrap();
    assert_eq!(ack["ok"], true, "{ack}");
    let gamma = ack["session"].as_str().unwrap().to_string();
    let sb = sidebar_with(&mut c, &[&gamma]);
    assert_eq!(rows(&sb).len(), 3, "{sb}");
    assert_eq!(row_tabs(&rows(&sb)[0]), vec![gamma], "{sb}");
    assert!(row_of(&sb, &beta).is_some(), "{sb}");
}

/// `search` walks a session's whole history in tmux's copy mode: typing
/// refines one search from the bottom, `older`/`newer` step through the
/// matches, and `end` puts the pane back at its live output.
#[test]
fn serve_search_walks_the_history() {
    let mut fx = fixture();
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    assert_eq!(c.recv().expect("snapshot")["t"], "snapshot");
    // 300 lines: far more than the pane shows, so most are in history.
    let s = create_terminal(
        &mut c,
        1,
        "hist",
        "seq -f 'row %g pages_manage' 1 300; echo 'Mixed TUI line'; sleep 600",
    );
    let deadline = Instant::now() + WAIT;
    loop {
        let out = fx.tmux(&["capture-pane", "-p", "-t", &s]);
        let screen = String::from_utf8_lossy(&out.stdout).into_owned();
        if screen.contains("Mixed TUI line") {
            break;
        }
        assert!(Instant::now() < deadline, "the output never came: {screen}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let line = |fx: &Fixture| {
        let out = fx.tmux(&[
            "display",
            "-p",
            "-t",
            &s,
            "#{pane_in_mode}|#{copy_cursor_line}",
        ]);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let mut id = 10;
    let mut search_with = |c: &mut Conn, action: &str, text: &str, extra: Value| {
        id += 1;
        let mut op = json!({"op": "search", "session": s, "action": action, "text": text});
        op.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let seen = command(c, id, op);
        let ack = seen.last().unwrap().clone();
        assert_eq!(ack["ok"], true, "{ack}");
        ack["search"].clone()
    };

    // Typed a character at a time: the last keystroke decides.
    search_with(&mut c, "find", "row 2", json!({}));
    let r = search_with(&mut c, "find", "row 29", json!({}));
    assert_eq!(r["found"], true, "{r}");
    assert_eq!(line(&fx), "1|row 299 pages_manage");
    if r.get("count").is_some() {
        // row 29, 290…299
        assert_eq!(r["count"], 11, "{r}");
    }
    search_with(&mut c, "older", "row 29", json!({}));
    assert_eq!(line(&fx), "1|row 298 pages_manage");
    search_with(&mut c, "older", "row 29", json!({}));
    assert_eq!(line(&fx), "1|row 297 pages_manage");
    search_with(&mut c, "newer", "row 29", json!({}));
    assert_eq!(line(&fx), "1|row 298 pages_manage");
    // Each change of direction moves on (tmux 3.4 needs a second step).
    search_with(&mut c, "older", "row 29", json!({}));
    assert_eq!(line(&fx), "1|row 297 pages_manage");
    search_with(&mut c, "newer", "row 29", json!({}));
    search_with(&mut c, "newer", "row 29", json!({}));
    assert_eq!(line(&fx), "1|row 299 pages_manage");

    // The wheel in copy mode reaches a program that takes the mouse on
    // the alternate screen, and otherwise scrolls as tmux's own binding
    // did, which is kept.
    for table in ["copy-mode", "copy-mode-vi"] {
        let out = fx.tmux(&["list-keys", "-T", table, "WheelUpPane"]);
        let binding = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            binding.contains("#{&&:#{alternate_on},#{mouse_any_flag}}")
                && binding.contains("send-keys -M")
                && binding.contains("scroll-up"),
            "{binding}"
        );
    }

    // Deep in the history.
    let r = search_with(&mut c, "find", "row 7 pages", json!({}));
    assert_eq!(r["found"], true, "{r}");
    assert_eq!(line(&fx), "1|row 7 pages_manage");
    // The text is literal, not a pattern, and quotes are kept.
    let r = search_with(&mut c, "find", "row.7", json!({}));
    assert_eq!(r["found"], false, "{r}");
    assert!(r.get("count").is_none(), "{r}");

    // Case is ignored unless asked for.
    let r = search_with(&mut c, "find", "mixed tui", json!({}));
    assert_eq!(r["found"], true, "{r}");
    assert_eq!(line(&fx), "1|Mixed TUI line");
    let r = search_with(&mut c, "find", "tui", json!({"match_case": true}));
    assert_eq!(r["found"], false, "{r}");
    let r = search_with(&mut c, "find", "TUI", json!({"match_case": true}));
    assert_eq!(r["found"], true, "{r}");
    let r = search_with(&mut c, "find", "Pages", json!({"match_case": true}));
    assert_eq!(r["found"], false, "{r}");

    // The client's colours mark the matches; anything but #rrggbb is
    // left out.
    let style = |fx: &Fixture, opt: &str| {
        let out = fx.tmux(&["show", "-wv", "-t", &s, opt]);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let colors = |bg: &str| {
        json!({"colors": {"current_bg": bg, "current_fg": "#000000",
                          "match_bg": "#4b3f6b", "match_fg": "#f8f8f2"}})
    };
    search_with(&mut c, "find", "row 29", colors("#bd93f9"));
    assert_eq!(
        style(&fx, "copy-mode-current-match-style"),
        "bg=#bd93f9,fg=#000000"
    );
    assert_eq!(style(&fx, "copy-mode-match-style"), "bg=#4b3f6b,fg=#f8f8f2");
    search_with(&mut c, "find", "row 29", colors("red,blink"));
    assert_eq!(
        style(&fx, "copy-mode-current-match-style"),
        "bg=#bd93f9,fg=#000000"
    );

    // Left copy mode behind Bosun's back: the next step starts again.
    fx.tmux(&["send-keys", "-t", &s, "-X", "cancel"]);
    let r = search_with(&mut c, "older", "row 29", json!({}));
    assert_eq!(r["found"], true, "{r}");
    assert_eq!(line(&fx), "1|row 299 pages_manage");

    // `end`, and `find` with nothing typed, leave copy mode.
    let r = search_with(&mut c, "end", "", json!({}));
    assert_eq!(r["found"], false, "{r}");
    assert!(line(&fx).starts_with("0|"), "{}", line(&fx));
    search_with(&mut c, "find", "row 29", json!({}));
    search_with(&mut c, "find", "", json!({}));
    assert!(line(&fx).starts_with("0|"), "{}", line(&fx));
    // `end` on a pane not in copy mode does nothing.
    search_with(&mut c, "end", "", json!({}));

    let seen = command(
        &mut c,
        99,
        json!({"op": "search", "session": "bosun-nope-1", "action": "find", "text": "x"}),
    );
    assert_eq!(seen.last().unwrap()["ok"], false, "{seen:#?}");
}

/// `set_section_defaults` stores a section's starting values for new
/// sessions: they go out with the sidebar, survive a client's
/// `sidebar_save` (which doesn't carry them) and a restart, and `null`
/// clears them.
#[test]
fn serve_section_defaults() {
    let mut fx = fixture();
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    assert_eq!(c.recv().expect("snapshot")["t"], "snapshot");
    let seen = command(&mut c, 1, json!({"op": "insert_section", "name": "work"}));
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    c.send(json!({"t": "resync"}));
    let snap = c.until("snapshot", |v| v["t"] == "snapshot").pop().unwrap();
    let section = snap["sidebar"]["sections"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let defaults = json!({"path": "/tmp/api", "agent": "codex", "args": "--model o4",
                          "options": {"codex": {"session_mode": "continue", "yolo": true}}});
    let seen = command(
        &mut c,
        2,
        json!({"op": "set_section_defaults", "section": section, "defaults": defaults}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    let delta = seen
        .iter()
        .rev()
        .find(|v| v["t"] == "sidebar")
        .unwrap_or_else(|| panic!("no sidebar delta: {seen:#?}"));
    let got = &delta["sidebar"]["sections"][0]["defaults"];
    assert_eq!(got["path"], "/tmp/api", "{got}");
    assert_eq!(got["agent"], "codex");
    assert_eq!(got["args"], "--model o4");
    assert_eq!(got["options"]["codex"]["yolo"], true);
    assert_eq!(got["options"]["codex"]["session_mode"], "continue");

    // A client's sidebar_save without defaults leaves them alone.
    let mut sidebar = delta["sidebar"].clone();
    sidebar["sections"][0]
        .as_object_mut()
        .unwrap()
        .remove("defaults");
    sidebar["sections"][0]["collapsed"] = json!(true);
    let seen = command(&mut c, 3, json!({"op": "sidebar_save", "sidebar": sidebar}));
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    c.send(json!({"t": "resync"}));
    let snap = c.until("snapshot", |v| v["t"] == "snapshot").pop().unwrap();
    assert_eq!(snap["sidebar"]["sections"][0]["collapsed"], true);
    assert_eq!(snap["sidebar"]["sections"][0]["defaults"]["agent"], "codex");

    // Unknown agent or section: refused.
    let seen = command(
        &mut c,
        4,
        json!({"op": "set_section_defaults", "section": section, "defaults": {"agent": "nope"}}),
    );
    assert_eq!(seen.last().unwrap()["ok"], false);
    let seen = command(
        &mut c,
        5,
        json!({"op": "set_section_defaults", "section": "sec-nope", "defaults": null}),
    );
    assert_eq!(seen.last().unwrap()["ok"], false);

    // They're in config.toml: a restart keeps them.
    drop(c);
    let mut child = fx.child.take().unwrap();
    // SAFETY: signalling a child we spawned and haven't reaped.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    child.wait().unwrap();
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    let snap = c.recv().expect("snapshot");
    assert_eq!(
        snap["sidebar"]["sections"][0]["defaults"]["path"], "/tmp/api",
        "{}",
        snap["sidebar"]
    );

    // null clears them.
    let seen = command(
        &mut c,
        6,
        json!({"op": "set_section_defaults", "section": section, "defaults": null}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    c.send(json!({"t": "resync"}));
    let snap = c.until("snapshot", |v| v["t"] == "snapshot").pop().unwrap();
    assert!(
        snap["sidebar"]["sections"][0].get("defaults").is_none(),
        "{}",
        snap["sidebar"]
    );
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn section_named<'a>(sidebar: &'a Value, name: &str) -> Option<&'a Value> {
    sidebar["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
}

/// A fresh snapshot's sidebar once `cond` holds for it.
fn sidebar_until(c: &mut Conn, what: &str, mut cond: impl FnMut(&Value) -> bool) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        c.send(json!({"t": "resync"}));
        let snap = c.until("snapshot", |v| v["t"] == "snapshot").pop().unwrap();
        if cond(&snap["sidebar"]) {
            return snap["sidebar"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {}",
            snap["sidebar"]
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Docking kills a row's sessions and keeps the row in Docked; undocking
/// puts it back where it was and resumes each tab in its place. A
/// snoozed row wakes on time, or with `until_needed` when its agent
/// finishes; dragging a row into Docked (a client's `sidebar_save`)
/// docks it too.
#[test]
fn serve_docked_rows_keep_their_name_and_can_be_renamed() {
    let mut fx = fixture();
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    assert_eq!(c.recv().expect("snapshot")["t"], "snapshot");
    let alpha = create_terminal(&mut c, 1, "alpha", "sleep 600");
    let sidebar = sidebar_with(&mut c, &[&alpha]);
    let row = row_of(&sidebar, &alpha).unwrap();
    let docked_tab =
        |sb: &Value| section_named(sb, "Docked").map(|s| s["members"][0]["tabs"][0].clone());

    // Renamed while live, then docked: the row keeps the new name, not
    // the one recents has from its creation.
    let seen = command(
        &mut c,
        10,
        json!({"op": "rename", "session": alpha, "display": "alpha two"}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    sidebar_until(&mut c, "the live rename", |sb| {
        sb["ungrouped"][0]["tabs"][0]["display"] == "alpha two"
    });
    let seen = command(
        &mut c,
        11,
        json!({"op": "park_row", "container_id": row, "role": "docked"}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    let sb = sidebar_until(&mut c, "alpha docked", |sb| {
        docked_tab(sb).is_some_and(|t| t["alive"] == false)
    });
    let tab = docked_tab(&sb).unwrap();
    assert_eq!(tab["display"], "alpha two", "{tab}");
    assert!(
        tab["recent"].is_i64(),
        "an exited tab says which recent it restarts from: {tab}"
    );

    // Renaming the docked tab works too, and is kept in config.toml.
    let seen = command(
        &mut c,
        12,
        json!({"op": "rename", "session": alpha, "display": "alpha three"}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    sidebar_until(&mut c, "the docked rename", |sb| {
        docked_tab(sb).is_some_and(|t| t["display"] == "alpha three")
    });
    let config = std::fs::read_to_string(fx.config_path()).unwrap();
    assert!(config.contains("alpha three"), "{config}");
    // A session that isn't anywhere still fails.
    let seen = command(
        &mut c,
        13,
        json!({"op": "rename", "session": "bosun-nope-1", "display": "x"}),
    );
    assert_eq!(seen.last().unwrap()["ok"], false, "{seen:#?}");

    // Undocked, it comes back under that name.
    let seen = command(&mut c, 14, json!({"op": "unpark_row", "container_id": row}));
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    let sb = sidebar_until(&mut c, "alpha resumed", |sb| {
        sb["ungrouped"][0]["id"] == row.as_str() && sb["ungrouped"][0]["tabs"][0]["alive"] == true
    });
    let tab = &sb["ungrouped"][0]["tabs"][0];
    assert_eq!(tab["display"], "alpha three", "{tab}");
    assert!(tab.get("recent").is_none(), "{tab}");
}

#[test]
fn serve_dock_and_snooze() {
    let mut fx = fixture();
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    assert_eq!(c.recv().expect("snapshot")["t"], "snapshot");
    let alpha = create_terminal(&mut c, 1, "alpha", "sleep 600");
    let beta = create_terminal(&mut c, 2, "beta", "sleep 600");
    let sidebar = sidebar_with(&mut c, &[&alpha, &beta]);
    let a_row = row_of(&sidebar, &alpha).unwrap();
    let b_row = row_of(&sidebar, &beta).unwrap();
    let alive = |fx: &Fixture, s: &str| {
        let ls = fx.tmux(&["list-sessions", "-F", "#{session_name}"]);
        String::from_utf8_lossy(&ls.stdout).lines().any(|l| l == s)
    };

    // Dock alpha: into Docked, its session gone, the row kept.
    let seen = command(
        &mut c,
        10,
        json!({"op": "park_row", "container_id": a_row, "role": "docked"}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    wait_for("alpha killed", WAIT, || !alive(&fx, &alpha));
    let sb = sidebar_until(&mut c, "alpha docked and exited", |sb| {
        section_named(sb, "Docked").is_some_and(|s| s["members"][0]["tabs"][0]["alive"] == false)
    });
    let docked = section_named(&sb, "Docked").unwrap();
    assert_eq!(docked["role"], "docked");
    assert_eq!(docked["collapsed"], true);
    assert_eq!(docked["members"][0]["id"], a_row.as_str());
    assert_eq!(docked["members"][0]["parked"]["from_index"], 0, "{docked}");
    assert_eq!(sb["ungrouped"].as_array().unwrap().len(), 1);
    // New sessions can't be made in it.
    let sec = docked["id"].as_str().unwrap().to_string();
    let seen = command(
        &mut c,
        11,
        json!({"op": "create", "section": sec, "spec": {"name": "x", "path": "/tmp", "agent": "terminal"}}),
    );
    assert_eq!(seen.last().unwrap()["ok"], false);

    // Undock: back at the top, a live session in the dead tab's place,
    // and no Docked section left.
    let seen = command(
        &mut c,
        12,
        json!({"op": "unpark_row", "container_id": a_row}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    let sb = sidebar_until(&mut c, "alpha resumed", |sb| {
        sb["ungrouped"][0]["id"] == a_row.as_str() && sb["ungrouped"][0]["tabs"][0]["alive"] == true
    });
    assert!(section_named(&sb, "Docked").is_none(), "{sb}");
    let row = &sb["ungrouped"][0];
    assert_eq!(row_tabs(row).len(), 1, "{row}");
    let resumed = row_tabs(row)[0].clone();
    assert_ne!(resumed, alpha);
    assert_eq!(row["tabs"][0]["display"], "alpha");
    assert!(row.get("parked").is_none(), "{row}");
    assert_eq!(container_option(&fx, &resumed), a_row);

    // Snooze beta for a moment: it wakes by itself, still running.
    let seen = command(
        &mut c,
        20,
        json!({"op": "park_row", "container_id": b_row, "role": "snoozed", "wake_at": now_ms() + 1500}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    let sb = sidebar_until(&mut c, "beta snoozed", |sb| {
        section_named(sb, "Snoozed").is_some()
    });
    assert_eq!(
        section_named(&sb, "Snoozed").unwrap()["members"][0]["id"],
        b_row.as_str()
    );
    assert!(alive(&fx, &beta));
    sidebar_until(&mut c, "beta awake", |sb| {
        section_named(sb, "Snoozed").is_none() && sb["ungrouped"][1]["id"] == b_row.as_str()
    });
    assert!(alive(&fx, &beta));

    // Until it needs you: the agent's Stop hook wakes it.
    let seen = command(
        &mut c,
        21,
        json!({"op": "park_row", "container_id": b_row, "role": "snoozed", "until_needed": true}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    sidebar_until(&mut c, "beta snoozed until needed", |sb| {
        section_named(sb, "Snoozed").is_some()
    });
    std::thread::sleep(Duration::from_millis(1500));
    let sb = sidebar_until(&mut c, "beta still snoozed", |_| true);
    assert!(
        section_named(&sb, "Snoozed").is_some(),
        "nothing needs you yet: {sb}"
    );
    run_claude_hook(
        &fx,
        &beta,
        "Stop",
        json!({"hook_event_name": "Stop", "stop_hook_active": false}),
    );
    sidebar_until(&mut c, "beta woken by its agent", |sb| {
        section_named(sb, "Snoozed").is_none()
    });

    // Dragging beta into Docked (a sidebar_save) docks it: killed.
    let seen = command(
        &mut c,
        30,
        json!({"op": "park_row", "container_id": a_row, "role": "docked"}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true);
    let mut sb = sidebar_until(&mut c, "alpha docked again", |sb| {
        section_named(sb, "Docked").is_some()
    });
    let bi = sb["ungrouped"]
        .as_array()
        .unwrap()
        .iter()
        .position(|r| r["id"] == b_row.as_str())
        .unwrap();
    let row = sb["ungrouped"].as_array_mut().unwrap().remove(bi);
    let di = sb["sections"]
        .as_array()
        .unwrap()
        .iter()
        .position(|s| s["name"] == "Docked")
        .unwrap();
    sb["sections"][di]["members"]
        .as_array_mut()
        .unwrap()
        .push(row);
    let seen = command(&mut c, 31, json!({"op": "sidebar_save", "sidebar": sb}));
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    wait_for("beta killed by the drag", WAIT, || !alive(&fx, &beta));
    let sb = sidebar_until(&mut c, "beta parked by the drag", |sb| {
        section_named(sb, "Docked").is_some_and(|s| s["members"].as_array().unwrap().len() == 2)
    });
    let docked = section_named(&sb, "Docked").unwrap();
    assert_eq!(
        docked["role"], "docked",
        "the save didn't carry the role, the daemon kept it"
    );
    assert!(docked["members"][1]["parked"].is_object(), "{docked}");

    // The TUI moving beta out of Docked in config.toml undocks it too:
    // resumed in its tab's place.
    let cfg = fx.config_path();
    let mut doc: toml::Value = toml::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    let sidebar = doc["sidebar"].as_table_mut().unwrap();
    let sections = sidebar["sections"].as_array_mut().unwrap();
    let docked = sections
        .iter_mut()
        .find(|s| s["name"].as_str() == Some("Docked"))
        .unwrap();
    let members = docked["members"].as_array_mut().unwrap();
    let bi = members
        .iter()
        .position(|r| r["id"].as_str() == Some(b_row.as_str()))
        .unwrap();
    let row = members.remove(bi);
    sidebar
        .entry("ungrouped")
        .or_insert_with(|| toml::Value::Array(vec![]))
        .as_array_mut()
        .unwrap()
        .push(row);
    write_config(&cfg, &toml::to_string(&doc).unwrap());
    let sb = sidebar_until(&mut c, "beta undocked by the TUI's edit", |sb| {
        sb["ungrouped"].as_array().unwrap().iter().any(|r| {
            r["id"] == b_row.as_str() && r["tabs"][0]["alive"] == true && r.get("parked").is_none()
        })
    });
    let row = sb["ungrouped"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == b_row.as_str())
        .unwrap();
    assert_eq!(row["tabs"][0]["display"], "beta");
    assert_ne!(row_tabs(row)[0], beta);
}

/// The TUI, running beside the daemon on the same tmux server and the
/// same `config.toml`, shows a row the daemon merged as one row with two
/// tabs, and doesn't write its old layout back. The TUI runs in a pane
/// of a second throwaway tmux server, narrow enough that it lists a
/// row's tabs under it in the sidebar.
#[test]
fn tui_shows_rows_the_daemon_merged() {
    let mut fx = fixture();
    let path = fx.start();
    let mut c = Conn::connect(&path);
    c.send(json!({"t": "hello", "protocol": 1}));
    assert_eq!(c.recv().expect("snapshot")["t"], "snapshot");
    let alpha = create_terminal(&mut c, 1, "alpha", "");
    let beta = create_terminal(&mut c, 2, "beta", "");
    let sidebar = sidebar_with(&mut c, &[&alpha, &beta]);
    let a_row = row_of(&sidebar, &alpha).unwrap();
    let b_row = row_of(&sidebar, &beta).unwrap();

    let host = Host(format!("{}-host", fx.sock));
    let unset: String = [
        "TMUX",
        "TMUX_PANE",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "BOSUN_PREFIX",
        "BOSUN_THEME",
        "BOSUN_DEFAULT_AGENT",
        "BOSUN_REMOVE_DEAD_SESSIONS",
        "BOSUN_SHOW_GROUP_IN_TITLE",
        "BOSUN_HIDE_SINGLE_TAB_STRIP",
        "BOSUN_EMBED",
        "BOSUN_TMUX_CONTROL",
    ]
    .iter()
    .map(|v| format!("-u {v} "))
    .collect();
    let tui = format!(
        "exec env {unset}HOME='{}' BOSUN_TMUX_SOCKET='{}' '{BIN}'",
        fx.home.path().display(),
        fx.sock
    );
    host.run(&[
        "new-session",
        "-d",
        "-s",
        "tui",
        "-x",
        "70",
        "-y",
        "30",
        &tui,
    ]);
    // Runs of spaces collapsed: the active tab gets padding around it.
    let screen = || {
        host.run(&["capture-pane", "-p", "-t", "tui"])
            .split(' ')
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    };
    wait_for("the TUI lists both rows", WAIT, || {
        let s = screen();
        s.contains("alpha") && s.contains("beta")
    });
    assert!(!screen().contains("(2)"), "{}", screen());

    let seen = command(
        &mut c,
        10,
        json!({"op": "merge_rows", "from": b_row, "into": a_row}),
    );
    assert_eq!(seen.last().unwrap()["ok"], true, "{seen:#?}");
    // One row with a two-tab badge, its tabs listed under it.
    let deadline = Instant::now() + WAIT;
    loop {
        let s = screen();
        if s.contains("(2)") && s.contains("alpha · beta") {
            break;
        }
        assert!(Instant::now() < deadline, "no merged row in the TUI:\n{s}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let s = screen();
    assert_eq!(
        s.matches("beta").count(),
        1,
        "beta has no row of its own:\n{s}"
    );

    // The TUI doesn't write its old layout back over the merge.
    std::thread::sleep(Duration::from_secs(3));
    let config = std::fs::read_to_string(fx.config_path()).unwrap();
    let file: toml::Value = toml::from_str(&config).unwrap();
    let ungrouped = file["sidebar"]["ungrouped"].as_array().unwrap();
    assert_eq!(ungrouped.len(), 1, "{config}");
    assert_eq!(ungrouped[0]["id"].as_str(), Some(a_row.as_str()));
    let members: Vec<&str> = ungrouped[0]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    assert_eq!(members, vec![alpha.as_str(), beta.as_str()]);
    let s = screen();
    assert!(s.contains("alpha · beta"), "{s}");
}

/// A second throwaway tmux server to run the TUI in. Killed on drop.
struct Host(String);

impl Host {
    fn run(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .arg("-L")
            .arg(&self.0)
            .args(args)
            .env_remove("TMUX")
            .output()
            .expect("tmux");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.run(&["kill-server"]);
    }
}
