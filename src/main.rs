use std::io::{self, Stdout};
use std::sync::Arc;

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use bosun::app::App;
use bosun::config::Config;
use bosun::error::{BosunError, Result};
use bosun::store::Store;
use bosun::tmux::{
    attach::emergency_unbind, status_bar::emergency_uninstall as emergency_status_bar,
    TokioTmuxClient,
};

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("bosun {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // `bosun update [--check]` runs synchronously, prints to stderr,
    // and exits before any TUI/tmux machinery starts. Failures
    // surface as a non-zero exit code with the anyhow chain printed.
    let mut args = std::env::args().skip(1);
    if let Some(first) = args.next() {
        if first == "update" {
            let check_only = args.any(|a| a == "--check");
            return match bosun::commands::update::run(check_only) {
                Ok(()) => Ok(()),
                Err(e) => {
                    eprintln!("bosun update: {:#}", e);
                    std::process::exit(1);
                }
            };
        }
        if first == "release-notes" {
            return match bosun::commands::release_notes::run() {
                Ok(()) => Ok(()),
                Err(e) => {
                    eprintln!("bosun release-notes: {:#}", e);
                    std::process::exit(1);
                }
            };
        }
        if first == "editor" {
            // `bosun editor` (no arg) prints current; `bosun editor
            // <cmd>` sets it. We don't accept multi-word commands
            // through argv splitting — if a user wants `code --new-window`
            // they can edit config.toml directly. The simple form is
            // the documented path.
            let arg = args.next();
            return match bosun::commands::editor::run(arg) {
                Ok(()) => Ok(()),
                Err(e) => {
                    eprintln!("bosun editor: {:#}", e);
                    std::process::exit(1);
                }
            };
        }
        if first == "help" || first == "--help" || first == "-h" {
            print_help();
            return Ok(());
        }
    }

    // Everything past this point shells out to tmux, so a missing
    // binary should fail here with install instructions — not as a
    // bare io::Error after we've already entered the alternate screen.
    ensure_tmux_installed();

    init_tracing();

    let config = Config::from_env();
    let socket = config.tmux_socket.clone();
    let client: Arc<TokioTmuxClient> = match &socket {
        Some(s) => Arc::new(TokioTmuxClient::with_socket(s.clone())),
        None => Arc::new(TokioTmuxClient::new()),
    };

    // Open the SQLite store for recents + future metadata. Failure
    // here is non-fatal — we fall back to an in-memory store so bosun
    // still runs, just without persistence across launches.
    let store = Arc::new(match Store::open_default() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("store open failed, using in-memory: {}", e);
            Store::in_memory().expect("in-memory store cannot fail")
        }
    });

    // Panic hook: restore terminal + clean up C-q binding + restore
    // the user's tmux status bar before we die.
    let socket_for_hook = socket.clone();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        // Pop unconditionally — harmless if we never pushed (the
        // terminal ignores a pop on an empty stack), and it keeps the
        // kitty keyboard protocol from leaking past a panic.
        let _ = execute!(
            io::stdout(),
            PopKeyboardEnhancementFlags,
            DisableFocusChange,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        emergency_unbind(socket_for_hook.as_deref());
        emergency_status_bar(socket_for_hook.as_deref());
        default_hook(info);
    }));

    let (mut terminal, kbd_enhanced) = setup_terminal()?;

    // Probe the outer terminal for its default fg/bg/cursor colors so
    // the embedded session PTY can answer the OSC 10/11/12 queries that
    // apps like Codex / Neovim use to pick a light vs dark palette
    // (issue #2). Must run here — after raw mode is on but *before*
    // `App::new` spawns the input actor that owns stdin. Only worth the
    // ~120ms window when the embed is actually in play; if the terminal
    // doesn't answer, the embed falls back to the active theme colors.
    let term_colors = if config.embed_enabled {
        bosun::terminal_query::probe(std::time::Duration::from_millis(120))
    } else {
        bosun::terminal_query::TermColors::default()
    };

    let mut app = App::new(client, socket, config, store);
    app.kbd_enhanced = kbd_enhanced;
    app.term_colors = term_colors;
    let run_result = app.run(&mut terminal).await;
    restore_terminal(&mut terminal, kbd_enhanced)?;
    run_result
}

