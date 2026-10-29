# Bosun for Mac: design plan

A native macOS front end for Bosun. It's a Swift/AppKit app with a session sidebar and a main area that holds either one terminal or a tiled grid of them. Each terminal is a libghostty view attached to a Bosun tmux session. tmux stays the backend, so sessions outlive the app, Moshi and the TUI still reach the same agents, and all of Bosun's session logic is reused instead of ported.

"Fast" here means runtime speed: typing, switching, resizing and idle cost. Every decision below is made to hit the budgets in section 1, and section 11 covers how to measure them.

## 1. Speed budgets

| Path | Budget | How it's met |
|---|---|---|
| Keystroke → glyph | Same as Ghostty itself | No app or daemon code in the byte path (section 2) |
| Switch to a warm session | ≤ 1 frame (8–16 ms) | Warm pool: swap an already-attached view in (section 5) |
| Switch to a cold session | < 80 ms | One `tmux attach` spawn + shared font atlas; prewarm on selection |
| Agent state change → sidebar badge | < 250 ms | Daemon pushes deltas; no polling from Swift |
| Cold launch → sidebar painted | < 300 ms | Sidebar drawn from cached snapshot before the daemon answers |
| Cold launch → first live terminal | < 500 ms | Visible tiles attach first, pool warms afterwards |
| Divider drag | Display rate (120 Hz on ProMotion) | Manual frame layout, no Auto Layout; pty resize throttled |
| Idle CPU (10 sessions, 4 tiles, agents idle) | < 0.5 % | Damage-based rendering, occluded views paused, CALayer spinners |
| Memory per terminal view | < 15 MB | Ghostty scrollback near zero; tmux owns history |

## 2. Architecture

```
┌──────────────────────── Bosun.app (Swift) ────────────────────────┐
│  Sidebar (SwiftUI)        Main area (AppKit, manual layout)       │
│  ▲ state deltas           ┌─────────┬─────────┐                   │
│  │                        │ Ghostty │ Ghostty │  each view runs   │
│  │ commands ▼             │  view   │  view   │  tmux attach in   │
│  │                        └────┬────┴────┬────┘  its own pty      │
└──┼──────────────────────────────┼─────────┼────────────────────────┘
   │ Unix socket (JSON frames)    │ pty     │ pty
┌──┴──────────────┐          ┌────┴─────────┴─────────────────────┐
│ bosun serve     │ tmux -C  │ tmux server  (-L bosun)            │
│ (Rust daemon,   ├─────────►│ sessions, agents, history          │
│  tmux actor)    │◄─────────┤ __bosun_monitor control session    │
└─────────────────┘          └────────────────────────────────────┘
```

There are three paths, and they never share a thread or a process:

1. **Bytes** (keys in, output out): Ghostty view ⇄ pty ⇄ `tmux attach` client ⇄ tmux server. Neither the Swift app logic nor the daemon touches these bytes, so typing latency is exactly Ghostty's plus tmux's, and nothing we write can slow it down.
2. **State** (session list, agent status, sections, recents): tmux control mode → daemon → pushed deltas over the socket → per-row Swift models.
3. **Commands** (create, restart, kill, rename, launch agent): Swift → socket → daemon → written to the control-mode channel. No fork/exec per command.

## 3. Daemon: `bosun serve`

The seam already exists. `actors/tmux_actor.rs` takes `Command` in and emits `AppMsg` out, owns the `ControlClient` on `__bosun_monitor`, and runs the detector registry. `bosun serve` hosts that actor with no ratatui and maps a subset of `Command`/`AppMsg` onto a socket protocol.

**Transport:** a Unix domain socket in Bosun's data dir (next to the SQLite store), using length-prefixed JSON frames. Messages are small and infrequent, so JSON with Swift `Codable` costs microseconds, and you can debug it with `socat`. Only switch to msgpack if profiling ever shows serialization on a trace, which is unlikely.

