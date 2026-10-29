import AppKit
import BosunCore

// Focus-test section for finding in a pane (Cmd+F): the bar opens on the active pane and borrows the
// keyboard like the filter does, typing searches the session's whole history through tmux's copy mode,
// Return / Shift+Return and Cmd+G / Cmd+Shift+G step through the matches, case is ignored unless the Aa
// toggle is on, matches are marked in the theme's colours, Esc closes it and takes the pane back to its
// live output, a click in the pane keeps the pane where the search took it, and the filter
// (Option+Cmd+F), the palette, another row or a drop close it.

extension FocusTest {
    /// The find bar open on the active pane.
    var findBar: PaneFindBar? { ws.findBar(for: ws.activeTile) }

    /// The find bar's field editor has the keyboard.
    var findFieldHasKeyboard: Bool { ((w.firstResponder as? NSTextView)?.delegate as AnyObject?) is FindField }

    /// tmux's view of a pane: "<in a mode>|<the line the copy-mode cursor is on>|<match count>".
    func copyMode(_ s: String) -> String {
        tmuxOut(["display", "-p", "-t", "=\(s):", "#{pane_in_mode}|#{copy_cursor_line}|#{search_count}"])
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }

    func inCopyMode(_ s: String) -> Bool { copyMode(s).hasPrefix("1|") }