/// Set up the terminal and return it alongside whether the kitty
/// keyboard progressive-enhancement flags were successfully pushed.
/// The bool is threaded back so teardown pops exactly what we pushed.
fn setup_terminal() -> Result<(Terminal<CrosstermBackend<Stdout>>, bool)> {
    enable_raw_mode().map_err(BosunError::Io)?;
    let mut stdout = io::stdout();
    // Mouse capture is needed so the draggable divider between the
    // session list and preview pane can see clicks and drags. We
    // tear it down around `tmux attach` so tmux owns the mouse
    // during an attach (see `App::perform_attach`).
    // Bracketed paste lets crossterm hand us pasted text as one
    // `Event::Paste(String)` rather than character-by-character
    // `Event::Key` events. The outer terminal also encodes
    // drag-drop file paths and image markers using the same
    // protocol, so this is the path for "I dropped an image onto
    // bosun" → forward to the focused embed PTY.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        // FocusGained/FocusLost events let us recover from things
        // like iTerm's Cmd+R "reset" that clears the screen and
        // exits alt screen out from under ratatui — the next focus
        // gain triggers a full repaint. See `App::recover_display`.
        EnableFocusChange,
    )
    .map_err(BosunError::Io)?;

    // Request the kitty keyboard protocol's "disambiguate escape
    // codes" enhancement. This is what makes the outer terminal
    // report modifiers unambiguously — without it, terminals fall
    // back to legacy encoding and hand us *bare* keys for chords
    // like Option+Delete (should be Alt+Backspace) and Shift+Up/Down
    // (should be modified arrows), which breaks word-delete in the
    // embed and the in-focus session-cycle chords. Gated on
    // `supports_keyboard_enhancement` so it's a no-op on terminals
    // that don't speak the protocol (Apple Terminal.app), where we
    // keep the prior behavior. DISAMBIGUATE alone (no
    // REPORT_EVENT_TYPES) means no key-release events, so the focus
    // nav chords don't double-fire.
    let kbd_enhanced = matches!(supports_keyboard_enhancement(), Ok(true))
        && execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        )
        .is_ok();

    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
        .map(|t| (t, kbd_enhanced))
        .map_err(BosunError::Io)
}

fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    kbd_enhanced: bool,
) -> Result<()> {
    if kbd_enhanced {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    disable_raw_mode().map_err(BosunError::Io)?;
    execute!(
        terminal.backend_mut(),
        DisableFocusChange,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
    )
    .map_err(BosunError::Io)?;
    terminal.show_cursor().map_err(BosunError::Io)?;
    Ok(())
}

/// Exit with install instructions if no `tmux` binary is on PATH.
fn ensure_tmux_installed() {
    if std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok()
    {
        return;
    }
    eprintln!(
        "bosun requires tmux, but no `tmux` binary was found on your PATH.

Install it and run bosun again:

    macOS:          brew install tmux
    Debian/Ubuntu:  sudo apt install tmux
    Fedora:         sudo dnf install tmux

See https://github.com/yetidevworks/bosun#requirements for details."
    );
    std::process::exit(1);
}

fn print_help() {
    println!(
        "bosun {version} — tmux-native orchestrator for AI agent sessions

USAGE:
    bosun                Launch the TUI (default)
    bosun update         Check for and install the latest release
    bosun update --check Check for an update without installing
    bosun release-notes  Page the bundled CHANGELOG.md
    bosun editor [<cmd>] Print or set the editor launched by `e` in the TUI
                         (e.g. `bosun editor zed`, `bosun editor code`)
    bosun --version      Print version and exit
    bosun --help         Print this message

ENVIRONMENT:
    BOSUN_LOG    Tracing filter (e.g. `info`, `bosun=debug`). Off by default.

See https://github.com/yetidevworks/bosun for full docs.",
        version = env!("CARGO_PKG_VERSION")
    );
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    // Send logs to stderr. During TUI mode this may scramble the alt-screen,
    // so default filter is OFF — enable with BOSUN_LOG=info.
    let filter = EnvFilter::try_from_env("BOSUN_LOG").unwrap_or_else(|_| EnvFilter::new("off"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .try_init();
}
