//! The real foreground command of a pane whose shell runs behind a PTY
//! proxy (#24).
//!
//! Tools like Atuin's output capture (`atuin pty-proxy`), `script` and
//! `asciinema rec` own the tmux pane's terminal and run the shell on a
//! second PTY of their own. tmux only sees the pane's terminal, so
//! `#{pane_current_command}` reads `atuin` at the prompt and while an
//! agent runs alike, and Bosun can't tell a bare shell from a live
//! agent: launches wait out the shell timeout, the "did the agent
//! start?" check passes at once, and an agent that exited never reads
//! as a shell.
//!
//! When tmux reports one of [`PTY_PROXIES`], this module reads the
//! process table and follows the proxy to the PTY it runs the shell
//! on, then returns that PTY's foreground process, the same thing tmux
//! reports for an ordinary pane. Only known proxies are followed: an
//! agent can run its own tools on a PTY too (Codex does), and following
//! that one would read a tool's shell as "the agent exited".

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::process::Command;

/// Commands that run the shell on a PTY of their own. Matched against
/// the basename tmux (or `ps`) reports.
pub const PTY_PROXIES: &[&str] = &["atuin", "script", "asciinema"];

/// Hops to follow through nested proxies (`script` inside Atuin, say)
/// before giving up.
const MAX_HOPS: usize = 4;

pub fn is_pty_proxy(cmd: &str) -> bool {
    PTY_PROXIES.contains(&basename(cmd))
}

/// One row of `ps -A -o pid=,ppid=,pgid=,tpgid=,tty=,comm=`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    /// Foreground process group of the process's controlling terminal;
    /// 0 (macOS) or -1 (Linux) when it has none.
    pub tpgid: i32,
    /// Controlling terminal without `/dev/` (`ttys003`, `pts/3`), or
    /// `None` for `??` (macOS) / `?` (Linux).
    pub tty: Option<String>,
    /// Basename of the command (`ps` prints the full path on macOS).
    pub name: String,
}

/// The arguments for [`parse_ps`]. `comm` is last because on macOS it's
/// the executable's full path, which may contain spaces.
pub const PS_ARGS: &[&str] = &["-A", "-o", "pid=,ppid=,pgid=,tpgid=,tty=,comm="];

pub fn parse_ps(out: &str) -> Vec<Proc> {
    out.lines().filter_map(parse_ps_line).collect()
}

fn parse_ps_line(line: &str) -> Option<Proc> {
    let mut rest = line.trim_start();
    let mut field = || {
        let (head, tail) = rest.split_once(char::is_whitespace)?;
        rest = tail.trim_start();
        Some(head)
    };
    let pid = field()?.parse().ok()?;
    let ppid = field()?.parse().ok()?;
    let pgid = field()?.parse().ok()?;
    let tpgid = field()?.parse().ok()?;
    let tty = field()?;
    let comm = rest.trim_end();
    if comm.is_empty() {
        return None;
    }
    Some(Proc {
        pid,
        ppid,
        pgid,
        tpgid,
        tty: (!tty.starts_with('?')).then(|| tty.to_string()),
        name: basename(comm).to_string(),
    })
}

fn basename(cmd: &str) -> &str {
    cmd.rsplit('/').next().unwrap_or(cmd)
}

/// The foreground command of the pane on `pane_tty` (`/dev/ttys001` as
/// `#{pane_tty}` gives it), looking through PTY proxies. `None` when
/// the table has nothing for that terminal.
pub fn resolve(procs: &[Proc], pane_tty: &str) -> Option<String> {
    let mut tty = pane_tty
        .strip_prefix("/dev/")
        .unwrap_or(pane_tty)
        .to_string();
    let mut name = None;
    for _ in 0..MAX_HOPS {
        let leader = foreground_leader(procs, &tty)?;
        name = Some(leader.name.clone());
        if !is_pty_proxy(&leader.name) {
            break;
        }
        match inner_tty(procs, leader.pid, &tty) {
            Some(inner) => tty = inner,
            None => break,
        }
    }
    name
}

/// The process tmux would name for `tty`: the leader of its foreground
/// process group, or any member if the leader has gone.
fn foreground_leader<'a>(procs: &'a [Proc], tty: &str) -> Option<&'a Proc> {
    let tpgid = procs
        .iter()
        .find(|p| p.tty.as_deref() == Some(tty) && p.tpgid > 0)?
        .tpgid;
    procs
        .iter()
        .find(|p| p.pid == tpgid)
        .or_else(|| procs.iter().find(|p| p.pgid == tpgid))
}

/// The terminal a proxy runs its shell on: that of the nearest
/// descendant of `proxy` attached to a terminal other than `outer`.
fn inner_tty(procs: &[Proc], proxy: i32, outer: &str) -> Option<String> {
    let mut level = vec![proxy];
    for _ in 0..MAX_HOPS {
        let children: Vec<&Proc> = procs.iter().filter(|p| level.contains(&p.ppid)).collect();
        if let Some(p) = children
            .iter()
            .find(|p| p.tty.as_deref().is_some_and(|t| t != outer))
        {
            return p.tty.clone();
        }
        level = children.iter().map(|p| p.pid).collect();
        if level.is_empty() {
            break;
        }
    }
    None
}