**Push, never poll:** on connect the client sends `hello {protocol: 1}`, and the daemon replies with one `snapshot` (sidebar model, sessions, statuses, recents, config view). After that it sends `delta` events, each with a monotonically increasing `seq`. A client that sees a gap asks for a fresh snapshot. Swift has no timers for state.

```
→ {"t":"hello","protocol":1,"client":"bosun-mac/0.1"}
← {"t":"snapshot","seq":1,"sidebar":{…},"sessions":[…],"status":{…}}
← {"t":"status","seq":2,"session":"bosun-api","state":"working"}
← {"t":"sessions","seq":3,"added":[…],"removed":["bosun-old"]}
→ {"t":"cmd","id":7,"op":"restart","session":"bosun-api","resume":true}
← {"t":"ack","id":7,"ok":true}
```

**Commands over the control channel:** today `TmuxClient` execs a `tmux` process per command. Route commands through the already-open `tmux -C` stdin instead, matching replies by the `%begin/%end` command number. This is the "control mode for commands" piece noted earlier as worth doing anyway. It also speeds up the TUI, so it lands in Bosun proper before any Swift work.

**Agent state:** fetch it with one `list-panes -a -F …` written to the control channel every 250 ms: a single pipe write and a parse, with no process spawn. The spike showed that tmux format subscriptions only fire about once a second and can't watch other sessions directly, so they don't replace the poll.

**Agent state from hooks:** the TUI's status has always been a guess from pane titles and screen contents, and it's often wrong. Agents that support hooks report their own state instead:

