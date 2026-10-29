import XCTest
@testable import BosunCore

final class ProtocolTests: XCTestCase {
    private func decode(_ json: String) throws -> ServerMessage {
        try ServerMessage.decode(Data(json.utf8))
    }

    static let snapshotJSON = """
    {"t":"snapshot","seq":1,
     "server":{"version":"2.2.0","protocol":1,"tmux_socket":"bosun"},
     "sessions":[{"name":"bosun-api-1a2b3c4d","display":"api","agent":"claude","path":"/w/api",
                  "windows":1,"attached":true,"created":1792130862,
                  "container_id":"con-1","worktree_path":"/w/api/.worktrees/feat","branch":"feat",
                  "pane_title":"✳ Fix the flaky test","pane_command":"2_1_220"}],
     "statuses":{"bosun-api-1a2b3c4d":{"state":"waiting","source":"hook","detail":"permission: Bash"},
                 "bosun-web-9c0d1e2f":{"state":"idle","source":"hook","done_at":1760000123456},
                 "bosun-docs-5e6f7a8b":{"state":"idle","source":"detector"}},
     "sidebar":{"ungrouped":[
        {"id":"con-1","name":"api","active":"bosun-api-1a2b3c4d",
         "tabs":[{"session":"bosun-api-1a2b3c4d","display":"api","alive":true},
                 {"session":"bosun-api-2-9f8e7d6c","display":"api 2","alive":false,"recent":12}]}],
       "sections":[{"id":"sec-1","name":"Work","collapsed":false,"banner_font":"metalix","members":[],
                     "defaults":{"path":"~/work","agent":"codex","options":{"codex":{"session_mode":"continue","yolo":true}}}}]},
     "recents":[{"id":42,"name":"api","path":"/w/api","agent":"claude","args":"","options":{"claude":{"session_mode":"continue","skip_permissions":false,"remote_control":false}},"last_used_at":1792130862123,"use_count":7}],
     "config":{"settings":[
        {"key":"theme","value":"opencode","choices":["opencode","tokyonight"],"read_only":false},
        {"key":"default_agent","value":"claude","choices":["claude","codex","kimi","opencode","qwen","terminal"],"pinned_by":"BOSUN_DEFAULT_AGENT","read_only":true},
        {"key":"remove_dead_sessions","value":false,"read_only":false},
        {"key":"tmux_socket","value":"bosun","read_only":true}]},
     "theme":{"name":"nord","bg":"#2e3440","panel":"#3b4252","panel_alt":"#434c5e","selection_bg":"#4c566a",
       "text":"#d8dee9","text_muted":"#6d7a90","accent":"#88c0d0","shadow":"#1a1f28","dim_fg":"#4c566a",
       "status_running":"#a3be8c","status_waiting":"#ebcb8b","status_idle":"#6d7a90","status_error":"#bf616a","status_done":"#8fbcbb"},
     "themes":[{"name":"opencode","source":"bundled"},{"name":"my-theme","source":"user"}],
     "some_future_field":{"x":1}}
    """

    func testSnapshotDecodes() throws {
        guard case .snapshot(let s) = try decode(Self.snapshotJSON) else { return XCTFail("not a snapshot") }
        XCTAssertEqual(s.seq, 1)
        XCTAssertEqual(s.server.tmuxSocket, "bosun")
        XCTAssertEqual(s.server.protocolMajor, 1)
        XCTAssertEqual(s.sessions.first?.containerID, "con-1")
        XCTAssertEqual(s.sessions.first?.paneCommand, "2_1_220")
        XCTAssertEqual(s.statuses["bosun-api-1a2b3c4d"]?.state, .waiting)
        XCTAssertEqual(s.statuses["bosun-api-1a2b3c4d"]?.detail, "permission: Bash")
        XCTAssertEqual(s.statuses["bosun-docs-5e6f7a8b"]?.source, "detector")
        XCTAssertNil(s.statuses["bosun-docs-5e6f7a8b"]?.doneAt)
        XCTAssertEqual(s.statuses["bosun-web-9c0d1e2f"]?.doneAt, 1_760_000_123_456)
        XCTAssertEqual(s.statuses["bosun-web-9c0d1e2f"]?.isHook, true)
        XCTAssertEqual(s.sidebar.ungrouped.first?.tabs.count, 2)
        XCTAssertEqual(s.sidebar.ungrouped.first?.tabs[1].alive, false)
        XCTAssertEqual(s.sidebar.ungrouped.first?.tabs[1].recent, 12)
        XCTAssertNil(s.sidebar.ungrouped.first?.tabs[0].recent)
        XCTAssertEqual(s.sidebar.sections.first?.bannerFont, "metalix")
        let d = s.sidebar.sections.first?.defaults
        XCTAssertEqual(d?.path, "~/work")
        XCTAssertEqual(d?.agent, "codex")
        XCTAssertEqual(d?.args, "")
        XCTAssertEqual(d?.options.codex.yolo, true)
        XCTAssertEqual(d?.options.codex.sessionMode, .continue)
        XCTAssertEqual(s.recents.first?.options.claude.sessionMode, .continue)
        XCTAssertEqual(s.recents.first?.options.codex.sessionMode, .new)
        XCTAssertEqual(s.config.settings[1].pinnedBy, "BOSUN_DEFAULT_AGENT")
        XCTAssertEqual(s.config.value("remove_dead_sessions"), .bool(false))
        XCTAssertEqual(s.theme.panelAlt, "#434c5e")
        XCTAssertEqual(s.themes.last?.source, "user")
        XCTAssertNil(s.keybindings)
        XCTAssertEqual(s.notices, [])
    }

