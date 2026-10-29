# `bosun serve` protocol, version 1

`bosun serve` runs Bosun's tmux actor without the TUI and lets other programs (the planned Mac app first) drive it over a Unix socket. A client connects, says `hello`, receives one `snapshot` of everything it needs to draw Bosun's sidebar, then a stream of deltas. Commands go the other way and are acknowledged by id. The client never polls.

The Rust types for every message live in `src/serve/protocol.rs`. This document and that module change together.

## Running the daemon

```
bosun serve [--idle-timeout <secs>]
bosun serve --print-socket
```

- **Socket:** `bosun.sock` in Bosun's data directory, next to `bosun.db`. On macOS that is `~/Library/Application Support/dev.yetidevworks.bosun/bosun.sock`; on Linux `$XDG_DATA_HOME/bosun/bosun.sock` (usually `~/.local/share/bosun/`). `bosun serve --print-socket` prints the exact path. The socket is created with mode `0600`, and connections from another user id are refused.
- **Single instance:** the daemon holds an exclusive `flock` on `bosun-serve.lock` in the same directory (the file holds its pid). A second `bosun serve` exits with status 1 and `bosun serve is already running (pid N)` on stderr. A client that finds no socket, or a socket that refuses connections, starts `bosun serve` itself and then connects; if two clients race, one daemon wins and both connect to it.
- **Lifetime:** the daemon exits 30 seconds after its last client disconnects (or after starting, if nobody connects). `--idle-timeout` changes that. SIGINT and SIGTERM stop it at once. It removes its socket on the way out. Nothing is lost when it exits: sessions live in tmux and Bosun's state lives in `config.toml` and SQLite.
- **tmux socket and settings:** the same as the TUI, from `config.toml` and the `BOSUN_*` environment variables (`BOSUN_TMUX_SOCKET` and so on).
- **Logs:** `BOSUN_LOG=info` (or any tracing filter) writes logs to stderr. Off by default.

## Framing

Every message, in both directions, is one frame: a 4-byte big-endian unsigned length, followed by that many bytes of UTF-8 JSON. Frames larger than 16 MiB are rejected. Each message is a JSON object whose `"t"` field names its kind. Unknown fields must be ignored by both sides, so later versions can add fields without a version bump.

```
00 00 00 2e  {"t":"hello","protocol":1,"client":"bosun-mac/0.1"}
```

## Session

```
→ {"t":"hello","protocol":1,"client":"bosun-mac/0.1"}
← {"t":"snapshot","seq":1,"server":{…},"sessions":[…],"statuses":{…},"sidebar":{…},"recents":[…],"config":{…},"theme":{…},"theme_light":{…},"theme_dark":{…},"themes":[…],"keybindings":{…}}
← {"t":"status","seq":2,"session":"bosun-api-1a2b3c4d","state":"running","source":"hook"}
→ {"t":"cmd","id":7,"op":"restart","session":"bosun-api-1a2b3c4d","resume":true}
← {"t":"notice","seq":3,"message":"restarted api"}
← {"t":"ack","id":7,"ok":true,"messages":["restarted api"]}
```

1. The client sends `hello` first. Anything else before it gets an `error` with code `no_hello` and the connection is closed.
2. The daemon answers with a `snapshot`.
3. From then on the daemon pushes deltas whenever something changes, and the client sends commands whenever it likes.

### Sequence numbers