/// Reads the process table, keeping the last read for a short while so
/// the 250 ms status poll runs `ps` at most about once a second (a full
/// `ps -A` takes ~70 ms on a busy Mac). Only consulted when tmux
/// reports a proxy, so panes without one never pay for it. Clones share
/// the cached table.
#[derive(Debug, Clone, Default)]
pub struct ProcTable {
    cached: Arc<Mutex<Option<Snapshot>>>,
}

/// A read of the process table and when it was taken.
type Snapshot = (Instant, Arc<Vec<Proc>>);

impl ProcTable {
    /// The process table, read afresh when the cached one is older than
    /// `max_age` (`Duration::ZERO` always reads). `None` if `ps` failed.
    pub async fn get(&self, max_age: Duration) -> Option<Arc<Vec<Proc>>> {
        if let Some((at, procs)) = self.cached.lock().ok()?.as_ref() {
            if at.elapsed() < max_age {
                return Some(procs.clone());
            }
        }
        let out = Command::new("ps")
            .args(PS_ARGS)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())?;
        let procs = Arc::new(parse_ps(&String::from_utf8_lossy(&out.stdout)));
        if let Ok(mut cached) = self.cached.lock() {
            *cached = Some((Instant::now(), procs.clone()));
        }
        Some(procs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(rows: &str) -> Vec<Proc> {
        parse_ps(rows)
    }

    // Atuin started by fish's integration at the first prompt: the
    // outer fish runs `atuin pty-proxy`, which runs the inner fish on
    // pts/7.
    const ATUIN_IDLE: &str = "\
      100     1   100   200 pts/3    fish
      200   100   200   200 pts/3    atuin
      201   200   201   201 pts/7    fish
      300     1   300    -1 ?        sshd
";

    #[test]
    fn idle_shell_behind_atuin_reads_as_the_shell() {
        assert_eq!(
            resolve(&table(ATUIN_IDLE), "/dev/pts/3").as_deref(),
            Some("fish")
        );
    }

    #[test]
    fn agent_behind_atuin_reads_as_the_agent() {
        let procs = table(
            "\
      100     1   100   200 pts/3    fish
      200   100   200   200 pts/3    atuin
      201   200   201   202 pts/7    fish
      202   201   202   202 pts/7    /Users/a/.local/share/claude/versions/2.1.310
      203   202   202   202 pts/7    node
",
        );
        assert_eq!(resolve(&procs, "/dev/pts/3").as_deref(), Some("2.1.310"));
    }

    #[test]
    fn script_on_macos() {
        // Captured from `tmux new-session 'script -q /dev/null zsh -f'`
        // with `sleep 30` running.
        let procs = table(
            "\
28439 28438 28439 28439 ttys001  /usr/bin/script
28441 28439 28441 28684 ttys003  -zsh
28684 28441 28684 28684 ttys003  sleep
",
        );
        assert_eq!(resolve(&procs, "/dev/ttys001").as_deref(), Some("sleep"));
    }

    #[test]
    fn nested_proxies_are_followed() {
        let procs = table(
            "\
      200     1   200   200 pts/3    atuin
      201   200   201   300 pts/7    fish
      300   201   300   300 pts/7    script
      301   300   301   301 pts/9    bash
",
        );
        assert_eq!(resolve(&procs, "/dev/pts/3").as_deref(), Some("bash"));
    }

    #[test]
    fn a_pty_an_agent_opens_for_its_tools_is_not_followed() {
        // Codex runs its tool shells on a PTY of its own; that zsh is
        // not the pane's shell coming back.
        let procs = table(
            "\
      100     1   100   150 ttys001  zsh
      150   100   150   150 ttys001  codex
      151   150   151   151 ttys005  zsh
",
        );
        assert_eq!(resolve(&procs, "/dev/ttys001").as_deref(), Some("codex"));
    }

    #[test]
    fn proxy_without_an_inner_terminal_is_reported_as_is() {
        let procs = table("  200     1   200   200 pts/3    atuin\n");
        assert_eq!(resolve(&procs, "/dev/pts/3").as_deref(), Some("atuin"));
    }

    #[test]
    fn unknown_terminal_resolves_to_none() {
        assert_eq!(resolve(&table(ATUIN_IDLE), "/dev/pts/99"), None);
    }

    #[test]
    fn parses_paths_with_spaces_and_missing_terminals() {
        let procs = table(
            " 1463  1323  1323     0 ??       /Applications/Some App.app/Contents/MacOS/Some Helper\n\
             garbage line\n",
        );
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0].name, "Some Helper");
        assert_eq!(procs[0].tty, None);
    }

    #[test]
    fn proxy_names() {
        assert!(is_pty_proxy("atuin"));
        assert!(is_pty_proxy("/usr/bin/script"));
        assert!(!is_pty_proxy("zsh"));
        assert!(!is_pty_proxy("2_1_301"));
    }
}