    /// Standing notices (an out-of-date tmux server) ride in the snapshot, so a client that connected
    /// after they were sent still shows them.
    func testSnapshotNoticesArePassedOnAsNotices() throws {
        let json = Self.snapshotJSON.replacingOccurrences(of: #""some_future_field""#,
                                                          with: #""notices":["tmux server is old"],"some_future_field""#)
        guard case .snapshot(let s) = try decode(json) else { return XCTFail("not a snapshot") }
        XCTAssertEqual(s.notices, ["tmux server is old"])
        var state = BosunState()
        XCTAssertEqual(state.apply(.snapshot(s)), .applied([.snapshot, .notice("tmux server is old")]))
    }

    func testDeltasDecode() throws {
        XCTAssertEqual(try decode(#"{"t":"status","seq":2,"session":"bosun-api-1a2b3c4d","state":"running","source":"hook"}"#),
                       .status(seq: 2, session: "bosun-api-1a2b3c4d", info: StatusInfo(state: .running, source: "hook")))
        XCTAssertEqual(try decode(#"{"t":"status","seq":4,"session":"s","state":"waiting","source":"hook","detail":"permission: Bash"}"#),
                       .status(seq: 4, session: "s", info: StatusInfo(state: .waiting, source: "hook", detail: "permission: Bash")))
        XCTAssertEqual(try decode(#"{"t":"sessions","seq":3,"removed":["bosun-old-0f0f0f0f"]}"#),
                       .sessions(seq: 3, upsert: [], removed: ["bosun-old-0f0f0f0f"]))
        XCTAssertEqual(try decode(#"{"t":"notice","seq":10,"message":"created bosun-api-1a2b3c4d"}"#),
                       .notice(seq: 10, "created bosun-api-1a2b3c4d"))
        XCTAssertEqual(try decode(#"{"t":"client_session","seq":11,"tty":"/dev/ttys026","session":"bosun-api-1a2b3c4d"}"#),
                       .client(seq: 11, tty: "/dev/ttys026", session: "bosun-api-1a2b3c4d"))
        XCTAssertEqual(try decode(#"{"t":"client_detached","seq":12,"tty":"/dev/ttys026"}"#),
                       .client(seq: 12, tty: "/dev/ttys026", session: nil))
        if case .themes(let q, let t) = try decode(#"{"t":"themes","seq":9,"themes":[{"name":"nord","source":"bundled"}]}"#) {
            XCTAssertEqual(q, 9)
            XCTAssertEqual(t.first?.name, "nord")
        } else { XCTFail() }
        if case .sidebar(let q, let sb) = try decode(#"{"t":"sidebar","seq":5,"sidebar":{"ungrouped":[],"sections":[{"id":"sec-1","name":"Work","members":[]}]}}"#) {
            XCTAssertEqual(q, 5)
            XCTAssertEqual(sb.sections.first?.collapsed, false)
        } else { XCTFail() }
        XCTAssertEqual(try decode(#"{"t":"status","seq":7,"session":"s","state":"idle","source":"hook","done_at":1760000123456}"#),
                       .status(seq: 7, session: "s", info: StatusInfo(state: .idle, source: "hook", doneAt: 1_760_000_123_456)))
        XCTAssertEqual(try decode(#"{"t":"status","seq":8,"session":"s","state":"running","source":"hook","started_at":1760000100000}"#),
                       .status(seq: 8, session: "s", info: StatusInfo(state: .running, source: "hook", startedAt: 1_760_000_100_000)))
        // A future state word decodes as unknown rather than failing.
        if case .status(_, _, let info) = try decode(#"{"t":"status","seq":6,"session":"s","state":"thinking","source":"hook"}"#) {
            XCTAssertEqual(info.state, .unknown)
        } else { XCTFail() }
    }

    func testAckAndError() throws {
        let ack = try decode(#"{"t":"ack","id":1,"ok":true,"session":"bosun-api-1a2b3c4d","pending_launch":{"session":"bosun-api-1a2b3c4d","resume":null},"messages":["created bosun-api-1a2b3c4d"]}"#)
        guard case .ack(let a) = ack else { return XCTFail() }
        XCTAssertTrue(a.ok)
        XCTAssertEqual(a.pendingLaunch?.session, "bosun-api-1a2b3c4d")
        XCTAssertNil(a.pendingLaunch?.resume)
        XCTAssertNil(ack.seq)
        guard case .ack(let bad) = try decode(#"{"t":"ack","id":7,"ok":false,"error":"worktree has uncommitted changes; not removed"}"#) else { return XCTFail() }
        XCTAssertFalse(bad.ok)
        XCTAssertEqual(bad.messages, [])
        XCTAssertEqual(try decode(#"{"t":"error","code":"protocol_mismatch","message":"update bosun"}"#),
                       .error(code: "protocol_mismatch", message: "update bosun"))
    }

    func testSearchAckAndCommand() throws {
        guard case .ack(let a) = try decode(#"{"t":"ack","id":3,"ok":true,"search":{"found":true,"count":12}}"#) else { return XCTFail() }
        XCTAssertEqual(a.search, SearchState(found: true, count: 12))
        XCTAssertEqual(a.search?.label, "12 matches")
        guard case .ack(let none) = try decode(#"{"t":"ack","id":4,"ok":true,"search":{"found":false}}"#) else { return XCTFail() }
        XCTAssertEqual(none.search?.label, "No matches")
        XCTAssertEqual(SearchState(found: true, count: 1).label, "1 match")
        XCTAssertEqual(SearchState(found: true, count: 1, partial: true).label, "1+ matches")
        // tmux 3.4 doesn't count.
        XCTAssertEqual(SearchState(found: true).label, "Found")
        let body = try ClientMessage.cmd(id: 5, .search(session: "s", action: .older, text: "pages_manage")).encoded()
        let obj = try JSONSerialization.jsonObject(with: body) as! [String: Any]
        XCTAssertEqual(obj["op"] as? String, "search")
        XCTAssertEqual(obj["action"] as? String, "older")
        XCTAssertEqual(obj["text"] as? String, "pages_manage")
        XCTAssertNil(obj["match_case"])
        XCTAssertNil(obj["colors"])
        let colors = SearchColors(currentBg: "#bd93f9", currentFg: "#000000", matchBg: "#4b3f6b", matchFg: "#f8f8f2")
        let cased = try ClientMessage.cmd(id: 6, .search(session: "s", action: .find, text: "TUI", matchCase: true, colors: colors)).encoded()
        let o2 = try JSONSerialization.jsonObject(with: cased) as! [String: Any]
        XCTAssertEqual(o2["match_case"] as? Bool, true)
        XCTAssertEqual(o2["colors"] as? [String: String], ["current_bg": "#bd93f9", "current_fg": "#000000",
                                                           "match_bg": "#4b3f6b", "match_fg": "#f8f8f2"])
    }

    func testUnknownKindKeepsSeq() throws {
        XCTAssertEqual(try decode(#"{"t":"wobble","seq":12,"x":1}"#), .unknown(kind: "wobble", seq: 12))
    }

    func testKeybindingsFieldAndDelta() throws {
        let json = #"{"t":"keybindings","seq":3,"keybindings":{"previous_tab":"Shift+Left","next_tab":null,"previous_session":"Alt+Up","send_next_key":"Ctrl+v"}}"#
        guard case .keybindings(let q, let k) = try decode(json) else { return XCTFail() }
        XCTAssertEqual(q, 3)
        XCTAssertEqual(k.previousTab, .chord("Shift+Left"))
        XCTAssertEqual(k.nextTab, .disabled)
        XCTAssertEqual(k.previousSession, .chord("Alt+Up"))
        XCTAssertEqual(k.nextSession, .unset)
        let nav = NavBindings(k)
        XCTAssertNil(nav.chords[.nextTab])
        XCTAssertEqual(nav.chords[.nextSession], KeyChord(key: .down, mods: [.shift]))
        XCTAssertEqual(nav.chords[.previousSession], KeyChord(key: .up, mods: [.option]))
    }

    func testCommandEncoding() throws {
        func json(_ m: ClientMessage) throws -> [String: Any] {
            try JSONSerialization.jsonObject(with: try m.encoded()) as! [String: Any]
        }
        let hello = try json(.hello(client: "bosun-mac/0.2"))
        XCTAssertEqual(hello["t"] as? String, "hello")
        XCTAssertEqual(hello["protocol"] as? Int, 1)

        let restart = try json(.cmd(id: 7, .restart(session: "s", resume: true, deferLaunch: false)))
        XCTAssertEqual(restart["t"] as? String, "cmd")
        XCTAssertEqual(restart["op"] as? String, "restart")
        XCTAssertEqual(restart["id"] as? Int, 7)
        XCTAssertEqual(restart["resume"] as? Bool, true)

        let launch = try json(.cmd(id: 8, .launchAgent(session: "s", resume: nil)))
        XCTAssertTrue(launch["resume"] is NSNull)

        var spec = Spec(name: "api", path: "~/work/api", agent: "claude", args: "--model opus")
        spec.options.claude.sessionMode = .continue
        let create = try json(.cmd(id: 1, .create(spec: spec, deferLaunch: true)))
        XCTAssertEqual(create["defer_launch"] as? Bool, true)
        XCTAssertNil(create["section"])
        XCTAssertNil(create["replaces"])
        let inSection = try json(.cmd(id: 2, .create(spec: spec, deferLaunch: true, section: "sec-1")))
        XCTAssertEqual(inSection["section"] as? String, "sec-1")
        let replacing = try json(.cmd(id: 2, .create(spec: spec, deferLaunch: true, replaces: "bosun-api-1")))
        XCTAssertEqual(replacing["replaces"] as? String, "bosun-api-1")
        let clear = try json(.cmd(id: 3, .setSectionDefaults(section: "sec-1", defaults: nil)))
        XCTAssertEqual(clear["op"] as? String, "set_section_defaults")
        XCTAssertTrue(clear["defaults"] is NSNull)
        let set = try json(.cmd(id: 4, .setSectionDefaults(section: "sec-1", defaults: SectionDefaults(path: "~/w", agent: "claude"))))
        XCTAssertEqual((set["defaults"] as? [String: Any])?["path"] as? String, "~/w")
        let s = create["spec"] as! [String: Any]
        XCTAssertEqual(s["agent"] as? String, "claude")
        XCTAssertEqual(((s["options"] as! [String: Any])["claude"] as! [String: Any])["session_mode"] as? String, "continue")

        let sb = Sidebar(ungrouped: [Container(id: "con-1", name: "api", active: "a", tabs: [Tab(session: "a", display: "api", alive: true)])],
                         sections: [Section(id: "sec-1", name: "Work")])
        let save = try json(.cmd(id: 11, .sidebarSave(sb)))
        let tabs = ((save["sidebar"] as! [String: Any])["ungrouped"] as! [[String: Any]])[0]["tabs"] as! [[String: Any]]
        XCTAssertEqual(tabs[0].keys.sorted(), ["session"])

        let flags = try json(.cmd(id: 16, .clientFlags(tty: "/dev/ttys026", ignoreSize: true)))
        XCTAssertEqual(flags["op"] as? String, "client_flags")
        XCTAssertEqual(flags["ignore_size"] as? Bool, true)
    }

    func testGetThemeEncodesAndItsAckCarriesThePalette() throws {
        let m = try JSONSerialization.jsonObject(with: try ClientMessage.cmd(id: 30, .getTheme(name: "nord")).encoded()) as! [String: Any]
        XCTAssertEqual(m["op"] as? String, "get_theme")
        XCTAssertEqual(m["name"] as? String, "nord")
        guard case .ack(let a) = try decode(#"{"t":"ack","id":30,"ok":true,"theme":"# + Self.themeJSON + "}}") else { return XCTFail() }
        XCTAssertEqual(a.theme?.name, "nord")
        guard case .ack(let other) = try decode(#"{"t":"ack","id":31,"ok":true}"#) else { return XCTFail() }
        XCTAssertNil(other.theme)
    }

    func testFrameRoundTrip() throws {
        var r = FrameReader()
        let a = Frame.encode(Data(#"{"t":"hello"}"#.utf8))
        let b = Frame.encode(Data(#"{"t":"resync"}"#.utf8))
        let all = a + b
        // Feed one byte at a time: frames must come out whole and in order.
        var out: [String] = []
        for byte in all {
            r.append(Data([byte]))
            while let f = try r.next() { out.append(String(decoding: f, as: UTF8.self)) }
        }
        XCTAssertEqual(out, [#"{"t":"hello"}"#, #"{"t":"resync"}"#])
        XCTAssertEqual(Array(a.prefix(4)), [0, 0, 0, 13])

        var big = FrameReader()
        big.append(Data([0x02, 0x00, 0x00, 0x00]))
        XCTAssertThrowsError(try big.next())
    }

    func testStateAppliesDeltasAndDetectsGaps() throws {
        var st = BosunState()
        XCTAssertEqual(st.apply(try decode(Self.snapshotJSON)), .applied([.snapshot]))
        XCTAssertTrue(st.live)
        _ = st.apply(try decode(#"{"t":"status","seq":2,"session":"bosun-api-1a2b3c4d","state":"running","source":"hook"}"#))
        XCTAssertEqual(st.state(of: "bosun-api-1a2b3c4d"), .running)
        let up = #"{"t":"sessions","seq":3,"upsert":[{"name":"bosun-new-1","display":"new","windows":1,"attached":false}],"removed":["bosun-api-1a2b3c4d"]}"#
        XCTAssertEqual(st.apply(try decode(up)), .applied([.sessions(changed: ["bosun-new-1"], removed: ["bosun-api-1a2b3c4d"])]))
        XCTAssertNil(st.statuses["bosun-api-1a2b3c4d"])
        XCTAssertEqual(st.sessionOrder, ["bosun-new-1"])
        // seq 5 after 3: a gap.
        XCTAssertEqual(st.apply(try decode(#"{"t":"notice","seq":5,"message":"x"}"#)), .gap(expected: 4, got: 5))
        // An unknown kind still advances seq.
        XCTAssertEqual(st.apply(try decode(#"{"t":"wobble","seq":4}"#)), .applied([]))
        XCTAssertEqual(st.apply(try decode(#"{"t":"client_session","seq":5,"tty":"/dev/ttys026","session":"bosun-new-1"}"#)),
                       .applied([.client(tty: "/dev/ttys026", session: "bosun-new-1")]))
        XCTAssertEqual(st.apply(try decode(#"{"t":"client_detached","seq":6,"tty":"/dev/ttys026"}"#)),
                       .applied([.client(tty: "/dev/ttys026", session: nil)]))
        XCTAssertEqual(st.apply(try decode(##"{"t":"theme","seq":7,"theme":{"name":"nord","bg":"#000000","panel":"#111111","panel_alt":"#222222","selection_bg":"#333333","text":"#ffffff","text_muted":"#888888","accent":"#88c0d0","shadow":"#000000","dim_fg":"#444444","status_running":"#00ff00","status_waiting":"#ffff00","status_idle":"#888888","status_error":"#ff0000","status_done":"#0000ff"}}"##)), .applied([.theme]))
        XCTAssertEqual(st.theme.bg, "#000000")
        // The cache round-trips, and a cached snapshot is never live.
        let snap = try XCTUnwrap(st.snapshot())
        let data = try JSONEncoder().encode(snap)
        var cached = BosunState()
        cached.loadCached(try JSONDecoder().decode(Snapshot.self, from: data))
        XCTAssertFalse(cached.live)
        XCTAssertEqual(cached.sessionOrder, ["bosun-new-1"])
        XCTAssertEqual(cached.apply(try decode(#"{"t":"notice","seq":1,"message":"x"}"#)), .gap(expected: 0, got: 1))
    }

    private static let themeJSON = ###"{"name":"nord","bg":"#2e3440","panel":"#3b4252","panel_alt":"#434c5e","selection_bg":"#4c566a","text":"#d8dee9","text_muted":"#81899c","accent":"#88c0d0","shadow":"#1a1f28","dim_fg":"#4c566a","status_running":"#a3be8c","status_waiting":"#ebcb8b","status_idle":"#6d7a90","status_error":"#bf616a","status_done":"#8fbcbb""###

    func testThemeWithoutATerminalTableDecodesAsNil() throws {
        // An older daemon's theme has no `terminal` key.
        let p = try JSONDecoder().decode(ThemePalette.self, from: Data((Self.themeJSON + "}").utf8))
        XCTAssertEqual(p.name, "nord")
        XCTAssertNil(p.terminal)
        // And a `null` reads the same.
        let q = try JSONDecoder().decode(ThemePalette.self, from: Data((Self.themeJSON + #","terminal":null}"#).utf8))
        XCTAssertNil(q.terminal)
    }

    func testThemeTerminalTableDecodesAndRoundTrips() throws {
        let palette = (0..<16).map { String(format: "\"#%02x%02x%02x\"", $0, $0, $0) }.joined(separator: ",")
        let table = ##","terminal":{"ghostty":"Nord","background":"#2e3440","foreground":"#d8dee9","cursor":"#eceff4","selection_bg":"#eceff4","selection_fg":"#4c566a","palette":["##
            + palette + "]}}"
        let p = try JSONDecoder().decode(ThemePalette.self, from: Data((Self.themeJSON + table).utf8))
        let t = try XCTUnwrap(p.terminal)
        XCTAssertEqual(t.ghostty, "Nord")
        XCTAssertEqual(t.background, "#2e3440")
        XCTAssertEqual(t.selectionBg, "#eceff4")
        XCTAssertEqual(t.selectionFg, "#4c566a")
        XCTAssertEqual(t.palette.count, 16)
        XCTAssertEqual(t.palette[15], "#0f0f0f")
        let again = try JSONDecoder().decode(ThemePalette.self, from: JSONEncoder().encode(p))
        XCTAssertEqual(again, p)
        // The ghostty name is optional.
        let noName = table.replacingOccurrences(of: #""ghostty":"Nord","#, with: "")
        XCTAssertNil(try JSONDecoder().decode(ThemePalette.self, from: Data((Self.themeJSON + noName).utf8)).terminal?.ghostty)
        // It arrives in a theme message too.
        let msg = try decode(##"{"t":"theme","seq":8,"theme":"## + Self.themeJSON + table + "}")
        if case .theme(_, let th) = msg { XCTAssertEqual(th.terminal?.palette.count, 16) } else { XCTFail("not a theme message") }
    }

    /// The first-launch palette is the opencode theme file, so a first launch doesn't flash another theme.
    func testFallbackPaletteMatchesTheOpencodeThemeFile() throws {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent()
        let toml = try String(contentsOf: root.appendingPathComponent("themes/opencode.toml"), encoding: .utf8)
        var values: [String: String] = [:]
        var palette: [String] = []
        var inTerminal = false
        for raw in toml.split(whereSeparator: \.isNewline) {
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line == "[terminal]" { inTerminal = true; continue }
            if inTerminal, line.hasPrefix("\"") {
                palette += line.split(separator: ",").map { $0.trimmingCharacters(in: CharacterSet(charactersIn: " \"")) }.filter { !$0.isEmpty }
                continue
            }
            let parts = line.split(separator: "=", maxSplits: 1).map { $0.trimmingCharacters(in: CharacterSet(charactersIn: " \"")) }
            if parts.count == 2 { values[(inTerminal ? "terminal." : "") + parts[0]] = parts[1] }
        }
        let f = ThemePalette.fallback
        let expected: [(String, String)] = [
            ("name", f.name), ("bg", f.bg), ("panel", f.panel), ("panel_alt", f.panelAlt), ("selection_bg", f.selectionBg),
            ("text", f.text), ("text_muted", f.textMuted), ("accent", f.accent), ("shadow", f.shadow), ("dim_fg", f.dimFg),
            ("status_running", f.statusRunning), ("status_waiting", f.statusWaiting), ("status_idle", f.statusIdle),
            ("status_error", f.statusError), ("status_done", f.statusDone),
            ("terminal.background", f.terminal?.background ?? ""), ("terminal.foreground", f.terminal?.foreground ?? ""),
            ("terminal.cursor", f.terminal?.cursor ?? ""), ("terminal.selection_bg", f.terminal?.selectionBg ?? ""),
            ("terminal.selection_fg", f.terminal?.selectionFg ?? ""),
        ]
        for (key, value) in expected { XCTAssertEqual(values[key], value, key) }
        XCTAssertEqual(palette, f.terminal?.palette)
    }
}