    func find(_ f: Fixture) async {
        r.say("")
        r.say("# Find in a pane")
        await showRow(f.api)
        // History to search: 60 lines the fake agent echoes back, most of them scrolled past.
        for i in 1...60 {
            tmux(["send-keys", "-t", "=\(f.api):", "-l", "findme-\(i)"])
            tmux(["send-keys", "-t", "=\(f.api):", "Enter"])
        }
        let echoed = await paneShows(f.api, "got: findme-60", timeout: 5)
        r.check("find: the fixture's api pane has 60 lines of history to search", echoed)

        // Cmd+F: the bar on the active pane, with the keyboard; not the sidebar's filter.
        await cmd("f")
        await waitUntil(2) { app.focus.exception == .find }
        r.check("find: Cmd+F opens the find bar on the active pane and gives it the keyboard (the find exception)",
                app.focus.exception == .find && findFieldHasKeyboard && findBar != nil && app.current.findSession == f.api,
                "fr=\(String(describing: w.firstResponder.map { Swift.type(of: $0) }))")
        r.check("…saying which session it searches", findBar?.placeholder == "Find in \(app.displayName(f.api))", findBar?.placeholder ?? "no bar")
        r.check("…and the sidebar's filter stays closed", app.sidebar.view.filterBar.isHidden && app.sidebar.filtered == nil)
        if let bar = findBar, let term = ws.terminal(in: f.api) {
            let b = bar.convert(bar.bounds, to: nil), t = term.convert(term.bounds, to: nil)
            r.check("…in the terminal's top right corner", t.contains(b) && b.maxX == t.maxX && b.maxY == t.maxY, "\(b) in \(t)")
        }

        // A search with no match.
        await type("zzq")
        await waitUntil(2) { findBar?.count.stringValue == "No matches" }
        r.check("find: a search with no match says so", findBar?.count.stringValue == "No matches", findBar?.count.stringValue ?? "")
        r.check("…typed keys never reach the pane", !pane(f.api).contains("got: zzq"))

        // Typing searches as you go, from the bottom up.
        await key(51, "\u{7f}"); await key(51, "\u{7f}"); await key(51, "\u{7f}")
        await type("findme-4")
        await waitUntil(2) { copyMode(f.api).contains("findme-49") && findBar?.count.stringValue.hasSuffix("matches") == true }
        let first = copyMode(f.api)
        r.check("find: typing finds the newest match, in tmux's copy mode", first.hasPrefix("1|") && first.contains("findme-49"), first)
        let n = first.split(separator: "|").last.map(String.init) ?? ""
        r.check("…and the bar says how many there are, as tmux counts them", n.isEmpty ? findBar?.count.stringValue == "Found"
                : findBar?.count.stringValue == "\(n) matches", "\(findBar?.count.stringValue ?? "") vs tmux \(n)")
        shot("60-find")

        // Return: older; Shift+Return: newer; Cmd+G and Cmd+Shift+G the same.
        await type("\n")
        await waitUntil(2) { copyMode(f.api) != first }
        let older = copyMode(f.api)
        r.check("find: Return goes to the next older match", older != first && older.contains("findme-4"), older)
        await key(36, "\r", [.shift])
        await waitUntil(2) { copyMode(f.api) == first }
        r.check("…Shift+Return back to the newer one", copyMode(f.api) == first, copyMode(f.api))
        await cmd("g")
        await waitUntil(2) { copyMode(f.api) == older }
        r.check("…Cmd+G goes older too", copyMode(f.api) == older, copyMode(f.api))
        await cmd("g", .shift)
        await waitUntil(2) { copyMode(f.api) == first }
        r.check("…and Cmd+Shift+G newer", copyMode(f.api) == first, copyMode(f.api))
        r.check("…with the keyboard still in the bar", findFieldHasKeyboard && app.focus.exception == .find)

        // The theme's colours mark the matches: the accent behind the match the pane is on.
        let style = tmuxOut(["show", "-wv", "-t", "=\(f.api):", "copy-mode-current-match-style"])
        let colors = app.current.findColors
        r.check("find: matches are marked in the theme's colours (the accent behind the current one)",
                style.contains(colors.currentBg) && style.contains(colors.currentFg)
                    && tmuxOut(["show", "-wv", "-t", "=\(f.api):", "copy-mode-match-style"]).contains(colors.matchBg), style)

        // Case: ignored unless the Aa toggle is on.
        // (Select All directly: a test window running behind another app isn't key, so the menu's Cmd+A has
        // no first responder to reach.)
        (w.firstResponder as? NSTextView)?.selectAll(nil)
        await type("FINDME-4")
        await waitUntil(2) { copyMode(f.api) == first }
        r.check("find: case is ignored by default (FINDME-4 finds findme-49)", copyMode(f.api) == first && findBar?.matchesCase == false,
                "\(copyMode(f.api)) field=\(findBar?.field.stringValue ?? "-") count=\(findBar?.count.stringValue ?? "-") fr=\(findFieldHasKeyboard)")
        findBar?.matchCase.performClick(nil)
        await waitUntil(2) { findBar?.count.stringValue == "No matches" }
        r.check("…the Aa toggle matches case: FINDME-4 finds nothing", findBar?.matchesCase == true && app.current.findMatchCase
                && findBar?.count.stringValue == "No matches", findBar?.count.stringValue ?? "")
        r.check("…and the field keeps the keyboard", findFieldHasKeyboard && app.focus.exception == .find)
        (w.firstResponder as? NSTextView)?.selectAll(nil)
        await type("findme-4")
        await waitUntil(2) { copyMode(f.api) == first }
        r.check("…while findme-4 still finds findme-49", copyMode(f.api) == first, copyMode(f.api))
        findBar?.matchCase.performClick(nil)
        await waitUntil(2) { findBar?.matchesCase == false }

        // Esc: the bar closes, the pane is back at its live output and the terminal has the keyboard.
        await key(53, "\u{1b}")
        await waitUntil(2) { app.focus.exception == nil && !inCopyMode(f.api) }
        r.check("find: Esc closes the bar, leaves copy mode and hands the keyboard back to the terminal",
                findBar == nil && app.focus.exception == nil && !inCopyMode(f.api) && firstResponderIsActiveTerminal, copyMode(f.api))
        await type("after-find\n")
        r.check("…typing reaches the pane again", await paneShows(f.api, "got: after-find"))

        // Cmd+F again offers the last search, selected, and searches for it.
        await cmd("f")
        await waitUntil(2) { app.focus.exception == .find && inCopyMode(f.api) }
        let sel = (w.firstResponder as? NSTextView)?.selectedRange()
        r.check("find: the next Cmd+F offers the last search again, selected, and finds it",
                findBar?.field.stringValue == "findme-4" && sel == NSRange(location: 0, length: 8) && inCopyMode(f.api), "\(String(describing: sel)) \(copyMode(f.api))")

        // A click in the pane: the bar closes, the pane stays where the search took it.
        if let term = ws.terminal(in: f.api) {
            let p = term.convert(NSPoint(x: term.bounds.midX, y: term.bounds.midY), to: nil)
            await click(p)
        }
        await waitUntil(2) { app.focus.exception == nil }
        r.check("find: a click in the pane closes the bar and keeps the pane at the match (still in copy mode)",
                findBar == nil && app.focus.exception == nil && inCopyMode(f.api) && firstResponderIsActiveTerminal, copyMode(f.api))
        tmux(["send-keys", "-t", "=\(f.api):", "-X", "cancel"])

        // Option+Cmd+F from the bar: the sidebar's filter takes over, the pane goes back to live output.
        await cmd("f")
        await waitUntil(2) { app.focus.exception == .find && inCopyMode(f.api) }
        await cmd("f", .option)
        await waitUntil(2) { app.focus.exception == .filter && !inCopyMode(f.api) }
        r.check("find: Option+Cmd+F closes the find bar (out of copy mode) and opens the sidebar's filter",
                findBar == nil && app.focus.exception == .filter && !inCopyMode(f.api), copyMode(f.api))
        await key(53, "\u{1b}")
        await waitUntil(2) { app.focus.exception == nil }

        // Cmd+F from the filter's field keeps the filter's query and opens the find bar.
        await cmd("f", .option)
        await waitUntil(2) { app.focus.exception == .filter }
        await type("api")
        await cmd("f")
        await waitUntil(2) { app.focus.exception == .find }
        r.check("find: Cmd+F in the filter's field keeps the filter and opens the find bar",
                app.focus.exception == .find && findFieldHasKeyboard && app.sidebar.filtered?.query == "api")
        await key(53, "\u{1b}")
        await waitUntil(2) { app.focus.exception == nil }
        app.sidebar.clearFilter()

        // The palette takes the keyboard: the bar closes.
        await cmd("f")
        await waitUntil(2) { app.focus.exception == .find && inCopyMode(f.api) }
        await cmd("k")
        await waitUntil(2) { app.focus.exception == .palette }
        await waitUntil(2) { !inCopyMode(f.api) }
        r.check("find: Cmd+K opens the palette, closing the bar and leaving copy mode",
                findBar == nil && app.current.findSession == nil && app.focus.exception == .palette && !inCopyMode(f.api), copyMode(f.api))
        await key(53, "\u{1b}")
        await waitUntil(2) { app.focus.exception == nil }

        // A drop on the pane: the bar closes, the pane leaves copy mode and the drop is pasted.
        await cmd("f")
        await waitUntil(2) { app.focus.exception == .find && inCopyMode(f.api) }
        let file = NSTemporaryDirectory() + "bosun find drop.png"
        FileManager.default.createFile(atPath: file, contents: Data([0x89, 0x50, 0x4e, 0x47]))
        let pb = NSPasteboard(name: NSPasteboard.Name("dev.yetidevworks.bosun.test-find-drag"))
        pb.clearContents()
        pb.writeObjects([URL(fileURLWithPath: file) as NSURL])
        if let c = tileCenter(f.api) {
            let info = FakeDrag(location: c, pasteboard: pb, window: w)
            _ = ws.area.draggingEntered(info)
            _ = ws.area.performDragOperation(info)
        }
        await waitUntil(2) { app.focus.exception == nil && !inCopyMode(f.api) }
        r.check("find: a drop on the pane closes the bar and leaves copy mode",
                findBar == nil && app.current.findSession == nil && app.focus.exception == nil && !inCopyMode(f.api)
                    && firstResponderIsActiveTerminal, copyMode(f.api))
        await sleep(300)
        await type("\n")
        r.check("…and the dropped path reaches the pane", await paneShows(f.api, "got: \(ShellEscape.escape(file))"))

        // Another row: the bar closes, and the pane it was on isn't left in copy mode.
        await cmd("f")
        await waitUntil(2) { app.focus.exception == .find && inCopyMode(f.api) }
        await showRow(f.docs)
        await waitUntil(2) { app.focus.exception == nil && !inCopyMode(f.api) }
        r.check("find: showing another row closes the bar and takes its pane out of copy mode",
                app.current.findSession == nil && app.focus.exception == nil && !inCopyMode(f.api) && firstResponderIsActiveTerminal, copyMode(f.api))
        await showRow(f.api)
        r.check("…and coming back shows no bar", findBar == nil)
    }
}