Every message from the daemon except `ack` and `error` carries a `seq`. The `snapshot` carries the current value; each later message carries exactly one more than the one before it on that connection. A client that sees a gap (it shouldn't happen on a stream socket, but a client that drops messages under load could cause one) sends `resync` and gets a fresh `snapshot`. Apply deltas in order onto the last snapshot and the result always equals what a new snapshot would contain.

### Versioning

`protocol` is the major version, currently `1`. The daemon refuses any other value with an `error` whose `code` is `protocol_mismatch` and whose `message` is meant for a person, then closes the connection:

```
← {"t":"error","code":"protocol_mismatch","message":"bosun serve 2.2.0 speaks protocol 1 but this client speaks 2; update bosun or the app so both match"}
```

New message kinds, new commands and new optional fields do not change the major version. Clients ignore message kinds they don't know.

## Client → daemon

### `hello`

```json
{"t":"hello","protocol":1,"client":"bosun-mac/0.1"}
```

`client` is optional and free-form; it's only for logs. Sending `hello` again on a live connection just returns a new snapshot.

### `resync`

```json
{"t":"resync"}
```

Ask for a fresh `snapshot`.

### `cmd`

```json
{"t":"cmd","id":12,"op":"kill","session":"bosun-api-1a2b3c4d"}
```

`id` is chosen by the client (any unsigned 64-bit number; use a counter) and comes back in the `ack`. `op` names the command; its arguments sit next to it in the same object. Commands are carried out in the order they arrive. Commands that change tmux state are run by the same actor the TUI uses, one at a time, so an `ack` can take a moment (a `restart` waits for the old agent to stop). The deltas a command causes are sent before its `ack`.

Text fields that end up in tmux (display names, section names, paths, args) may not contain control characters; such a command is refused with `ok: false` before anything runs. Session names are the internal tmux names from `sessions[].name`.

#### `create`

```json
{"t":"cmd","id":1,"op":"create","defer_launch":true,
 "spec":{"name":"api","path":"~/work/api","agent":"claude","args":"--model opus",
         "options":{"claude":{"session_mode":"continue","skip_permissions":false,"remote_control":false}},
         "container_id":null,"resume":false,"worktree_branch":null}}
```

Creates a session from a spec (see [Spec](#spec)). The display name is made unique the same way the TUI does it ("api" becomes "api 2"). The `ack` carries the new internal name in `session`.

An optional `section` (a section id) puts the new row at the end of that section instead of among the ungrouped rows. An unknown section is refused before anything is created. It's ignored when `spec.container_id` adds the session to an existing row as a tab.

An optional `replaces` (a session name) restarts an exited tab: the new session takes that tab's place in its row, so the row keeps its id and its spot in its section instead of a new row appearing. `spec.container_id` is set to that row when the spec leaves it empty, and `section` is then ignored. It's ignored when it names a live session or a tab that isn't in the sidebar. Bosun for Mac sends it for Restart and Resume on an exited row.

The agent command is typed into the new shell straight away, unless `defer_launch` is `true`. Then the session starts as a bare shell, the `ack` carries `pending_launch`, and the client sends `launch_agent` with those values once its terminal view is attached. Agents like Codex probe the terminal's background color at startup; deferring the launch until a terminal is there to answer is what lets them pick the right palette (Bosun issue #2). A GUI that shows the session right away should defer.

```json
{"t":"ack","id":1,"ok":true,"session":"bosun-api-1a2b3c4d","pending_launch":{"session":"bosun-api-1a2b3c4d","resume":null},"messages":["created bosun-api-1a2b3c4d"]}
```

#### `kill`

```json
{"t":"cmd","id":2,"op":"kill","session":"bosun-api-1a2b3c4d"}
```

Kills the tmux session. Killing a session that's already gone succeeds. Whether the sidebar row stays (as an exited row that can be restarted) or goes follows the `remove_dead_sessions` setting, as in the TUI.

#### `kill_container`

```json
{"t":"cmd","id":3,"op":"kill_container","sessions":["bosun-api-1a2b3c4d","bosun-api-2-9f8e7d6c"]}
```

Kills every tab of a sidebar row. Failures on one tab don't stop the others; they're reported together in `error`.

#### `kill_remove_worktree`

```json
{"t":"cmd","id":4,"op":"kill_remove_worktree","session":"bosun-feat-1a2b3c4d",
 "worktree_path":"/w/api/.worktrees/feat","branch":"feat","merge":false}
```

Kills a worktree-backed session (the row's `worktree_path` and `branch`) and removes its worktree. With `merge: true` the branch is merged into the repo's current branch first and deleted afterwards. A worktree with uncommitted changes is never removed, and a conflicted merge is aborted; both come back as `ok: false` with the reason, and the worktree is left as it was.

#### `rename`

```json
{"t":"cmd","id":5,"op":"rename","session":"bosun-api-1a2b3c4d","display":"API server"}
```

Changes the display name. The internal name never changes. For a tab whose session has ended (a docked row's, or an exited one), the name is kept on the sidebar instead (`ended_names` in `config.toml`), and the tab comes back under it when it's resumed or restarted.

#### `restart`

```json
{"t":"cmd","id":6,"op":"restart","session":"bosun-api-1a2b3c4d","resume":true,"defer_launch":false}
```

Stops the agent (Ctrl-C until the pane is back at a shell) and launches it again in the same session, from the spec stored on it. `resume: true` relaunches into the agent's resume command (`claude --continue`, `codex resume --last`, …) for this launch only. The `ack` arrives once the old agent has stopped; the relaunch follows. With `defer_launch: true` the relaunch waits for the client, as with `create`, and the `ack` carries `pending_launch` with the `resume` value to pass back.

#### `launch_agent`

```json
{"t":"cmd","id":7,"op":"launch_agent","session":"bosun-api-1a2b3c4d","resume":null}
```

Types the agent's launch command into a session sitting at a bare shell, rebuilt from the spec stored on the session. `resume: null` uses the stored session mode; `true` or `false` overrides it for this launch. Send the `pending_launch` values from a deferred `create` or `restart` here.

#### `get_spec`

```json
{"t":"cmd","id":8,"op":"get_spec","session":"bosun-api-1a2b3c4d"}
```

Reads the spec stored on a live session, to pre-fill a modify form. The `ack` carries it in `spec`. Sessions Bosun didn't create have none (`ok: false`).

#### `get_theme`

```json
{"t":"cmd","id":9,"op":"get_theme","name":"flexoki-light"}
```

Reads any theme's palette, in the same form as `theme` in the snapshot (its `[terminal]` table included when it has one), so a client can preview themes while a picker is open. It changes nothing: no delta follows and nothing is written. The name must be one of `themes` (`ok: false` otherwise).

#### `modify`

```json
{"t":"cmd","id":9,"op":"modify","session":"bosun-api-1a2b3c4d","spec":{…}}
```

Stores a new spec on a live session (and its display name if that changed). The running agent is left alone; the next `restart` uses the new spec.

#### `delete_recent`

```json
{"t":"cmd","id":10,"op":"delete_recent","recent":42}
```

Removes one row from the recents list, by its `id`.

#### `sidebar_save`

```json
{"t":"cmd","id":11,"op":"sidebar_save","sidebar":{
  "ungrouped":[{"id":"con-1","name":"api","active":"bosun-api-1a2b3c4d","tabs":[{"session":"bosun-api-1a2b3c4d"}]}],
  "sections":[{"id":"sec-1","name":"Work","collapsed":false,"members":[]}]}}
```

Replaces the sidebar model after a drag and drop reorder, a move between sections, a collapse toggle or a tab reorder. Send back the [sidebar](#sidebar) from the last snapshot or delta, changed; a row's `name` and, on tabs, everything but `session` are ignored. Ids must be unique, every row needs at least one tab, `active` must be one of its tabs, and a session may appear only once. Sections and rows keep their ids, and a section's `defaults` and `role` and a row's `parked` stay as the daemon has them whether or not they're sent (moving a row into or out of Snoozed or Docked parks or unparks it; see `park_row`). The change is written to `config.toml` (where the TUI reads it), and section membership is remembered by display name as it is in the TUI, so a session that is restarted or recreated lands back in its section. Sessions the model leaves out are added back as ungrouped rows on the next refresh.

#### `move_tab`

```json
{"t":"cmd","id":18,"op":"move_tab","session":"bosun-api-2-9f8e7d6c","container_id":"con-1","index":0}
```

Moves one tab into another row, at `index` among that row's tabs (appended when absent). With `"container_id": null` the tab moves out into a new row of its own: at `index` among the rows of section `section` (or among the ungrouped rows when `section` is absent), or right after its old row when `index` is absent. An unknown section is an error. A row left with no tabs disappears, and the target row keeps its active tab. The session keeps running: only the grouping changes. The change is saved to `config.toml` and to each moved session's `@bosun_container_id`, so it survives a tmux server restart, and a TUI on the same config picks it up on its next reload. Moving a tab into the row it's already in is acked and changes nothing; an unknown session or row is an error.

#### `merge_rows`

```json
{"t":"cmd","id":19,"op":"merge_rows","from":"con-2","into":"con-1","index":1}
```

Moves every tab of row `from` into row `into`, in order, at `index` among that row's tabs (appended when absent), as one sidebar change (one `sidebar` delta). Row `from` disappears, and the target row keeps its active tab.

#### `insert_section`

```json
{"t":"cmd","id":12,"op":"insert_section","name":"Reviews"}
```

Adds an empty section at the end.

#### `rename_section`

```json
{"t":"cmd","id":13,"op":"rename_section","section":"sec-1","name":"Work (old)"}
```

Renames a section by id. Remembered section membership follows the new name.

#### `park_row` and `unpark_row`

```json
{"t":"cmd","id":15,"op":"park_row","container_id":"con-1","role":"snoozed","wake_at":1792134462123}
{"t":"cmd","id":16,"op":"park_row","container_id":"con-1","role":"snoozed","until_needed":true}
{"t":"cmd","id":17,"op":"park_row","container_id":"con-1","role":"docked"}
{"t":"cmd","id":18,"op":"unpark_row","container_id":"con-1"}
```

`park_row` moves a row to the end of the built-in Snoozed or Docked section, made when it's first needed (collapsed, after the user's own sections, Snoozed before Docked) and removed when its last row leaves. The row remembers the section and index it came from in its `parked` (see [Sidebar](#sidebar)); parking a row that's already parked moves it between the two and keeps where it came from.

- **Snoozed:** its sessions keep running. It goes back by itself at `wake_at` (Unix ms), or with `until_needed` as soon as one of its tabs needs you: a hook reports a finished turn after the row was snoozed, or a tab is waiting, done or in error when it wasn't at the moment it was snoozed.
- **Docked:** its live sessions are killed, and the row stays with its exited tabs.

`unpark_row` puts a parked row back at its old index in its old section (among the ungrouped rows if that section is gone); it's acknowledged and does nothing for a row that isn't parked. A row leaving Docked has each exited tab resumed: recreated from its recents entry (matched by display name, as the TUI's restart does) in the dead tab's place, with the agent's resume invocation. The tabs resume one at a time; one with no recents entry stays exited.

Moving a row into or out of Snoozed or Docked with `sidebar_save` does the same: a row dragged into Docked is docked, one dragged into Snoozed is snoozed until it needs you, and one dragged out goes where it was dropped (resumed, if it was docked). New sessions can't be created in either section.

#### `set_section_defaults`

```json
{"t":"cmd","id":14,"op":"set_section_defaults","section":"sec-1",
 "defaults":{"path":"~/work/api","agent":"codex","args":"--model o4","options":{"codex":{"session_mode":"continue","yolo":true}}}}
```

Stores what a new session made in that section starts with: a folder, an agent with its `options` (as in a [Spec](#spec)), and extra arguments. Every field is optional; an empty `path` or `args`, or no `agent`, leaves that field to the client's usual default. `"defaults": null` clears them. They're saved in `config.toml` with the section and sent in its `defaults` in the sidebar. `sidebar_save` never changes them. Bosun for Mac fills its new-session sheet from them for a session made in the section; `create` itself doesn't apply them.

#### `set_theme`

```json
{"t":"cmd","id":14,"op":"set_theme","name":"nord","persist":true}
```

Switches the theme. `persist: false` is a live preview (while a picker is open) that isn't written to `config.toml`; send the original name with `persist: false` to cancel it. `persist` defaults to `true`. The name must be one of `themes`.

#### `apply_setting`

```json
{"t":"cmd","id":15,"op":"apply_setting","key":"remove_dead_sessions","value":true}
```

Changes one settings-panel option and writes it to `config.toml`, the same as the TUI's settings panel. `key` is one of the `config.settings` keys; booleans take JSON booleans and choices take one of the listed `choices`. Pinned settings (an environment variable is set) and the read-only install settings are refused with the reason in `error`.

`theme_light` and `theme_dark` also take `null` (or `""`), which unsets that side and removes the key from `config.toml`:

```json
{"t":"cmd","id":16,"op":"apply_setting","key":"theme_dark","value":"tokyonight"}
{"t":"cmd","id":17,"op":"apply_setting","key":"theme_light","value":null}
```

Setting either never changes `theme`; see [Light and dark themes](#light-and-dark-themes).

`editor` takes any command (it isn't checked against `PATH`, which differs between a terminal and a GUI app), or `null` (or `""`) to unset it.

#### `client_flags`

```json
{"t":"cmd","id":16,"op":"client_flags","tty":"/dev/ttys026","ignore_size":true}
```

Sets or clears tmux's `ignore-size` flag on the tmux client attached through `tty`, using `refresh-client -t <tty> -f ignore-size` (or `-f '!ignore-size'`). A terminal view that is hidden in a warm pool sets it so it stops taking part in window-size negotiation; clear it when the view is shown again. `tty` is the client's terminal, which libghostty reports with `ghostty_surface_tty_name()` and tmux shows as `#{client_tty}`. It must be a `/dev/…` path of a client that is attached.

#### `make_latest`

```json
{"t":"cmd","id":17,"op":"make_latest","tty":"/dev/ttys026"}
```

Makes the client on `tty` tmux's latest client for its session, so under `window-size latest` the session takes that client's size straight away. This is what a view needs when it's shown again after another client (Moshi, the TUI) resized the session: clearing `ignore-size` alone isn't enough if the other client acted more recently and the view is the same size as before. `refresh-client` can't do this; the daemon runs `switch-client -c <tty> -t =<session>:` to the session the client is already on, which makes it latest without changing its last-session.

It has the side effects of a keypress in that client: tmux redraws the client and bumps the session's activity time, which can make an activity-based status read briefly as running. The alternative is for the view to write a focus-in report (`ESC [ I`) into its own pty, which tmux swallows and treats as input from that client; with tmux's `focus-events on`, Ghostty already does this when the view gains focus. Use `make_latest` when the view is shown without taking focus.

#### `search`

```json
{"t":"cmd","id":18,"op":"search","session":"bosun-api-1a2b3c4d","action":"find","text":"pages_manage"}
```

Searches a session's whole history. The history is tmux's, so the search runs in tmux's copy mode on the session's active pane: tmux scrolls the pane to the match and highlights every other one, in every client attached to it (the TUI showing the same session sees it too). `action` is one of:

- `find`: search for `text`, towards older output, from the bottom of the history. Send it on every keystroke: each one searches again from the bottom. An empty `text` ends the search, as `end` does.
- `older` and `newer`: the next match in that direction, wrapping round at the ends. If the pane has left copy mode since (`q` in it, or another client), they start the search for `text` again.
- `end`: leave copy mode, so the pane is back at its live output. It does nothing on a pane that isn't in copy mode.

`text` is literal (the daemon turns it into a regular expression for tmux, with every special character escaped). Case is ignored unless `match_case` is `true`. On a `find`, an optional `colors` sets how matches look, so they follow the client's theme rather than tmux's defaults (the terminal's magenta and cyan): `current_bg` and `current_fg` for the match the pane is on, `match_bg` and `match_fg` for every other one, each `#rrggbb`. The daemon sets them as the window's `copy-mode-current-match-style` and `copy-mode-match-style`, and leaves them out if any isn't `#rrggbb`.

```json
{"t":"cmd","id":19,"op":"search","session":"bosun-api-1a2b3c4d","action":"find","text":"TUI","match_case":true,
 "colors":{"current_bg":"#7c5cff","current_fg":"#f2f3f5","match_bg":"#33295b","match_fg":"#e6e9ef"}}
```

On bosun's own tmux server, starting a search also makes the mouse wheel in copy mode go to a program that takes the mouse on the alternate screen (leaving copy mode), since tmux keeps no history for it; elsewhere the wheel keeps tmux's own copy-mode binding.

The `ack` carries where the search stands in `search`: `found` (the pane is on a match), `count` (how many matches the whole history has; absent on tmux 3.4, which doesn't count them) and `partial` (`true` when tmux stopped counting early, so there are at least `count`). After `end`, `found` is `false`. A session that doesn't exist is an error.

```json
{"t":"ack","id":18,"ok":true,"search":{"found":true,"count":12}}
```

### Spec

What the new-session form collects:

| Field | Type | Notes |
|---|---|---|
| `name` | string | Display name. Required, no control characters. |
| `path` | string | Working directory. `~` is expanded. Empty means tmux's default. |
| `agent` | string | `claude`, `codex`, `kimi`, `opencode`, `qwen` or `terminal`. |
| `args` | string | Extra arguments appended to the agent command (for `terminal`, the whole command). |
| `options` | object | Per-agent options, below. Every field defaults to off / `new`. |
| `container_id` | string or null | Join this sidebar row as a new tab. |
| `resume` | bool | One-shot resume for this launch only (recreating an exited session). |
| `worktree_branch` | string or null | Create the session in a new git worktree on this branch, placed by the `worktree_location` setting. |

```json
"options": {
  "claude":   {"session_mode":"new","skip_permissions":false,"remote_control":false},
  "codex":    {"session_mode":"new","yolo":false},
  "kimi":     {"session_mode":"new","yolo":false},
  "opencode": {"session_mode":"new","auto":false},
  "qwen":     {"session_mode":"new","yolo":false}
}
```

`session_mode` is `new`, `continue` (resume the directory's last session) or `resume` (the agent's session picker). Only the block for the chosen agent matters.

## Daemon → client

### `snapshot`

```json
{"t":"snapshot","seq":1,
 "server":{"version":"2.2.0","protocol":1,"tmux_socket":"bosun"},
 "sessions":[…],"statuses":{…},"sidebar":{…},"recents":[…],"config":{…},"theme":{…},
 "theme_light":{…},"theme_dark":{…},"themes":[…],"keybindings":{…}}
```

The whole state. `server.tmux_socket` is the `-L` socket name terminal views attach with (`tmux -L <socket> attach -t =<session>`); it's absent when Bosun uses tmux's default socket. `notices` comes only when there's something to say: warnings that hold for as long as the daemon runs, such as a tmux server still running an older tmux than `tmux -V`, which can stop new sessions from starting. Each was also sent as a `notice` when it arose, but a client that connected later missed that, so show them the way you show a `notice`.

#### Sessions

One entry per live tmux session that Bosun manages, in tmux's order:

```json
{"name":"bosun-api-1a2b3c4d","display":"api","agent":"claude","path":"/w/api",
 "windows":1,"attached":true,"created":1792130862,
 "container_id":"con-1","worktree_path":"/w/api/.worktrees/feat","branch":"feat",
 "pane_title":"✳ Fix the flaky test","pane_command":"2_1_220"}
```

`name` is the key for every command. `path` is the path the session was created with, or the shell's working directory for sessions Bosun didn't create. `attached` is whether any tmux client is attached. `created` is Unix seconds. `pane_title` and `pane_command` are the active pane's title and foreground command. Optional fields are left out when unknown.

#### Statuses

```json
{"bosun-api-1a2b3c4d":{"state":"waiting","source":"hook","detail":"permission: Bash"},
 "bosun-web-9c0d1e2f":{"state":"idle","source":"hook","done_at":1760000123456},
 "bosun-docs-5e6f7a8b":{"state":"idle","source":"detector"}}
```

The agent state per session. `state` is `running`, `waiting` (blocked on you: a permission or question prompt), `done` (finished a turn), `idle`, `error` or `unknown`. `source` is `hook` when the agent reported the state itself through the hooks Bosun installs at launch (see the README's "Agent status from hooks"), or `detector` when Bosun worked it out from the pane title and contents. `detail` comes only with hook states and says what the state is about, e.g. `permission: Bash`, `question` or `plan approval` while waiting, or `background agent` (`2 background agents`) while running: the agent ended its turn with background agents still at work, and the turn goes on until they report back. `done_at` comes only with hook states, when the agent's own report is that it finished its turn (Claude Code's `Stop` hook): it's when it said so, in Unix milliseconds. `state` is `idle` then, as below. A newer turn finishing gives a newer `done_at`, and any other hook report (the user sent a prompt, a permission prompt opened) drops it. A client that wants to show "finished, your turn" compares it with the last time the user typed into that session. `started_at` comes while `state` is `running` or `waiting`: when the current turn started, in Unix milliseconds, for a working timer. A turn starts when the daemon first sees the session running and carries on through permission waits until the agent is done (a hook's done or idle ends it at once, and background agents keep it going; a detector state that drops out of running for less than five seconds keeps the same turn). The daemon keeps the start on the tmux session as `@bosun_turn_at`, so a daemon that starts while an agent is working carries on from it. One that finds no stored start (the session's turn began under a daemon older than 2.2.7) counts the turn from its own start. A session that's removed from `sessions` is removed from here too.

This is the state Bosun detects for the session itself. The TUI also shows an idle row as done when it ran and changed since you last looked at it; that depends on what each viewer has seen, so the daemon leaves it to the client.

#### Sidebar

```json
{"ungrouped":[
   {"id":"con-1","name":"api","active":"bosun-api-1a2b3c4d",
    "tabs":[{"session":"bosun-api-1a2b3c4d","display":"api","alive":true},
            {"session":"bosun-api-2-9f8e7d6c","display":"api 2","alive":false,"recent":12}]}],
 "sections":[
   {"id":"sec-1","name":"Work","collapsed":false,"banner_font":"metalix",
    "defaults":{"path":"~/work","agent":"codex","options":{…}},
    "members":[…rows like the ones above…]},
   {"id":"sec-2","name":"Snoozed","collapsed":true,"role":"snoozed",
    "members":[{"id":"con-2","name":"docs","active":"bosun-docs-3c4d5e6f","tabs":[…],
                "parked":{"at":1792130862123,"from_section":"sec-1","from_index":2,"wake_at":1792134462123}}]}]}
```

Ungrouped rows come first, then each section's header and rows, the same order as the TUI. Each row has one or more tabs; `active` is the session name of the shown tab. A row's `name` is its label as the TUI shows it: the display name of its active tab (for an exited tab, its kept or recovered name, as below), so it changes when the active tab changes or is renamed. A tab with `alive: false` is a session that has ended; the TUI keeps such rows (as "exited") so they can be restarted, unless `remove_dead_sessions` is on. Its `display` is the name it had when its row was docked or was given since, or else the best name Bosun can recover from the recents list, and `recent` is the id of the recents entry it was created from, to restart it from (absent when there's none). `banner_font` is left out when the section uses the global one, and `defaults` when the section has none (see `set_section_defaults`). `role` is `snoozed` or `docked` on the built-in sections parked rows go to (see `park_row`), and a row in one has `parked`: when it was parked (`at`, Unix ms), the section and index it goes back to (`from_section` is absent for the ungrouped rows), and for a snoozed row `wake_at` or `until_needed`.

#### Recents

```json
[{"id":42,"name":"api","path":"/w/api","agent":"claude","args":"","options":{…},"last_used_at":1792130862123,"use_count":7}]
```

Specs used before, most recent first (up to 200). `last_used_at` is Unix milliseconds. Use them to pre-fill a new-session form, or recreate an exited row whose display name matches.

#### Config

```json
{"settings":[
  {"key":"theme","value":"opencode","choices":["opencode","tokyonight",…],"read_only":false},
  {"key":"default_agent","value":"claude","choices":["claude","codex","kimi","opencode","qwen","terminal"],"pinned_by":"BOSUN_DEFAULT_AGENT","read_only":true},
  {"key":"remove_dead_sessions","value":false,"read_only":false},
  {"key":"tmux_socket","value":"bosun","read_only":true}]}
```

The settings panel as data. Keys: `theme`, `theme_light` and `theme_dark` (a theme name, or `null` when unset), `banner_font`, `default_agent`, `editor` (a command such as `zed` or `code`, or `null` when unset: what the TUI's `e` key runs and what Bosun for Mac opens file links in), `worktree_location` (`subdir` or `sibling`), `single_window`, `show_group_in_title`, `hide_single_tab_strip`, `remove_dead_sessions`, `embed_enabled`, plus the read-only install settings `session_prefix` and `tmux_socket` (null for tmux's default socket). `choices` is present for choice settings. `pinned_by` names the environment variable that pins a setting; environment variables beat `config.toml`, so a pinned setting is read-only, as it is in the TUI.

#### Theme

```json
{"name":"nord","bg":"#2e3440","panel":"#3b4252","panel_alt":"#434c5e","selection_bg":"#4c566a",
 "text":"#d8dee9","text_muted":"#6d7a90","accent":"#88c0d0","shadow":"#1a1f28","dim_fg":"#4c566a",
 "status_running":"#a3be8c","status_waiting":"#ebcb8b","status_idle":"#6d7a90","status_error":"#bf616a","status_done":"#8fbcbb"}
```

Every color slot of the active theme as `#rrggbb`: backgrounds (`bg` the deepest, then `panel`, `panel_alt`, `selection_bg` for the selected row), text (`text`, `text_muted`), `accent`, `shadow`, `dim_fg` (text dimmed behind a modal) and one color per status. `status_done` is already resolved: themes that don't set it use `accent`, as the TUI does.

A theme can also carry a `terminal` object, the theme's `[terminal]` table, with the colours a graphical client paints its terminals with:

```json
"terminal":{"ghostty":"Nord","background":"#2e3440","foreground":"#d8dee9","cursor":"#eceff4",
 "selection_bg":"#eceff4","selection_fg":"#4c566a",
 "palette":["#3b4252","#bf616a","#a3be8c","#ebcb8b","#81a1c1","#b48ead","#88c0d0","#e5e9f0",
            "#596377","#bf616a","#a3be8c","#ebcb8b","#81a1c1","#b48ead","#8fbcbb","#eceff4"]}
```

`palette` always has 16 entries: ANSI 0-7, then the bright 8-15. `ghostty` is the name of the Ghostty theme the colours were taken from, for reference; it is left out when there isn't one, and a client uses the colours, not the name. The key is optional and additive: a daemon that predates it never sends it, a theme without a `[terminal]` table leaves it out, and a client treats a missing `terminal` as "no table" (Bosun for Mac then falls back to its own name map and a generated palette). It needs no protocol version bump. A `[terminal]` table in a theme file that isn't exactly 16 valid colours is dropped by the daemon (with a warning in its log) rather than sent. The TUI ignores the table.

#### Light and dark themes

```json
"theme_light":{"name":"github-light","bg":"#ffffff",…},
"theme_dark":{"name":"tokyonight","bg":"#1a1b26",…}
```

An optional pair of themes for a client that follows the system's light and dark appearance (Bosun for Mac): `theme_light` and `theme_dark` in `config.toml`. Each is a whole palette in the same form as `theme`, sent only when that side is set to a theme that exists; an absent side means "use `theme`". A client uses `theme_light` while the system is in light appearance and `theme_dark` in dark, and falls back to `theme` for a side that isn't set, so with neither set it behaves exactly as before.

`theme` itself keeps its meaning: it's the TUI's theme (the TUI can't tell the system appearance, so it ignores the pair) and what a client shows when the pair isn't set. `set_theme` changes only `theme`, and setting the pair with `apply_setting` never changes `theme`. A client that lets the user pick the pair sends `apply_setting` with `theme_light` or `theme_dark`.

The palettes come with the snapshot rather than on request, so a client can switch the moment the appearance changes, before the daemon answers anything and from a cached snapshot.

#### Themes

```json
[{"name":"opencode","source":"bundled"},{"name":"my-theme","source":"user"}]
```

Every theme `set_theme` accepts, in picker order. `user` themes are `.toml` files in Bosun's `themes` config directory; one with a bundled name replaces the bundled theme.

#### Keybindings

```json
{"previous_tab":"Shift+Left","next_tab":"Shift+Right","previous_session":"Shift+Up","next_session":"Shift+Down","send_next_key":"Ctrl+v"}
```

The TUI's navigation shortcuts from the `[keybindings]` table in `config.toml`, so a remap applies to the TUI and the app alike. Each value is written the way a user writes it in that table: the modifiers `Ctrl`, `Alt` and `Shift` (in that order) joined to the key with `+`, where the key is `Left`, `Right`, `Up`, `Down`, `F1` to `F12`, or a lowercase letter (a letter always comes with `Ctrl` or `Alt`). A disabled action is `null`, and every field is always present. When the table is invalid the TUI falls back to the defaults shown above, and so does this. `send_next_key` is the TUI's key for passing the next key straight through to the agent.

### Deltas

Each delta replaces one part of the snapshot.

```json
{"t":"sessions","seq":3,"upsert":[{…full session…}],"removed":["bosun-old-0f0f0f0f"]}
{"t":"status","seq":4,"session":"bosun-api-1a2b3c4d","state":"waiting","source":"hook","detail":"permission: Bash"}
{"t":"sidebar","seq":5,"sidebar":{…whole sidebar…}}
{"t":"recents","seq":6,"recents":[…whole list…]}
{"t":"config","seq":7,"config":{…whole config…}}
{"t":"theme","seq":8,"theme":{…whole palette…}}
{"t":"themes","seq":9,"themes":[…whole list…]}
{"t":"theme_pair","seq":10,"theme_light":{…whole palette…},"theme_dark":{…whole palette…}}
{"t":"keybindings","seq":11,"keybindings":{…whole object…}}
```

- `sessions`: `upsert` holds full rows that are new or changed, `removed` the names of sessions that are gone (their statuses go with them). Either may be absent. Row order follows tmux's; a client that cares about order uses the sidebar.
- `status`: one session's state, source, detail, `done_at` or `started_at` changed. The fields are the same as one entry in the snapshot's `statuses`. A state an agent reports through its hook goes out within about 250 ms: the daemon re-reads every session's hook state, pane title and command four times a second over its control-mode connection. States the detectors work out from the pane's contents follow the once-a-second refresh, as in the TUI. (If the control-mode connection is down, hook states fall back to the once-a-second refresh too.)
- `theme_pair`: the Light/Dark pair changed. It replaces both `theme_light` and `theme_dark`; a side that's absent (or `null`) is now unset.
- The others replace that part of the snapshot whole. These are small and change rarely (on a command, a reconcile, or an edit to `config.toml`). `keybindings` is sent when an edit to `config.toml` changes what the shortcuts resolve to.

The daemon picks up changes to `config.toml` made elsewhere (the TUI's theme picker or settings panel, a text editor) within a second and sends the matching deltas, so a theme change in either the TUI or the app shows in both.

### `notice`

```json
{"t":"notice","seq":12,"message":"created bosun-api-1a2b3c4d"}
```

A message for a person, the same text the TUI shows in its status bar: confirmations and errors from Bosun's own work, and warnings no command asked for, such as "api closed as soon as it started. See docs/troubleshooting.md" for a new session whose shell exited straight away. Show it as a toast or ignore it. Messages caused by a command are also listed in that command's `ack`; the unprompted warnings never are.

### `client_session` and `client_detached`

```json
{"t":"client_session","seq":13,"tty":"/dev/ttys026","session":"bosun-api-1a2b3c4d"}
{"t":"client_detached","seq":14,"tty":"/dev/ttys026"}
```

A tmux client attached to Bosun's tmux server changed session or went away. `tty` is the client's terminal, the same value `client_flags` takes (`#{client_tty}`, which libghostty reports with `ghostty_surface_tty_name()`); `session` is the tmux session name it's on now. They come from tmux control mode's `%client-session-changed` and `%client-detached`, for every terminal client on the server (control-mode clients are left out), so a client ignores ttys that aren't its own.

`client_session` is sent when a client attaches, when anything moves it (`switch-client`, `choose-tree`, the `prefix 1-9` and Shift+arrow bindings Bosun installs for the TUI, typed inside the terminal), and also when a client is switched to the session it's already on, which `make_latest` does. Compare `session` with what the terminal is meant to show and act only on a difference. `client_detached` follows a detach from any cause (`C-q`, `detach-client`, the session ending); the `tmux attach` process in that terminal exits at the same time.

These are events, not state: they aren't in the `snapshot` and a `resync` doesn't repeat them.

### `ack`

```json
{"t":"ack","id":7,"ok":false,"error":"worktree has uncommitted changes; not removed","messages":["worktree has uncommitted changes; not removed"]}
```

The reply to one `cmd`. `ok` says whether it worked; `error` is a human-readable reason when it didn't. Optional fields: `session` (`create`), `spec` (`get_spec`), `pending_launch` (`create` and `restart` with `defer_launch`), `search` (`search`) and `messages` (every `notice` the command caused).

### `error`

```json
{"t":"error","code":"bad_message","message":"unknown variant `nuke`, expected one of …"}
```

A protocol problem rather than a command failure. Codes: `protocol_mismatch` and `no_hello` close the connection; `bad_message` (unparseable JSON, an unknown `t` or `op`, a missing field) doesn't.

## Terminal clients

A GUI's terminal views attach with `tmux -L <socket> attach -t =<session>` in their own ptys (see `server.tmux_socket`). They should run that `tmux` with `TERM=xterm-ghostty-bosun`, a copy of Ghostty's `xterm-ghostty` terminfo entry under a name of its own (Bosun for Mac ships it in its bundle and points `TERMINFO` at it). tmux shows that name as `#{client_termname}`, and it's how Bosun tells a GUI's clients from the TUI's preview, Moshi or a plain `tmux attach`:

- Bosun's status line hints (`^Q detach · Shift+Left / Shift+Right cycle · …`) are the TUI's keys, so they're blank on these clients.
- A session that only these clients are attached to has no status line at all (`status off`), so each terminal gets that row back. As soon as any other client attaches, or none is attached, the session's status line is on again. tmux has no per-client status line, so while a session is shown both in the GUI and elsewhere, the GUI's terminals have an empty bottom row.

Both Bosun processes that may share a tmux server (the TUI and `bosun serve`) apply this the same way when a client attaches, moves or detaches.

## Debugging with socat or nc

Frames have a binary length prefix, so typing them by hand is awkward, but watching works. This sends a `hello` and prints what comes back for a few seconds (`socat - UNIX-CONNECT:"$SOCK"` works the same as `nc -U "$SOCK"`):

```sh
SOCK="$(bosun serve --print-socket)"
(python3 -c 'import json,struct,sys; b=json.dumps({"t":"hello","protocol":1}).encode(); sys.stdout.buffer.write(struct.pack(">I",len(b))+b)'; sleep 5) \
  | nc -U "$SOCK" | LC_ALL=C strings
```

## How commands reach tmux

The daemon (and the TUI) send tmux commands over a `tmux -C` control-mode connection rather than starting a `tmux` process for each one, and match replies by their `%begin`/`%end` numbers. See `src/tmux/command_channel.rs` for the details, including how arguments are quoted. Terminal views never go through the daemon: each one runs its own `tmux attach` in its own pty, so typing never touches it.
