# Changelog

All notable changes to bosun are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project
uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2.1.9] — 2026-08-23

### Fixed
- **`Ctrl+W` deletes a word in the new-session dialog** instead of
  typing a `w`. In the path field it walks up one directory, so you can
  back out of `~/work/deep/project` without holding backspace. The same
  slip meant any other Ctrl combo the dialog didn't handle typed its
  bare letter into the field — `Ctrl+A` inserted an `a`, and so on;
  modified keys are now ignored there. Thanks to
  [@attilaolah](https://github.com/attilaolah) for the report ([#11]).

[#11]: https://github.com/yetidevworks/bosun/issues/11

## [2.1.8] — 2026-08-23

### Fixed
- **Dialogs no longer show the pane's underlines through them.**
  Opening a dialog over a session containing underlined text drew those
  underlines through the dialog body — and the same went for bold,
  italic and reverse video. Modals, the tab strip and the preview now
  clear a cell before painting over it instead of only setting its
  colours. The dimmed backdrop still shows the real UI behind the
  dialog, attributes and all. Thanks to
  [@attilaolah](https://github.com/attilaolah) for the report ([#12]).

[#12]: https://github.com/yetidevworks/bosun/issues/12

## [2.1.7] — 2026-08-23

### Fixed
- **`~` in a session path works.** Creating a session with a path like
  `~/work` put it in your home directory without the subpath. tmux
  doesn't expand tildes, and when the directory doesn't exist it
  silently starts the session in `$HOME` rather than failing, so the
  mistake was invisible until you looked at where the session had
  landed. Bosun now expands the tilde before the path reaches tmux or
  git — for the new-session modal, for recents saved in the old form,
  and for restarts rebuilt from session metadata. Thanks to
  [@attilaolah](https://github.com/attilaolah) for the report ([#10]).

### Changed
- **Tab always moves between fields in the new-session dialog.** On the
  Path field it used to attempt completion first and only advance when
  there was nothing left to complete — which meant it usually didn't
  advance at all, and you had to press Esc to move on. Completion now
  lives on the Right arrow, matching how a shell accepts an
  autosuggestion: Right takes the highlighted dropdown entry, or
  extends to the longest common prefix. Down, Enter and Esc on the
  dropdown are unchanged.

[#10]: https://github.com/yetidevworks/bosun/issues/10

## [2.1.6] — 2026-08-23

### Fixed
- **Killing your last session no longer raises a spurious list
  warning.** tmux answers a session query with "server exited
  unexpectedly" when the query races the server shutdown that killing
  the last session triggers. Bosun already understood tmux's other
  three ways of saying "nothing to list" but not this one, so it
  reported an error instead of showing an empty sidebar. (Also the
  cause of a red integration job on the 2.1.5 release.)

## [2.1.5] — 2026-08-23

### Fixed
- **Switching is instant again, without the stray keystrokes.** 2.1.4
  stopped the injected newline/Ctrl-D by detaching the preview client
  with SIGHUP, but a tmux client ignores SIGHUP for a while — the old
  client stayed alive ~580 ms after each switch, overlapping the new
  one on tmux's single-threaded server. That brought back the sluggish
  switching and made a focused Shift+Up/Down cycle repaint garbled for
  a second. The client is killed immediately again, and the PTY is now
  closed only after the client has actually been reaped — which is
  what 2.1.3 got wrong and what caused the stray bytes in the first
  place. Fast, and nothing reaches the pane.

## [2.1.4] — 2026-08-23

### Fixed
- **Switching sessions no longer injects a newline or kills the pane.**
  2.1.3's faster embed teardown SIGKILLed the preview's `tmux attach`
  client and closed its PTY abruptly, which made the kernel flush a
  stray newline + Ctrl-D into the *session's* pane. That added a blank
  line to an agent's composer every time you viewed the session, and
  made a bare shell hit EOF and exit — killing the session — when you
  moved the cursor back to the list. The client is now detached
  cleanly (SIGHUP, PTY held open until it exits) with the whole wait
  moved to a background thread, so the pane stays untouched and the
  sidebar stays as responsive as 2.1.3.

## [2.1.3] — 2026-08-23

### Fixed
- **Sidebar keypresses no longer stall for 200 ms.** Every move that
  left a session dropped its embedded `tmux attach` client through
  `portable_pty`'s `kill`, which sends SIGHUP and then polls for exit
  in 50 ms steps for up to 200 ms before escalating to SIGKILL — and a
  tmux client doesn't exit on SIGHUP inside that window. The app loop
  blocked for ~210 ms on every selection change, which is the lag you
  felt moving up and down the list (present since the embedded preview
  arrived in 2.0). The client is now SIGKILLed directly and reaped on a
  background thread: keypress-to-cursor went from ~230 ms to ~15 ms,
  and a burst of 8 rapid presses settles 4 ms after the last key
  instead of half a second.

## [2.1.2] — 2026-08-23

### Fixed
- **Real cursor in the embedded preview.** The preview used to paint
  a gray block glyph at the inner app's cursor while the terminal's
  own cursor stayed hidden, parked wherever the last redraw ended.
  Mosh-based clients (Moshi on iOS) tracked that hidden, wandering
  cursor and, after a tap-to-position in the Claude Code input box,
  left ghost cursors across the output until a Ctrl-L repaint. Bosun
  now drives the terminal's actual cursor to the inner cursor cell
  (honouring the inner app's hide/show state), so you also get your
  terminal's native cursor shape and blink instead of a block.
- **Narrow layouts no longer shrink sessions to 20×3.** On a phone-
  width terminal the unfocused layout has no preview area, but bosun
  still attached a minimum-size embed to whichever session the cursor
  was on — and because that attach takes part in tmux's size
  negotiation, every session you scrolled past was resized to 20×3,
  then reflowed back up when you landed on it. No embed is spawned
  until there's somewhere to draw it; Enter still attaches full-body.

### Changed
- **Sidebar navigation is much cheaper.** Moving through the session
  list cost ~10 tmux processes per keypress (a full kill + attach of
  the embed, a capture-pane for every managed session, three
  `bind-key` self-heals, and a full status-bar rewrite every time
  tmux's attached flag flipped). An isolated keypress still attaches
  instantly, but holding Up/Down now shows cheap `capture-pane`
  snapshots and attaches once the cursor rests; `FocusPreview` only
  captures the selected session; the `window-size` reset runs once
  per session; and the status bar is only rewritten when membership
  or names change. 18 rapid presses across four sessions went from
  278 tmux execs to 95 at desktop width and 74 at phone width.

## [2.1.1] — 2026-08-18

### Changed
- **Friendly error when tmux is missing.** Bosun now checks for a
  `tmux` binary on PATH before starting the TUI and exits with
  per-platform install instructions instead of a bare io error after
  the terminal has already switched to the alternate screen.
- **tmux is documented as a prerequisite.** The README's Requirements
  and Installation sections now spell out that bosun drives tmux but
  does not bundle it, with install commands for macOS and Linux, and
  the Homebrew formula declares `depends_on "tmux"` so
  `brew install yetidevworks/bosun/bosun` pulls it in automatically.

## [2.1.0] — 2026-08-10

### Added
- **OpenCode is a first-class agent.** Pick `opencode` in the
  new-session modal and get a New/Continue session radio (`--continue`
  reopens the working directory's last session) plus an Auto checkbox
  (`--auto`, opencode's auto-approve mode). Restart's one-shot `r`
  resume, the recents store, modify, and live status detection all
  work the same as for the other agents.
- **Qwen Code is a first-class agent.** Pick `qwen` and get the full
  New/Continue/Resume radio — `--continue` reopens the working
  directory's most recent session, `--resume` opens Qwen's session
  picker — plus a YOLO checkbox (`--yolo`). Same restart/recents/
  modify/status-detection coverage as OpenCode.
- **Per-agent binary overrides.** A new `[agents]` table in
  `config.toml` maps an agent to the executable that launches it, e.g.
  `opencode = "/Users/me/bin/opencode-wrapper"` — handy for wrapper
  scripts that fix up the environment (say, unset `GIT_EXTERNAL_DIFF`)
  before exec'ing the real binary, or for differently-named installs
  like `codex-nightly`. Unset agents keep resolving their own name on
  the login-shell PATH.
- **Codex sessions can now be continued and resumed.** Codex grew a
  real `resume` subcommand since bosun first wired it, so the codex
  options gain the same session radio as Claude: Continue launches
  `codex resume --last`, Resume launches bare `codex resume` (its
  interactive picker).

## [2.0.17] — 2026-08-08

### Added
- **Claude Code sessions are named after the bosun session.** Launch
  and restart now pass `--name <slug>` (the display name, slugified —
  `My Rocket Fox` → `my-rocket-fox`) so `claude --resume`'s picker and
  the claude.ai session list show the same name as the bosun sidebar
  instead of an auto-generated one. A `--continue` restart re-asserts
  the name on the resumed session. If you put your own `--name` in the
  extra-args field it wins and bosun stays out of the way. Requires a
  Claude Code CLI new enough to know `--name`; codex and kimi have no
  equivalent flag and are unchanged.

## [2.0.16] — 2026-07-31

### Added
- **A new `◆ Done` status.** A session that finished a turn you haven't
  read yet now renders distinctly from one that's merely sitting at an
  empty composer — "ready for review" versus "nothing pending". It
  drops back to `○ Idle` the moment you select the row. Running and
  Waiting still win, so a session asking a question never hides behind
  it. Every built-in theme gets a `status_done` colour; user themes
  that predate the key keep loading and fall back to the accent.
- **The `?` help dialog now opens with a legend.** Two new sections
  ahead of the key bindings: every status glyph with what it means, and
  every row marker (unread dot, selection bar, tab count, background
  activity, attached tag). Glyphs are drawn in the colour they actually
  appear in on the session list. The dialog is retitled
  "Bosun · Reference" now that it's more than key bindings.
- **Status detection reads what the agent says about itself.**
  `list-sessions` now also pulls `#{pane_title}` and
  `#{pane_current_command}`. Claude Code publishes its state in the
  pane title (a braille spinner frame while a turn runs, `✳` when it
  doesn't) and renames its process to its own version (`2_1_220`), so
  both are far steadier signals than scraping pane text. Bosun prefers
  them and keeps the old heuristics as fallback. Costs nothing — the
  fields ride the poll we already do.

### Fixed
- **The unread dot strobed on and off on an idle session.** Two
  independent bugs, both visible on a Claude session showing the
  backgrounded-task list:
  - The dot was recomputed live each tick by comparing the pane's
    fingerprint against the baseline, so it *cleared itself* whenever
    the pane happened to return to exactly the baseline text. Whether
    something has happened since you looked is a question about
    history, so it's now latched — set on the first change and cleared
    only by viewing the row (or by a reflow re-baseline).
  - The fingerprint covered animated decoration. Claude cycles a
    spinner glyph (`✢ ✽ ✳ ✻ ✶ ·`) about twice a second and ages
    relative timestamps (`3m` → `4m`); sampled at 1Hz the hash
    random-walked over a handful of values, one of which was the
    baseline. Spinner glyphs, elapsed-time counters, and whitespace
    runs are now normalised out before hashing. Replaying the real
    captures that caused the report: 12 frames that produced 6
    distinct fingerprints now produce 1.
- **A session showing Claude's backgrounded-task list no longer picks
  up other sessions' state.** That screen lists tasks from every
  session, spinners and all; the detector now keys off this pane's own
  title instead, and recognises the screen as Claude via the process
  name (it has none of the usual prompt-box anchors).
- **The git unit tests no longer fail at random.** Their throwaway
  fixture repos inherited a global `commit.gpgsign = true` and dragged
  gpg-agent into every fixture commit, so the suite failed
  intermittently on whichever test ran when the agent was unhappy.
  Signing is now forced off for those repos.

## [2.0.15] — 2026-07-17

### Added
- **Kimi (Moonshot Kimi Code) is now a first-class agent.** Pick `kimi`
  in the new-session modal alongside claude / codex / terminal. It gets
  its own options block: a **session mode** radio (New / Continue /
  Resume, mapping to `--continue` / `--session`) and a **YOLO mode**
  checkbox (`--yolo`, auto-approve all actions). The launch command uses
  the `kimi` binary. Session mode and YOLO are persisted per session
  (so restart rebuilds the exact invocation) and remembered in recents
  (so the modal pre-fills from your last kimi session). Dead kimi rows
  restart with the same one-shot `r` "resume" action as claude/codex,
  and a dedicated status detector reports Running / Waiting / Idle glyphs
  for kimi panes.
- **bosun's tmux server now uses the `csi-u` extended-keys format.** Set
  globally (`extended-keys on` + `extended-keys-format csi-u`) on the
  dedicated `-L bosun` socket when a session is created, so Kimi Code
  stops warning about the default `xterm` format. Scoped to bosun's
  server — your default tmux is untouched — and harmless for
  claude/codex.

### Fixed
- **New / restarted sessions landed at a bare shell and never launched
  the agent (tmux 3.7).** The embedded preview attached to each session
  as a *read-only* client (`tmux attach -f read-only`). tmux 3.7 made
  `send-keys` refuse to target a session whose resolved client is
  read-only, failing with `client is read-only` — so while a session was
  merely being previewed (the normal case), *every* keystroke bosun sends
  it silently failed: the agent-launch command on a fresh create or
  restart, the `C-c` that stops a running agent, the redraw `C-l`. The
  agent never started until the user manually attached a read-write
  client (focus mode / full `tmux attach`), which is exactly the "it does
  nothing until I attach, then the keys come through" behaviour. Older
  tmux ignored the read-only flag for `send-keys`, so this surfaced as a
  regression after a tmux upgrade.

  The preview now attaches **read-write** like focus mode. The read-only
  flag was only belt-and-braces — bosun never forwards sidebar input to
  the embed unless it's focused (every `EmbedTerminal::write` call site is
  gated on `embed_focused`), and window-size negotiation behaves the same
  read-write — so dropping it restores working `send-keys` with no change
  to how previews look or size.
- **Hardened the launch keystroke against slow shells / `magic-enter`.**
  Independently of the read-only bug, `restart_in_place` now sends the
  command **and its Enter as one atomic literal** (a trailing carriage
  return in the same `send-keys -l`) instead of typing the command and
  pressing Enter as two separate keystrokes. On a heavy `~/.zshrc` the
  command text could be swallowed while the shell was still initialising
  and the separate Enter would land on an empty line — which an oh-my-zsh
  `magic-enter` binding turns into a stray `ls`. Atomic send makes that
  impossible, and the launch now confirms the agent actually started and
  re-sends if a slow shell dropped it (up to three attempts).
- **No more blank flash entering / leaving focus.** Now that preview and
  focus attach with identical args, the preview→focus handoff no longer
  drops and re-attaches the embed (which blanked the pane for a frame
  while the new attach repainted). It resizes the live embed in place
  instead — the vt100 grid keeps its content and just reflows around the
  focus border, so the transition is a single seamless repaint.
- **Fresh agent launches could break the agent's own `PATH` (`command
  not found`).** The deferred launch (`restart_in_place` on a freshly
  created pane) opened Phase 1 with an unconditional `C-c` meant to stop
  a running agent — but a fresh create has no agent, only a login shell
  that may still be sourcing a heavy `~/.zshrc`. The shell-ready poll
  couldn't tell "at prompt" from "mid-init" because `pane_current_command`
  reads `zsh` throughout an rc run, so that `C-c` landed inside `.zshrc`
  and SIGINTed it partway. Any `PATH` entry appended late in the rc
  (Kimi's `~/.kimi-code/bin` sits on the very last line) never got added,
  so the agent binary wasn't found and the pane sat at a bare shell with
  a `command not found`. Intermittent by nature — it only bit when the
  interrupt fell inside the rc's slow stretch (compinit, plugin managers),
  so a brand-new terminal always worked. The launch now passes
  `kill_first = false`: the interrupting `C-c` fires only for a genuine
  stop (a live agent), never for typing into a known bare shell. The
  atomic `cmd\r` already buffers and runs once the prompt is truly ready,
  so `.zshrc` finishes intact and the agent resolves.

## [2.0.13] — 2026-07-01

### Added
- **Optional git worktree sessions (issue #3).** Create a session
  inside a fresh git worktree on a new branch. The new-session modal
  gets a **Create in git worktree** checkbox (`Space` toggles) that
  reveals a branch field, prefilled from the session name and
  validated. Placement follows a new `worktree_location` config key —
  `subdir` (`<repo>/.worktrees/<branch>`, the default) or `sibling`
  (`<repo>-<branch>`). On kill, a worktree session offers `m` merge &
  remove, `x` remove (keep the branch), or `Enter` keep; a dirty
  worktree is never removed, and a conflicted merge is aborted rather
  than left half-applied. The worktree path and branch persist as
  `@bosun_worktree_path` / `@bosun_branch`, so they survive a tmux
  server restart. Thanks to @johnuopini for the contribution (PR #8).
- **In-progress feedback for session operations (issue #7).** When a
  create, kill, or restart is dispatched, the UI now shows it
  immediately instead of looking frozen until the next refresh lands.
  Kill and restart swap the affected row's status glyph for a `⟳`
  marker and trail a `killing…` / `restarting…` label; a create — which
  has no row yet — shows `⟳ creating <name>…` in the status bar. Each
  marker clears the instant its result lands (the row vanishes, the
  create-completion refresh arrives, or a warning reports back), with a
  10s backstop so a wedged operation can't leave a spinner stuck. The
  gap was most noticeable for git worktree creates, which also shell out
  to `git` before the session appears.

## [2.0.12] — 2026-06-29

### Added
- **Opt-in group name in the tab strip and terminal title (issue #4).**
  When `show_group_in_title = true` (or `BOSUN_SHOW_GROUP_IN_TITLE=1`),
  sessions that belong to a section render as `group/session` in both
  the tab pills above the embedded terminal and the OSC window title
  (`bosun — group/session`). Ungrouped sessions stay bare. Off by
  default, display only — no persistence or new session state. Thanks
  to @johnuopini for the contribution (PR #6).

## [2.0.11] — 2026-06-20

### Fixed
- **Restart (`Shift+R`) now waits for the embedded terminal too, so
  Codex/Neovim still detect a light background after an in-place restart
  (issue #2).** The 2.0.10 deferral only covered freshly-created
  sessions; `Shift+R` rebuilt and relaunched the agent immediately,
  before the embed's OSC 10/11/12 responder was guaranteed live, so a
  cold-start restart could still cache a dark diff palette. Restart now
  stops the agent to a bare shell and defers the relaunch through the
  same gate as create.
- **The deferred launch now waits for the embed to actually attach, not
  just for it to be spawned.** Previously the agent launched as soon as
  `EmbedTerminal::spawn` returned, but that only forks `tmux attach` —
  it doesn't mean the client has connected and is relaying. On a slow
  attach the agent could still probe a pane with no responder behind it.
  The launch now holds until the embed reports its first bytes (proof
  the attach is live), with a 20s fall-back so a hung attach can't
  strand a bare shell. This closes the slow-attach race on both the
  create and restart paths.
- **Restart no longer fires the shell prompt's `precmd` hooks before
  relaunching.** The split restart was prepping the shell line twice and
  submitting empty lines, so a prompt with a baked-in `git status` ran
  it two or three times (with stray blank prompts) on every `Shift+R`.
  The stop step now only kills the agent, and the launch step clears the
  line without submitting an empty Enter.

## [2.0.10] — 2026-06-08

### Fixed
- **Codex/Neovim now detect a light terminal background on freshly
  created sessions (issue #2).** The OSC 10/11/12 responder added in
  2.0.5 only existed once a session's embedded terminal had attached,
  but a new session typed its agent command the instant the pane was
  created — seconds before the embed, on a cold start. Codex probes the
  background within ~100ms and permanently caches a miss, so it fell
  back to its dark diff palette. Bosun now creates the pane as a bare
  shell and defers the agent launch until the embed (and its
  background-color responder) has attached, so the startup probe gets a
  real answer. Only applies when the embedded terminal is enabled; with
  it off the agent launches inline as before.

## [2.0.9] — 2026-06-08

### Added
- **Restart can resume the agent's last conversation.** The `Shift+R`
  restart prompt now offers an `r` action (alongside the usual `y`/`n`)
  for claude and codex sessions, on both the live restart and the
  recreate-from-recents (dead session) prompts. Plain restart relaunches
  with the saved config; `r` restarts into the agent's resume invocation
  for that one launch only — claude `--continue`, codex `resume --last`
  — without changing the session's stored spec, so the next plain
  restart goes back to the saved mode.
- **Double-click a session to attach.** Double-clicking a row in the
  session list now attaches to it, the same as pressing Enter. A single
  click still just moves the selection.

## [2.0.8] — 2026-06-03

### Fixed
- **Unread dots no longer fire on layout changes.** The unread
  fingerprint is now keyed on each session's pane width, so a reflow is
  treated as layout instead of new output: resizing the terminal,
  moving the selection (which sizes the focused pane to the preview
  area), re-attaching from a different-size device, or a second bosun
  instance attaching to the shared tmux server. Previously any of these
  re-wrapped the captured text and lit sessions as unread with no agent
  activity — connecting from a phone could mark everything unread, and
  just moving off a session could mark it unread. Crucially, one bosun
  instance resizing a shared pane can no longer flip another instance's
  dots. The fingerprint also ignores per-line trailing whitespace and
  blank rows, so pane-width padding doesn't perturb it.

## [2.0.7] — 2026-06-01

### Added
- **`opencode` banner font, now the default.** A custom lowercase pixel
  font modeled on the opencode CLI logo: square-cornered glyphs with a
  5-row body and 1-row ascender/descender legs, rendered with half-block
  cells so it sits at the same compact scale as the other banner fonts.
  It ships as a color TheDraw font (`themes/banners/opencode.tdf`) and is
  the new default for section headers and the empty-state splash; press
  `f` to cycle to the previous fonts (`eatmex`, `metalix`, …). Generated
  by, and tweakable via, `scripts/gen_opencode_tdf.py`.

## [2.0.6] — 2026-06-01

### Added
- **Unread indicator for sessions with unviewed activity.** Each
  sidebar row now shows a red notification dot in the gutter and a
  bold name when that session's pane has changed since you last looked
  at it — a finished turn, a permission prompt, a question, any new
  output. It works by fingerprinting each session's visible text on
  the refresh bosun already runs (no extra tmux calls) and comparing
  against the snapshot from the last time the row was selected.
  Selecting a row marks it seen and clears the dot; it lights again if
  the session keeps changing while you're looking elsewhere. New rows
  and failed captures never start unread.

### Fixed
- **A fresh or idle Claude no longer shows as "waiting" (◐).** Recent
  Claude versions use `❯` as the composer's input glyph, which the
  detector read as a confirmation menu — so an untasked instance
  sitting at its prompt looked like it needed an answer. `Waiting` is
  now limited to genuine decision points (numbered `❯ 1.` menus,
  `Do you want to…`, `(y/n)`); an empty or typed-but-unsubmitted
  composer reads as `Idle`.

## [2.0.5] — 2026-05-29

### Added
- **Hide / show the sidebar with `Ctrl+B`.** While focused on a
  session, `Ctrl+B` collapses the sidebar so the embedded pane takes
  the full width, and brings it back. The preference is sticky
  (persisted to `config.toml` as `sidebar_hidden`) and only applies
  while focused — detaching with `Ctrl+Q` always restores the sidebar
  so the session list stays reachable, and re-focusing re-applies your
  choice. Documented in the help cheat-sheet.
- **Terminal background detection for embedded agents.** Apps like
  Codex and Neovim probe the terminal's default background (OSC 10/11)
  at startup to pick a light vs dark palette. Inside bosun those
  queries dead-ended at the embed's parser, so Codex assumed a dark
  background and rendered diffs with dark colors on a light terminal.
  bosun now probes the real outer terminal for its fg/bg/cursor at
  startup and answers those queries on the embedded session's PTY
  (falling back to the active theme's colors if the terminal doesn't
  report). Fixes [#2](https://github.com/yetidevworks/bosun/issues/2).

### Changed
- **The add-tab `+` button now sits beside the last tab.** It used to
  float at the far right edge of the tab strip; it now follows the
  last tab directly and renders on a slightly raised background so it
  reads as a small button instead of getting lost.

### Fixed
- **Fast flicker in Ghostty and other focus-reporting terminals.** The
  2.0.3 `Cmd+R` recovery re-enabled focus reporting on every focus
  gain; terminals that echo their focus state when reporting is
  enabled (Ghostty) looped — recover → echoed FocusGained → recover —
  as a continuous full-screen flicker. Recovery now runs only on a
  genuine lost→gained transition, so the echo is swallowed. iTerm's
  `Cmd+R` repaint still works.
- **Unreadable colors in terminals without 24-bit color.** bosun's
  themes are all true-color, which Apple Terminal can't render, so its
  UI came out garbled and often illegible. On a terminal that doesn't
  advertise `COLORTERM=truecolor`, bosun now down-samples every color
  to the nearest xterm-256 palette entry in a final pass over the
  frame. Modern terminals (iTerm2, Ghostty, Warp, WezTerm, …) are
  unaffected and keep full 24-bit color. `BOSUN_TRUECOLOR=1`/`0`
  overrides the detection.

## [2.0.4] — 2026-05-28

### Changed
- **Dropped the embedded pane's tmux `status-left`.** The `⚓ bosun`
  chip and the session name are redundant once you're focused: the
  session name already shows in bosun's tab strip and the brand in
  its own TUI footer, so the pane's left status segment was just
  noise fighting the agent UI. The right-side key hint
  (detach / cycle / jump) stays.

### Fixed
- **Narrow / mobile embed now fills the full pane width.** Focused
  mode reserved a one-cell focus-border inset on every side even on
  narrow terminals — where no border is actually drawn — leaving
  dead padding down the left and right (and top/bottom). The embed
  now renders edge-to-edge in that layout, with the PTY size and
  mouse hit-testing reclaiming the same cells so everything stays
  aligned.

## [2.0.3] — 2026-05-28

### Added
- **Word-wise editing in the focused embed.** `Option+Delete`
  deletes the previous word and `Option+Left`/`Option+Right` move
  the cursor by word inside the embedded session. bosun now emits
  the readline word-motion bytes (`ESC \x7f` for delete-word,
  `ESC b`/`ESC f` for word-left/right) that zsh, bash, and Claude
  Code honor, instead of the xterm `\e[1;3D` Alt-arrow form those
  apps ignore.
- **Kitty keyboard protocol at startup.** bosun requests the
  `DISAMBIGUATE_ESCAPE_CODES` enhancement (gated on
  `supports_keyboard_enhancement`, so it's a no-op on terminals
  that don't speak it). This is what makes `Option`+key chords
  arrive with their modifier bits intact; without it the outer
  terminal falls back to legacy encoding and hands bosun bare
  keys, so word-motion and word-delete couldn't be distinguished
  from a plain arrow or backspace. Popped and re-pushed around the
  full-screen `tmux attach` path and on panic.
- **`BOSUN_KEYLOG` debug facility.** Setting the env var appends
  every focused-mode key event (code, modifiers, kind) to
  `/tmp/bosun-keys.log` — useful for diagnosing how a given
  terminal encodes a chord without a rebuild.
- **`Ctrl+L` redraw and focus-gain recovery.** `Ctrl+L` forces a
  full repaint — re-enters alt screen, re-arms mouse / paste /
  focus reporting, re-pushes the keyboard flags, and clears
  ratatui's cached frame — and still forwards `^L` to the inner
  shell. bosun also listens for focus events now, so switching
  back to the window after iTerm's `Cmd+R` "reset" repaints
  automatically. The help cheat-sheet documents both, plus the
  Option word-motion / word-delete keys.

### Fixed
- **syspolicyd CPU storm from the fast preview tick.** The fast
  status tick was running `tmux capture-pane` for *every* managed
  session every ~200ms (`1 + N` short-lived `tmux` execs per tick).
  At a dozen sessions that's ~60 process spawns a second per bosun
  instance, and because macOS Gatekeeper re-scans every exec of an
  ad-hoc-signed binary (Homebrew's `tmux` included) without caching
  the verdict, `syspolicyd` could peg every core. The fast tick now
  captures only the *focused* session — the one whose preview you're
  watching — and skips entirely when nothing is focused; background
  sessions keep their 1Hz status refresh. Per-instance fast-tick
  exec rate drops ~6×.
- **Unreadable text on accent-colored chips.** The "bosun" status
  chip and the active tab (top strip and sidebar pill) drew light
  text on the theme accent, which was low-contrast on light accents
  (tokyonight) and illegible in some terminals. A new luminance-aware
  `Theme::on` picks near-black or near-white ink per the accent's
  brightness, so the label stays readable across every built-in and
  user theme.

## [2.0.2] — 2026-05-28

### Changed
- **Single-window focused mode is now the only mode.** The `s`
  toggle, the `Command::SaveSingleWindow` persistence, the
  `BOSUN_SINGLE_WINDOW` env var, and the legacy full-screen
  `tmux attach` path are all gone. `Enter` on a session always
  opens it in the embedded focused PTY; `Ctrl+Q` always brings
  you back to the sidebar. Result: consistent keybindings — tab
  cycle, session cycle, modals — regardless of where you came
  from, and no more "bosun is paused while you're attached" trap
  where bosun couldn't intercept keys.
- **Plain `→`/`←` cycle tabs** within the selected container in
  sidebar mode (no-op on single-tab containers). `Enter` is now
  the only attach key; the old "Right also attaches" muscle
  memory collided with arrow-key navigation. `Shift+→/←` and
  `] [` still cycle tabs too, for parity with focused mode.

### Added
- **Mobile / narrow-terminal focused mode.** Below
  `PREVIEW_MIN_WIDTH` (80 cols), `Enter` on a session hands the
  entire body to the embed — sidebar hidden, embed full width —
  so bosun's key handlers stay alive on a phone via mosh.
  `Ctrl+Q` detaches back to the sidebar. Previously narrow
  mode silently disabled focused mode, dropping users into a
  full-screen `tmux attach` where bosun couldn't intercept tab
  chords.
- **Tab list under multi-tab containers in narrow mode.**
  Without the preview pane visible, the tab strip wasn't
  reachable. The sidebar now renders a third line per container
  listing each tab, with the active tab styled like the `bosun`
  chip, so tab membership and the active tab are visible at a
  glance on mobile.

### Fixed
- **Sidebar `Shift+arrow` matching with extra modifier bits.**
  Some mobile SSH clients ship Shift+arrow with extra flags
  (`SHIFT|KEYPAD`, `SHIFT|ALT`, etc.). The sidebar handler used
  exact `KeyModifiers::SHIFT` matching and silently dropped
  those, even though the focused handler caught them via
  `modifiers.contains(SHIFT)`. Sidebar now normalises arrow
  events to just SHIFT/CONTROL before matching, bringing the
  two paths in line.

## [2.0.0] — 2026-05-28

The 2.0 release turns bosun from a session *picker* into a session
*workspace*. The preview pane is now a real embedded terminal, the
focused session is interactive from inside bosun, and each sidebar
row can hold multiple tabs.

### Added — Embedded terminal preview

- **Live embedded preview.** The selected session renders from a
  real PTY (`portable-pty` + `vt100` + `tui-term`), not a polled
  snapshot. The vt100 parser is primed with a `capture-pane`
  snapshot on every switch so the first frame is correct — no
  multi-second scrollback replay animation.
- **Single-window focus mode (`s` toggles, persisted).** Press
  Enter on a session and it opens *inside* the preview pane in
  writable mode; the sidebar stays visible. `Ctrl+Q` exits focus,
  same chord as the classic full-screen detach.
- **Focus border.** Accent-colored 1-cell outline around whichever
  pane has the keyboard. Single-window mode reserves the border's
  space on both panes so the layout doesn't shift when focus
  toggles.
- **Two-way mouse navigation.** Click a session row to jump and
  exit focus. Click inside the preview to enter focus on the
  selected session. The triggering click isn't forwarded into the
  embed; subsequent clicks pass through.
- **Input correctness in the embed.** DECCKM cursor-key
  application mode, modifyOtherKeys for Shift+Enter / Ctrl+Tab /
  Ctrl+Backspace, bracketed paste forwarding (drag-drop an image
  → Claude Code sees `[Image #N]`), SGR 1006 mouse forwarding for
  click-to-cursor and scrollback.
- **Divider drag survives the cursor crossing into the preview**
  even when the inner app has mouse tracking on.

### Added — Tabs (multi-session containers per sidebar row)

- **Container abstraction.** Each sidebar row is now a `Container`
  that owns 1..N tmux sessions ("tabs"). Single-tab containers
  behave identically to the pre-2.0 single-session rows.
- **Browser-style tab strip.** Pill tabs above the embed (always
  visible while a container is selected so the `+` is always
  reachable). Active tab uses the same accent-chip styling as the
  `bosun` / `SW` status-bar pills. Each tab carries a per-tab
  status glyph so background tabs surface Running / Waiting state
  without focus.
- **`(N)` tab-count badge** on multi-tab sidebar rows; small
  accent dot when any non-active tab is busy.
- **Add-tab modal.** `Ctrl+T` (sidebar) or click the `+` button
  opens a slimmed new-session modal with the path field locked to
  the container's path. Submit stamps `@bosun_container_id` onto
  the new tmux session so siblings regroup correctly after a tmux
  server restart.
- **Auto-detach when opening the modal from focused mode** so
  keystrokes reach the form; auto-restore focus on the new tab
  after submit (or the original tab on Esc).
- **Tab keybindings.** `Shift+→ / ←` cycles the active tab,
  `Shift+↓ / ↑` cycles sessions in sidebar order — identical
  chords in both sidebar and focused-embed modes. Sidebar-only
  `]` / `[` mirrors the tab cycle. `Shift+D` kills the whole
  container; plain `d` kills the active tab (drops the container
  when the last tab goes).
- **Tab-strip windowing.** When tabs overflow the available
  width, the strip slides so the active tab stays visible —
  earlier tabs scroll off the left edge instead of being silently
  dropped.

### Added — Sessions / sections

- **Sections** for grouping sessions into named, collapsible
  buckets (`g` to create, `Tab` to collapse). Persisted in
  `config.toml`. Banner fonts cyclable per-section (`f` on a
  header).
- **Modify-session modal (`m`).** Pre-fills from the highlighted
  session's stored `@bosun_*` metadata. Save-only: the running
  agent keeps its current flags; the next `R` (restart) picks up
  the new spec.
- **Editor key (`e`).** Opens the session's path in your
  configured editor. Set with `bosun editor <cmd>`.
- **Sidebar-order session cycle** in both sidebar and focused
  modes (was added as Shift+→/← in focused mode in 2.0, then
  moved to Shift+↓/↑ to free Shift+→/← for tab cycling).
- **Live per-session status detection at the fast tick.** The
  `●◐○✕` sidebar glyphs update at ~5 Hz across **every** managed
  session (not just the focused one), driven off the same fast
  cadence as the preview. Claude and Codex detectors rewritten to
  scope substring scans to the bottom ~12 visible lines and to
  recognize Claude's box-drawn prompt directly — kills the "stale
  Thinking… pegs Running forever" failure mode.

### Changed

- **Sidebar persistence is now eager.** `SidebarModel::reconcile`
  reports whether it mutated the model; the `SessionsRefreshed`
  handler saves `config.toml` whenever a fresh session shows up
  via reconcile (not just on explicit organize actions). Fixes
  the long-standing bug where users who never reorganized would
  find `config.toml` had only `theme = "..."` and no `[sidebar]`
  block — after a reboot the sidebar would come up empty instead
  of preserving dead rows for re-attach.
- **Shift-arrow chord layout.** Plain `Shift+arrows` is now
  reserved for tab/session navigation (`Shift+→/←` tabs,
  `Shift+↓/↑` sessions) — same in sidebar and focused modes. The
  reorder / move-to-bucket actions moved to `Ctrl+Shift+arrows`
  (with `Shift+J/K` still working for vim-style row reorder).

### Fixed

- Mouse coordinates inside the focused embed were one row + one
  column off because the click-to-local-coord translation used
  the outer preview rect instead of the focus-border-inset embed
  rect. Click-to-cursor and drag-to-select now land on the actual
  click position.
- Focus border drawing through the tab strip (border now starts
  one row below the preview rect when a tab strip is shown).
- Sidebar's first-row title getting clipped by the focus border
  in single-window mode (sidebar content insets by one cell so
  the border has its own perimeter).
- Add-tab modal: `name` field starts empty instead of pre-filling
  with the container's internal tmux name.
- Clicks outside an open modal could activate the background
  preview pane — focus enter / click-out / tab-strip click paths
  are all gated on `modals.is_empty()` now.

### Internal

- New `Container { id, name, members, active }` struct;
  `SidebarModel.ungrouped` and `Section.members` migrated from
  `Vec<String>` to `Vec<Container>`. `VisibleEntry` carries
  `&Container`. Backwards-compat `#[serde(untagged)]` deserialize
  so 0.x configs (bare-string members) load unchanged and save
  in the new table form on first write.
- New `@bosun_container_id` tmux user option carries the
  container assignment across tmux server restarts.
- `tab_strip` module owns layout + render + click hit-test
  (pure-function `compute` shared by render and `app::run`).
- `AttachMode::Preview` vs `Focused` selection now flows through
  `sync_embed` based on `embed_focused` so an active-tab change
  while attached respawns in the right mode automatically.

## [0.4.1] — 2026-05-27

### Fixed

- Synchronous `capture_pane` on attach exit so the preview snapshot
  is current the moment focus returns to bosun — a stale snapshot
  used to flash for one tick before the next refresh overwrote it.

## [0.4.0] — 2026-05-27

### Added
- **Live-feeling pane preview (focused-session fast tick).** The
  preview pane on the selected session now updates at 5 fps by
  default instead of 1 Hz. A second timer inside the tmux actor
  re-captures only the focused pane on a fast cadence and emits a
  lightweight `PreviewRefreshed` update that bypasses the
  session-list / status-detector / statusbar paths — so the cost
  is one `capture-pane` per tick regardless of how many sessions
  you have open. The full 1 Hz refresh that updates the session
  list and the `●◐○✕` status glyphs is unchanged.
- Configurable via `preview_tick_ms = 250` in `config.toml` or
  `BOSUN_PREVIEW_TICK_MS=300` in the env. Set to `0` to disable
  the fast tick entirely and fall back to v0.3.x behavior.

### Internal
- New `AppMsg::PreviewRefreshed { name, bytes }` variant carries
  the lightweight payload from the actor to the app. The app
  handler updates the matching `SessionView.preview` in place and
  returns no commands — no sidebar reconcile, no statusbar diff,
  no detector run on the hot path.
- This is the Step 0 deliverable from the in-progress 2.0
  embedded-terminal work (see the `2.0` branch). Validated in live
  testing as a clear improvement over the v0.3.x 1 Hz preview but
  *not* a full substitute for a real embedded terminal — that work
  continues on the `2.0` branch and will land later.

## [0.3.11] — 2026-05-27

### Added
- **Open session path in an external editor (`e` key).** With a session
  highlighted on the main list, press `e` to launch your configured
  editor (`zed`, `code`, `subl`, `nvim`, etc.) against the session's
  working directory. The editor is set with the new `bosun editor
  <cmd>` CLI subcommand (e.g. `bosun editor zed`) and persisted to
  `config.toml` as `editor = "zed"`. Run `bosun editor` with no
  argument to print the current value, or `bosun editor ""` to clear
  it. Status bar now includes `e edit`, and the `?` help modal lists
  the binding under Sessions.

## [0.3.10] — 2026-05-25

### Added
- **Key-bindings help modal.** Press `?` or `h` on the main list to
  open a scrollable cheat sheet covering every binding in bosun —
  navigation, session ops, reorder/move, sections, modals, and the
  `Ctrl+Q` detach inside an attached session. Scroll with arrows /
  PgUp / PgDn / Home / End; Esc or Enter closes. The status bar now
  ends with `? help` so the key is discoverable.

## [0.3.9] — 2026-05-23

### Fixed
- **Restart unreliable: empty shell prompt, stuck "claude " text, stale
  preview.** Three different failure modes, all caused by the same
  root issue — the prior implementation guessed at timing with fixed
  sleeps. The worst case was the trailing `C-l` firing while the pane
  was still in zsh (claude hadn't finished launching yet), which
  cleared the shell's screen instead of forcing the agent to repaint,
  leaving capture-pane staring at an empty starship prompt.
  `TmuxClient::restart_in_place` now polls `#{pane_current_command}`
  at the two state transitions that matter:
  1. After `C-c`, wait until the foreground process is a shell again
     (resending `C-c` periodically while the agent is still up).
     Hard timeout 3.5s.
  2. After typing the launch command, wait until the foreground
     process is no longer a shell — i.e. the new agent is actually
     running. Only then send `C-l`. Hard timeout 6s.
  Restart now waits for the state it needs instead of hoping a sleep
  was long enough, so it works under cold-start npm spin-up, slow
  rc files, and busy agents alike.

### Internal
- New `TokioTmuxClient::pane_current_command` helper backed by
  `display-message -p '#{pane_current_command}'`.
- `restart_in_place` is split into explicit kill / prep / launch /
  wait-for-agent / redraw phases, each with its own deadline.

## [0.3.8] — 2026-05-20

### Fixed
- **Restart misses on stubborn agents.** Send `C-c` three times now
  (180ms → 220ms → 400ms) instead of twice. The first dismisses an
  open confirm dialog, the second tells the agent we mean it, the
  third covers codex `--yolo` and claude with deep nested tool
  calls that catch and discard the first two.
- **Preview stuck at pre-restart state until attach + detach.** Two
  things were going wrong:
  - Many TUI agents draw onto the alternate screen and only fully
    repaint when prompted with a WINCH or a form-feed. In a detached
    tmux pane that signal never arrives, so `capture-pane` kept
    returning a half-painted buffer until the user attached and
    detached manually to force the redraw. Restart now sends `C-l`
    after the agent has had ~450ms to claim the pane.
  - The preview was waiting up to a full 1s `preview_tick` for the
    next capture. The actor now bursts three refreshes (at +200ms,
    +600ms, +1200ms post-restart) so the preview tracks the agent's
    splash paint in real time.

### Internal
- `TmuxClient::restart_in_place` sends C-c × 3 with larger gaps and
  appends a trailing `C-l` after the launch.
- `Command::RestartSession` handler in the tmux actor now fires three
  spaced `do_refresh` calls after the restart instead of one.

## [0.3.7] — 2026-05-19

### Fixed
- **Restart-in-place was dropping the first character of the launch
  command** under async prompts (powerlevel10k, spaceship) and after
  agents that print a multi-line shutdown message. The `send-keys -l`
  arrived while zsh's line editor wasn't ready yet, so `claude …`
  showed up as `laude: command not found`. Restart now does
  `C-c → wait → C-c → longer wait → Enter → C-u → command`, which
  forces the prompt to repaint and clears any residue before the
  command lands.
- **Homebrew release workflow** now pulls release tarballs via
  `gh release download` from the just-created release page instead
  of `actions/download-artifact@v4`, which was intermittently 403ing
  on `ListArtifacts` when `update-formula` and `publish-crate`
  raced against the same artifact API endpoint after `release`.

## [0.3.6] — 2026-05-19

### Changed
- **`R` on a live session now restarts in place.** Instead of killing
  the tmux session and creating a new one with a fresh internal name,
  `R` sends `Ctrl-C` twice (covers agents that swallow the first
  interrupt to confirm) and then re-types the launch command in the
  same pane. The session, its internal name, and the sidebar slot are
  all preserved — no ghost rows, no jump to the end of the list, no
  selection bounce.

### Fixed
- **Ghost dead row on restart.** The previous kill-and-recreate path
  fired an intermediate `SessionsRefreshed` (between the kill and the
  create) which prematurely consumed the pending restart-swap state,
  so the new internal name was appended at the bottom while the dead
  `? <name>` ghost stayed in the old slot. The dead-row
  restart-from-recents path still uses swap, but now only consumes it
  when `select_after` is actually set — intermediate refreshes pass
  through harmlessly.
- **Homebrew formula on Intel macOS** (yetidevworks/bosun#1). The
  release workflow now writes top-level `if OS.mac?` / `if OS.linux?`
  guards with an `else` fallback for the arch, instead of the nested
  `on_macos do ... on_intel do` DSL. The latter form fails on
  Homebrew 5.x setups where `Hardware::CPU.*` returns `:dunno` and
  both `intel?` and `arm?` evaluate to `false`, leaving the formula
  with no URL registered and breaking `brew info` / `brew outdated`.

### Internal
- New `TmuxClient::restart_in_place(session, command)` method:
  `send-keys C-c` (×2 with a 120ms gap) followed by `send-keys -l
  <command>` and `send-keys Enter`. Mirrors the create path's idiom
  but skips the new-session step.
- `Command::RestartSession` handler in the tmux actor now uses
  `restart_in_place` instead of `kill_session + create_session`.
- Reducer no longer captures `pending_restart_swap` for live restart
  (the internal name doesn't change). Dead-row restart-from-recents
  still captures swap and now uses `as_deref()`-based consumption so
  intermediate refreshes don't clear the state prematurely.

## [0.3.5] — 2026-05-18

### Added
- **`bosun release-notes` subcommand.** Pages the `CHANGELOG.md` that
  was embedded in the binary at compile time. Honors `$PAGER`, falls
  back to `less -RFX` and then `more`. Piping (`bosun release-notes |
  grep …`) prints directly without a pager.

## [0.3.4] — 2026-05-18

### Added
- **`bosun update` subcommand.** Checks the latest GitHub release and
  upgrades in place. Detects how the running binary was installed and
  routes accordingly:
  - **Homebrew** (`/Cellar/`, `/homebrew/`) → prints `brew upgrade bosun`.
  - **Cargo** (`~/.cargo/bin/`) → prints `cargo install --force bosun-tmux`.
  - **Standalone binary** (anywhere else, including `~/.local/bin/bosun`
    from `make install`) → downloads the matching
    `bosun-<platform>.tar.gz` from the GitHub release, extracts to a
    temp dir, and atomically swaps it into place. Safe to run while a
    TUI session is attached — the running process keeps its mmap'd
    binary; the new one takes effect on the next launch.
- **`bosun update --check`.** Reports whether a newer release exists
  without downloading or installing anything.
- **`bosun --help` / `bosun help`.** Lists the available subcommands
  and the `BOSUN_LOG` env var.

### Internal
- New `src/commands/update.rs` (port of ygrep's `commands/update.rs`,
  adapted to bosun's release-asset naming and `directories::ProjectDirs`
  data dir). Update cache lives at
  `<data_dir>/update-check.json`.
- New deps: `ureq` (with the `json` feature) for the GitHub API call
  and asset download, `serde_json` for the API response.

## [0.3.3] — 2026-05-18

### Changed
- **Scroll-wheel direction inverted.** Wheel/trackpad pan over the
  session list now matches macOS natural-scroll semantics on both
  desktop and mobile clients (Termius, Blink): swiping content
  downward moves the selection up, and vice versa.
- **Scroll sensitivity throttled.** A single trackpad gesture no
  longer flies through the list — wheel events accumulate and step
  the selection every two ticks. Counter-flicks reset the
  accumulator so reversing direction feels immediate.

### Fixed
- **Restart no longer leaves a "? &lt;name&gt;" ghost row.** Restarting
  a session (live `R` or dead-row restart-from-recents) now swaps
  the new internal name into the old row's slot — same section,
  same position — instead of appending the new session at the
  bottom while a dead ghost sits above it.

### Internal
- `SidebarModel::replace_session(old, new)` swaps an internal name
  in place across `ungrouped` and section members.
- `AppState.pending_restart_swap` captures the old internal at
  modal-confirm time; the next `SessionsRefreshed` consumes it
  before `reconcile` so the new session inherits the slot.
- Scroll handling: new `SCROLL_TICKS_PER_STEP` constant + per-state
  `scroll_accum` for the throttle; direction flip lives in the
  `MouseEventKind::Scroll{Up,Down}` arms of `handle_mouse`.

## [0.3.2] — 2026-05-15

### Added
- **Sidebar survives tmux restarts and reboots.** `SidebarModel::reconcile`
  no longer auto-drops dead sessions; entries are removed only when the
  user explicitly hits `d`. A tmux server restart (or full reboot) no
  longer wipes section structure or ordering.
- **Friendly labels on dead sidebar rows.** Missing-session rows now
  render the original display name (looked up in the Recents store via
  slug match) instead of the raw internal `bosun-slug-hex` name. Falls
  back to the slug, then the internal name, if no Recent matches.
- **`R` restarts dead sessions from Recents.** Pressing `R` on a
  missing-session row resolves the slug → Recent and fires
  `CreateSession` with the stored spec. The session lands back in its
  original section (via `session_history`). The dead row stays until
  you `d` it, so accidental Esc on the confirm doesn't lose data.
- **`d` works on dead rows too.** Confirms with "Remove from sidebar?"
  and uses the same `KillSession` path (idempotent on dead sessions).

### Internal
- `slug_from_internal(internal, prefix)` reverses `build_internal_name`,
  returning `None` on shape mismatch (foreign session names, unknown
  prefix, malformed suffix). Unit tests cover the happy path and the
  reject cases.
- `AppState` gained `session_prefix: String` and `recents: Vec<Recent>`,
  populated at startup and refreshed on every `SessionsRefreshed` so
  dead-row resolution always uses the latest store.
- `slugify` made `pub(crate)` so `dead_display_for` /
  `recent_for_internal` can match across the same canonicalization
  that `build_internal_name` originally applied.

## [0.3.1] — 2026-05-15

### Fixed
- CI: `cargo fmt` and `cargo clippy -D warnings` formatting / lint nits
  in v0.3.0 that broke the release workflow on tag push. No
  user-visible behavior change.

## [0.3.0] — 2026-05-15

### Added
- **MRU session cycle bindings.** `shift+←` and `shift+→` (prefix-less)
  jump between recently-attached sessions without going back to the
  bosun TUI. Left toggles to the previous session; right walks one
  step further back in MRU order. `__bosun_monitor` is excluded so the
  cycle never lands on the internal control-mode session.
- **Quick-jump session picker (tmux).** `prefix + o` opens a floating
  tmux popup running `choose-tree -Zs` — type-ahead fuzzy session
  chooser. Enter switches, Escape closes. `__bosun_monitor` is
  filtered out.
- **Quick-jump session picker (TUI).** Press `/` in the bosun session
  list to open a centered modal with a live filter. Matches against
  session display name, agent, and path. Up/Down to highlight, Enter
  attaches, Esc cancels.

### Changed
- **Status bar overhaul.** Bar now reads
  `⚓ bosun · <current-session-name>` on the left and
  `^Q detach · S-←→ cycle · C-a 1-9 jump` on the right. The current
  session name is pulled from each session's `@bosun_display` user
  option (fallback to `#S`). Replaced the per-session chip strip
  (`1:foo 2:bar …`) which became unhelpful past ~5 sessions. The
  prefix+1..9 jump bindings still install and work — just no longer
  rendered in the bar.

### Internal
- `attach.rs` lifecycle now owns three runtime binding sets: the
  existing `C-q detach`, the new `S-Left`/`S-Right` cycle, and the new
  `prefix + o` quick-jump. All three self-heal on every refresh tick
  via `do_refresh` and are torn down on `GlobalsGuard::drop` plus the
  panic-hook `emergency_unbind`.
- Format-expansion escaping: tmux re-expands `run-shell` and
  `display-popup` arguments at trigger time, so the bindings that
  contain `#{…}` formats now double them as `##{…}` to survive the
  expansion intact. Documented inline next to each binding.
- `Command::Attach` from a closing modal is now intercepted by the app
  loop and re-routed to `pending_attach` (the actor ignores
  `Command::Attach` — only the app loop can do the tty handover).

## [0.2.14] — 2026-05-14

Last release before the navigation overhaul. See git history for
prior changes.