- A new `bosun hook <agent> <event>` subcommand is what the agent's hooks run. It reads the hook's JSON from stdin, maps it to a `Status` (Running, Waiting, Done, Idle), and writes it to the pane as tmux user options (`@bosun_state`, `@bosun_state_at`, and an optional `@bosun_state_detail` such as "permission: Bash"). It finds the pane from `$TMUX_PANE` and the server from `$TMUX`, so it works on any socket and needs no daemon. It must finish in a few milliseconds, print nothing (Claude adds some hooks' stdout to the conversation), and always exit 0 so a hook can never break the agent.
- The 250 ms poll reads those options in the same `list-panes` call, so the TUI, the daemon and the Mac app all see the same state.
- Bosun installs the hooks only through the launch command (for Claude, `--settings` with a Bosun-generated hooks file), never by editing the user's global agent config. Sessions pick them up on their next launch or restart.
- Claude Code mapping: prompt submitted or tool running → Running; permission or question prompt → Waiting; turn finished → Done; session start → Idle. The other agents get hooks where they offer a clean way to inject them from the launch command, and keep the existing detectors otherwise.
- Hook state wins over the detectors while it's fresh. The detectors stay as a cross-check for cases hooks miss. For example, Claude fires no Stop hook when you interrupt it with Esc, but its pane title stops showing the spinner, so a "Running" hook state with a non-working title falls back to the detector.

**Lifecycle:** the app starts `bosun serve` if the socket doesn't answer, and reuses it if it does. A lock file ensures a single instance. The daemon exits 30 s after its last client disconnects. Nothing is lost because sessions live in tmux and Bosun's state lives in SQLite. A protocol major-version mismatch is a hard, readable error ("update bosun"). The TUI keeps its in-process actor for now; making it a daemon client too is a later, optional step that would halve tmux polling when both are open.

## 4. Terminal views (libghostty)

- **Build:** a Ghostty xcframework from a **pinned** Ghostty commit (the same route as Ghostty's own Mac app and cmux). Wrap it in one Swift class, `TerminalView`, so the rest of the app never calls libghostty directly. If the embedding API changes or breaks, only that file changes, and SwiftTerm stays a viable fallback behind the same class.
- **One app instance, many views:** a single libghostty app object shares the font atlas and config across views, so a new view costs a Metal layer and a pty, not a font load.
- **Command:** each view runs `tmux -L bosun attach -t =<session>` (`=` for an exact match). Bundle Ghostty's terminfo in the app's Resources and point `TERMINFO` at it, so the tmux client gets `xterm-ghostty` features. Inside sessions, tmux keeps its own `TERM`.
- **Scrollback:** set Ghostty's scrollback limit to about zero. tmux owns history, and mouse-wheel scrolling enters tmux copy mode just as it does in Bosun today. This is most of the per-view memory win, since Ghostty's default scrollback is about 10 MB per view.
- **Config:** read the user's Ghostty config for font, theme and padding (as cmux does), then apply app overrides on top. Strip Ghostty's keybinds that clash with app shortcuts (split, new tab, close).
- **Occlusion:** a view that's hidden (in the warm pool), in a minimized window, or fully covered is marked occluded (`ghostty_surface_set_occlusion`, verify the name against the pinned header) and its renderer stops. Its pty keeps draining, so tmux never blocks on a slow client and the view is already current when it's shown again.
- **Focus and the cursor blink:** libghostty's focus is the active pane of a key window, nothing else. A focused terminal blinks its cursor, and each blink is a whole frame, so a pane left focused costs about 33 frames in 20 s while idle. libghostty's renderer can lose a focus change's effect on the blink timer (a focus-out in the same batch as a blink reset from pty output, or a focus-in while a cancel is in flight), so the view watches its own frames: an unfocused pane drawing at exactly the blink interval, or a focused one not drawing at all, is given the focus change again.
- **Links:** Cmd+click opens URLs and paths. tmux has the mouse, and libghostty only looks for links under a captured mouse with Shift held, so the app adds Shift to Cmd for the pointer and left button. Relative paths are resolved against the pane's directory from tmux (`#{pane_current_path}`), since no OSC 7 comes through tmux.

## 5. Switching: the warm pool

This is the biggest speed lever.

- **Pool:** an LRU of attached `TerminalView`s keyed by session, 8 by default (configurable). Switching to a pooled session is a view swap inside the tile: no spawn, no attach, no redraw wait, because the hidden view kept consuming output while occluded. It takes one frame.
- **Prewarm:** keyboard navigation in the sidebar starts attaching the selected session after a 50 ms dwell (the same idea as Bosun's settle debounce), so by the time you press Return it's usually warm. Prewarming runs at utility QoS and never preempts a visible attach.
- **Cold path:** a session that isn't pooled spawns one new view and one `tmux attach`. tmux paints the full screen on attach. The target is under 80 ms, and the main costs are the client process spawn and Metal layer setup.
- **Eviction:** the least recently used hidden view is detached (the clean SIGHUP path, off the main thread — the lesson from 2.1.3/2.1.4). Visible views are never evicted.
- **One tile per session:** a session appears in at most one tile at a time. This removes the "two GUI clients of different sizes" case entirely.
- **Sizing of hidden clients:** pooled-but-hidden clients must not drag a session's size around. The plan is to toggle tmux's `ignore-size` flag per client from the daemon (`refresh-client -t <client> -f ignore-size` on hide, clear it on show) and keep `window-size latest`. To do that the daemon has to match a view to its tmux client, by `#{client_pid}` against the view's child pid, or with a tiny exec wrapper that reports `$$` first. This is spike item #1 because everything about switching depends on it.

Rejected alternative: one client per tile with `switch-client` on every switch. It's simpler, but every switch costs a full tmux redraw, and it can't keep background tiles live.

## 6. Main area layout: each sidebar row is a workspace

A sidebar row (a Bosun container) owns its own split layout, and **the row's tabs are its panes**. In the TUI, tabs hide each other, so related work in one project ends up behind a tab strip you rarely look at. In the app, the tabs of a row sit side by side, so one row can hold an agent, a test watcher and a shell for the same project, all visible at once.

- **Selecting a row replaces the whole main area** with that row's layout: one pane or several, exactly as you left it. Clicking a row, previous/next session, Cmd+1…9 and Cmd+K all switch whole workspaces. Nothing from the previous row stays on screen.
- **Switching stays fast:** the warm pool (section 5) keeps the views of recently shown rows attached and occluded, so switching to a warm row with three panes is three view swaps in one frame. Prewarm on hover warms every pane of the hovered row, visible panes first.
- **New pane = new tab:** Cmd+D (right) and Cmd+Shift+D (down) create a new tab in the current row as a plain terminal in the row's directory, instantly and with no sheet, and split the active pane for it. "New Tab…" (Cmd+T) opens the new-session sheet pre-filled with the row's directory and adds the result as a split the same way, for when the new pane should run an agent. Both send `create` with the row's `container_id`, so the TUI sees an ordinary new tab.
- **Closing a pane ends its tab:** Cmd+W closes the active pane and kills that tab's session. If anything other than a shell at its prompt is running, it asks first, the way Terminal.app does. Closing the last pane of a row is a kill of the row and always asks. Killing a whole row stays a menu item with a confirmation.
- **Tabs from elsewhere:** a tab created in the TUI, or a row that already has several tabs when the app first sees it, gets a pane automatically: split beside the last pane, alternating direction, so a row with four tabs and no saved layout opens as a 2×2 grid. A tab that ends (exited, or killed in the TUI) loses its pane and the space goes to its neighbour.
- **Tab navigation becomes pane navigation:** previous/next tab (Shift+Left/Right by default, from `[keybindings]`) moves the active pane through the row's panes in tab order, and Cmd+Option+arrows moves spatially. Tab rows under a container in the sidebar stay, and clicking one activates that pane.
- **Model:** each row has a split tree, `Node = leaf(session) | split(axis, ratio, [Node])`. It's pure data, serializable and unit-tested without views. Moving a pane (dragging its header onto another pane's edge, or onto its middle to swap) is a tree operation inside the row.
- **Persistence:** the trees are GUI-only, so they're stored in the app's Application Support JSON keyed by container id, not in the daemon or Bosun's SQLite. Each row also remembers its active pane.
- **View:** a custom `NSView` container computes child frames directly from the tree (O(n) arithmetic per layout) and sets them. No Auto Layout and no nested `NSSplitView` in the terminal area: constraint solving is the usual cause of choppy divider drags. Dividers are thin hit-test views with a hover cursor.
- **Zoom** (Cmd+Return, or a double click on a pane's header) shows one pane of the row full size without changing its tree, like tmux's `resize-pane -Z`. The other panes' views stay attached and hidden (occluded, `ignore-size`), so unzooming is a view swap. The zoom follows the active pane, so pane navigation flips through the row full size (tmux's `select-pane -Z`); a pane added or closed ends it. It's saved with the row's layout.
- **Live resize:** pixel frames follow the pointer at display rate. Grid-size changes (pty winsize → SIGWINCH → agent redraw) are throttled to one every 100 ms during the drag, plus a final one on mouse-up, because ink-based agents like Claude Code redraw their whole screen on every SIGWINCH. The same throttle covers dividers, the window's live resize and the sidebar's edge. libghostty's layer keeps its contents top-left, so a pane between two grid sizes shows its text unstretched. Measured with four ink-style agents redrawing at 30 fps: the main thread keeps up at any throttle (every step handled, layout under 2 ms at p99), so the throttle is for the agents: 100 ms cuts a 2 s drag from about 117 SIGWINCHes per pane to about 23, and 150 ms only to 16.
- **Windows and tear-off:** every window is a full Bosun window (sidebar and main area) that can show any row, but a row is on screen in one window at most, so a session still appears in one tile at a time. Selecting a row another window shows brings that window forward, each sidebar marks those rows, and previous/next row skips them. One warm pool and one store of row layouts serve every window: a view lives in the area of the window that last showed it and is reparented, never reattached, when another window shows its session. Dragging a row out of the sidebar opens it in a new window (the old window moves on to a free row); dragging a pane out of the window by its header moves its tab out into a row of its own (`move_tab` with no container, so the TUI sees it) and opens that row in a new window. Closing a window keeps its views warm in another; the other windows and their rows are saved with the row layouts.
- **Practical ceiling:** 9 panes per row is the supported maximum. Past that, the budgets aren't promised.

## 7. Sidebar

- **Data:** mirrors Bosun's sidebar model: sections, containers with tabs, sessions, agent badges and recents, all from the daemon snapshot and deltas.
- **Implementation:** SwiftUI `List` with one `@Observable` model per row (macOS 15+ Observation), so a status delta re-renders only its own row. If a profile ever shows a row update over 2 ms, drop to `NSOutlineView` with diffable updates. With Bosun-scale lists (tens of rows, under 200) SwiftUI should be fine.
- **Badges:** the "working" spinner is a `CALayer` animation, which runs in the render server at about zero app CPU. No SwiftUI repeating animations: they invalidate the view tree every frame, which is the same class of bug as issue #16.
- **The sidebar never takes the keyboard** (section 7b). It's driven by the mouse and by app shortcuts that work from anywhere: Cmd+1…9 to jump, Cmd+K for a fuzzy quick-jump palette, the TUI's previous/next session and tab keys (section 8), and Cmd+Shift+N for a new session (the same modal fields as the TUI, with the spec sent to the daemon). Each of these changes what the active tile shows and leaves the keyboard in that terminal.
- **Prewarm on hover and on keyboard navigation:** hovering a row for 50 ms starts attaching its session, and so does stepping onto it with the previous/next keys, so a click or a pause usually lands on a warm view.
- **Header:** the anchor tile (the theme's `accent`, the glyph in ink by the TUI's `Theme::on` luminance rule, drawn as a path so it follows the theme), "Bosun" and the filter's magnifier share the row with the traffic lights; a one-line status under them counts sessions, waiting and done, or says the daemon is out of reach with a Reconnect link. A 135° gradient from `accent` (32% on dark themes, 22% on light) through `status_done` to clear fades out before the first row. The gradient and tile are static layers changed only by the theme, and the counts by the deltas that already refresh the rows, so the header costs nothing while idle. Dragging its empty area moves the window.
- **Rows:** the selected row gets the same 3 pt accent bar as the active pane's tab, since light themes' `selection_bg` is close to their `panel`; the row under the pointer gets a light `panel_alt` fill.
- **Filter (Option+Cmd+F):** a field under the header that borrows the keyboard as rename does (section 7b). Each key narrows the list (names by subsequence with the matched letters underlined; section, agent and the directory's last two parts by the whole query; two letters or more of a state word by state). Return keeps the filter as a chip and hands the keyboard back, Esc clears it, a click elsewhere keeps it. The row on screen is never hidden: when it doesn't match it's listed last under "On screen". Cmd+1…9 and previous/next row follow the listed rows, so "rev", Return, Cmd+2 is the second review. Per window and never saved. The logic is `SidebarFilter` in BosunCore, unit tested.
- **Find in a pane (Cmd+F):** a bar in the active pane's top right corner, "Find in <session>", so it can't be taken for the filter. Ghostty keeps no scrollback here (the history is tmux's), so it searches through the daemon's `search` command, which drives tmux's copy-mode search: tmux scrolls to each match, highlights the rest and counts them. Typing searches again from the bottom, as a regular expression the daemon escapes, so case can be ignored (the default) or matched (the Aa toggle) whatever tmux's own smart-case rule would do; matches take the theme's accent. Return / Cmd+G go older, Shift+Return / Cmd+Shift+G newer. Esc leaves copy mode; a click in the pane keeps it there, at the match. It borrows the keyboard like the filter, and anything else that takes the keyboard (the palette, a sheet, a rename, the filter), another row or pane, a drop on the pane, or the session ending closes it.
- **Standard Mac sidebar behaviour:** drag and drop to reorder sessions and move them between sections (sections reorder the same way), double-click a row's name to rename it in place, a right-click menu for each row (restart, resume, modify, rename, kill), and collapsible sections. There's no type-to-select, because typed keys always belong to the terminal. Every change goes to the daemon, which writes the same sidebar model in `config.toml` that the TUI uses.
- **Implementation note:** a SwiftUI `List` takes keyboard focus when it's clicked, so the sidebar is an `NSOutlineView` with `refusesFirstResponder = true` (clicks still select, drag and drop and in-place editing are built in), with SwiftUI only for row content if it helps. Each row still updates on its own from its model.

## 7b. Focus: the terminal always has the keyboard

The TUI's biggest daily annoyance is focus. The sidebar and the terminal take turns being "active", and it's easy to lose track of which one is. You type and nothing reaches the agent, or a stray key starts a rename or opens settings, or you drop an image and it silently does nothing because the sidebar had focus. The Mac app removes the sidebar as a focus state entirely.

- **One keyboard owner:** while the window is key, the active terminal tile is always the first responder. Nothing in the sidebar, toolbar or tile chrome can take it. Clicking a sidebar row, dragging a row, collapsing a section or using its menu leaves the keyboard in the active terminal.
- **Temporary, visible exceptions only:** in-place rename, the Cmd+K palette, the sidebar filter's field (while its ring shows; an applied filter's chip holds nothing), the pane's find bar and the new session / modify sheet take the keyboard while they're open, because they're obviously text fields. Return or Esc (or clicking anywhere else) closes them, and the keyboard goes straight back to the active terminal. Nothing else ever takes focus.
- **Active tile ring:** with two or more tiles, the active tile gets a 2 px ring in the theme's `accent` colour, drawn inside the tile edge so the layout never shifts. Inactive tiles get a 1 px `panel_alt` border and are slightly dimmed (Ghostty's unfocused-split dimming, driven by the app). The ring dims when the window isn't key, so you can tell at a glance that keys aren't going anywhere. With one tile there's nothing to tell apart, so the ring is hidden.
- **Switching tiles:** a single click on a tile activates it. That first click is consumed rather than passed to tmux, so activating a tile never clicks something in the agent's UI by accident. Cmd+Option+arrows move between tiles from the keyboard.
- **Focus follows the mouse (on by default):** with two or more tiles, moving the pointer into a tile makes it active. To avoid surprises it only reacts to real pointer movement (not a tile moving under a still pointer after a layout change), never while a mouse button is held, never while rename, the palette or a sheet is open, and never when the window isn't key. It's a setting for people who prefer click-only.
- **Drops go where the pointer is, never nowhere:** dropping files or images on a tile activates that tile and inserts the paths into it, whichever tile was active before. Dropping files on a sidebar session row shows that session in the active tile and inserts them there. A drop anywhere else (sidebar background, section headers) is refused with the standard no-drop cursor, so it's clear before you let go that nothing will happen.
- **Selecting a row** shows that row's own panes (section 6), and the keyboard goes to the pane that was active when you last left the row.
- **Status stays separate from focus:** a tile whose agent is waiting shows it in the tile's header strip (session name plus the status colour), never as a border, so "needs you" can't be confused with "active".

## 7a. Themes

- **App chrome follows the Bosun theme.** The daemon sends the active theme's palette (the same TOML fields as `ui/theme.rs`: `bg`, `panel`, `panel_alt`, `selection_bg`, `text`, `text_muted`, `accent`, the status colours) plus the list of available themes, bundled and user ones. The sidebar, dividers, tile borders and badges are drawn from it. Changing the theme in either the TUI or the app goes through the same `SetTheme` path and `config.toml`, so both switch together.
- **Terminal colours follow the Bosun theme by default.** A theme file's optional `[terminal]` table (an optional `ghostty` name, `background`, `foreground`, `cursor`, `selection_bg`, `selection_fg` and a 16-colour `palette`) travels with the theme as `terminal`, so the daemon is the single source and a user theme can carry one too. Every bundled theme has one: the Ghostty counterpart's colours where there is one, a generated palette otherwise. The app takes the palette from, in order, the theme's `terminal` table, the Swift name map of Ghostty counterparts (for a daemon that predates the table), and a palette generated from the theme's colours, where each hue is lifted toward the text colour until it reads at 3:1 on the terminal background. The colours are applied as an app-level Ghostty config override after the user's config, colour keys only (`background`, `foreground`, `cursor-color`, `selection-background`, `selection-foreground`, `palette`), so font, padding and the rest of the user's config still apply and their Ghostty config file is never touched. The chrome and the terminals change together, including with the Light/Dark pair.
- **Setting.** Settings > Theme has a two-way picker, **Follow the Bosun theme** or **Use my Ghostty config**, stored as `matchTerminalTheme`. It defaults to on, and the first launch with it switches it on for everyone once, whatever was stored before (it used to default to off, so a stored off mostly meant nobody had changed it). An existing install whose terminals change because of that sees a one-time title-band note ("Terminals now follow the Bosun theme (Settings → Theme to change)"). After that first launch, choosing Use my Ghostty config sticks.
- **Preview cost.** The TUI's theme picker and the View → Theme hover send a theme per keystroke, and every `ghostty_app_update_config` rebuilds the config and redraws every terminal, so terminal colour changes reach Ghostty at most once per 80 ms: the first goes through at once and a burst after it becomes one more update with the latest colours. The chrome is never delayed.
- **Light and dark:** an optional pair of themes (`theme_light` / `theme_dark`) that switch with the macOS appearance. When it isn't set, the single `theme` is used, as in the TUI.
- Window background and titlebar use the theme's `bg`, so a light theme doesn't leave a dark frame around it.

## 8. Keys and input

- The focused terminal view gets every key except app key equivalents (Cmd+…). There's no prefix key and no send-next-key, because Cmd belongs to the app and everything else goes to the agent.
- Option-as-Meta comes from the Ghostty config (`macos-option-as-alt`). IME and dead keys are handled by Ghostty's own text-input path, which the spike must verify in the embed.
- **Close vs kill:** a pane is a tab, so Cmd+W closes the active pane and ends its tab (section 6). It asks first unless the pane is a shell at its prompt, and closing a row's last pane always asks. Killing a whole row is a menu item with a confirmation dialog, never a single keystroke.
- Default shortcuts: Cmd+D new terminal tab split right, Cmd+Shift+D split down, Cmd+T new tab with the sheet, Cmd+Option+arrows move focus between tiles, Cmd+Return zoom.
- **Navigation matches the TUI:** previous/next session and previous/next tab work like the TUI's `[keybindings]` defaults, using the same config so a remap applies to both. Where a TUI key would clash with what the agent needs, the app uses the Cmd version of it.
- **Drag and drop into terminals:** dropping files or images on a tile inserts their paths, shell-escaped, as typed text, so Claude Code attaches dropped images the same way it does in Ghostty. Image data that isn't a file (from a browser, or a screenshot thumbnail) is written to a temp file first and its path inserted. This ports Ghostty's own drop handling into `TerminalView`; the spike didn't have it.

## 9. Startup sequence

1. Draw the window and sidebar from a cached snapshot JSON the app wrote on its last quit. That's the first frame, with no I/O beyond one small file read.
2. In parallel, connect to the daemon, or spawn it and connect. When the live snapshot arrives, diff it onto the cached rows.
3. Restore the last layout and attach **visible** tiles first.
4. At low priority, warm the pool with recently used sessions.

Link libghostty statically, keep dependencies near zero, and target Apple Silicon only at first (add a universal build if anyone asks).

## 10. Notifications

State is already pushed, so these are nearly free:

- **When:** an agent starts waiting on you (permission or question, with the detail) or finishes a turn (a new hook `done_at`), and you can't see it: Bosun isn't the active app, the window is hidden, or the session isn't in the row on screen. Once per wait and once per finished turn, nothing for what was already waiting or done when the app connected, and a session's notifications are taken back once you see it, type into it, or its state moves on.
- **Click:** brings the app forward on that row with the keyboard in that pane.
- **Dock badge:** the number of sessions waiting on you. Done isn't counted, because a done session stays done until you type into it.
- **Settings:** Notify When Waiting, Notify When Done and Show Dock Badge, all on by default.
- A pane whose agent is waiting shows it in its header strip (section 7b; borders are reserved for the active ring). Cmd+Shift+U jumps to the oldest waiting session.

## 11. Measuring

The budgets are only real if they're measured.

- **App:** `os_signpost` intervals around switch (warm and cold), attach → first frame, divider drag, and daemon round trip, plus a saved Instruments template that shows them together.
- **Daemon:** `tracing` spans on command latency and the status poll, and `bosun serve --stats` to dump p50/p99.
- **Harness:** reuse the tmux-it fixture to stand up 10 sessions on a throwaway socket. `make perf-mac` scripts 200 switches (warm and cold) and reports p50/p99 from the signposts. An idle check samples CPU for 60 s with 4 tiles open.
- **Typing latency:** compare against Ghostty standalone with Typometer. The goal is "no measurable difference", and section 2 is what makes that achievable.

## 12. Open questions (spike items)

1. **Hidden-client sizing:** does toggling `ignore-size` per client from the daemon keep sessions correctly sized across GUI, TUI and Moshi? How do we reliably map a view to its tmux client?
2. **Subscriptions:** can `refresh-client -B` from the monitor session watch panes in other sessions? If not, keep the 250 ms control-channel poll.
3. **libghostty embedding:** at the pinned commit, check view creation, a custom command, occlusion, set-size, the text input path (IME) and reading the user's config.
4. **Live resize reflow:** does throttled SIGWINCH look acceptable with Claude Code mid-render?
5. **Control-client sizing:** confirm the `__bosun_monitor` control client is still ignored by window-size negotiation once it's also carrying commands.

## 13. Phases

| Phase | Scope | Exit criteria |
|---|---|---|
| 0. Spike (≈2 days) | Xcode project, libghostty xcframework, hardcoded session list from `tmux list-sessions`, one view attaching, a 3-view pool swap | Open questions 1, 3 and 4 answered; warm swap ≤ 1 frame measured; typing feels like Ghostty |
| 1. Daemon | Commands over the control channel (benefits the TUI too), `bosun serve`, protocol v1, snapshot/delta, hook-based agent state, TUI and app running side by side (sidebar reload, shared global bindings), tests | TUI unchanged and faster; `socat` session shows snapshot + deltas; Claude badges driven by hooks; CI green |
| 2. MVP app | Sidebar from daemon with sections, drag and drop reordering and inline rename, the focus model (section 7b), per-row workspaces where a row's tabs are its split panes (section 6: Cmd+D / Cmd+Shift+D new tab as a split, Cmd+W closes a pane and its tab, draggable dividers, dragging panes to rearrange), warm pool + prewarm, create/restart/kill/rename, TUI-style navigation keys, file and image drops into terminals, Bosun themes for the app chrome and optionally the terminals, Cmd+K, 250 ms status poll in the daemon | Daily-drivable; typing never lands anywhere but a terminal; all budgets in section 1 except layout ones met |
| 3. Layout | Zoom, tuned resize throttling, tear-off and several windows | Divider drag at display rate with 4 busy agents (met: every step handled at 60 Hz, layout under 2 ms at p99) |
| 4. Polish + ship | Notifications, settings (via daemon, env-pinned rows read-only as in the TUI), signing + notarization, Homebrew cask in the existing tap | Notarized build installs via `brew install --cask` |

**Repo:** a `macos/` directory inside the bosun repo, so the protocol and both ends change in the same commit. Add `macos/` to `exclude` in `Cargo.toml` so it stays out of the crates.io package.
