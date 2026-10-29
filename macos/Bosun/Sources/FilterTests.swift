import AppKit
import BosunCore

// Focus-test section for the sidebar filter (Option+Cmd+F) and the header (design 7): the field borrows the
// keyboard like rename and the palette do, typed keys never reach the pane, Return keeps the filter as
// a chip and hands the keyboard back, Esc clears it, a click elsewhere keeps it, the row on screen is
// never hidden, Cmd+1…9 and Shift+Up/Down follow what's listed, and each window has its own filter.

extension FocusTest {
    var filterBar: SidebarFilterBar { app.sidebar.view.filterBar }

    /// The filter's field editor has the keyboard.
    var filterFieldHasKeyboard: Bool { (w.firstResponder as? NSTextView)?.delegate === filterBar.field }

    /// The rows `bw`'s outline lists (container ids), in order.
    func listedRows(_ bw: BosunWindow? = nil) -> [String] {
        let o = (bw ?? app.current).sidebar.outline
        return (0..<o.numberOfRows).compactMap { o.item(atRow: $0) as? SidebarItem }.compactMap { if case .row(let id) = $0.kind { return id }; return nil }
    }

    /// The outline lists the "On screen" heading.
    func listsOnScreenHeading(_ bw: BosunWindow? = nil) -> Bool {
        let o = (bw ?? app.current).sidebar.outline
        return (0..<o.numberOfRows).contains { (o.item(atRow: $0) as? SidebarItem)?.kind == .heading("on-screen") }
    }

    func label(_ ids: [String]) -> String {
        ids.compactMap { app.daemon.state.sidebar.container(id: $0) }.map(app.rowLabel).joined(separator: ", ")
    }

    func filter(_ f: Fixture) async {
        r.say("")
        r.say("# Sidebar filter")
        guard let apiRow = rowID(f.api), let r1Row = rowID(f.review1), let r2Row = rowID(f.review2), let parserRow = rowID(f.parser),
              let reviews = app.daemon.state.sidebar.sections.first(where: { $0.name == "Reviews" })?.id else {
            r.check("filter: the fixture's rows", false)
            return
        }
        await showRow(f.api)
        let allRows = app.daemon.state.sidebar.allRows.count
        r.check("filter: nothing in the sidebar until Option+Cmd+F (no field, no filter)", filterBar.isHidden && app.sidebar.filtered == nil)

        // A collapsed section with a match shows expanded for the filter, without changing its saved state.
        app.setSectionCollapsed(reviews, true)
        await waitUntil(3) { app.daemon.state.sidebar.sections.first { $0.id == reviews }?.collapsed == true }
        await sleep(100)

        // Option+Cmd+F: the field, with the keyboard.
        await cmd("f", .option)
        await waitUntil(2) { app.focus.exception == .filter }
        r.check("filter: Option+Cmd+F opens the field and gives it the keyboard (the filter exception)",
                app.focus.exception == .filter && filterFieldHasKeyboard && !filterBar.isHidden, "fr=\(String(describing: w.firstResponder.map { Swift.type(of: $0) }))")
        r.check("…with the accent ring", filterBar.editing && filterBar.layer?.borderWidth == 2)
        r.check("…and the terminal isn't first responder while it does", !firstResponderIsActiveTerminal)
        await type("review-")
        await sleep(200)
        r.check("filter: every key narrows the list live (review-1, review-2, then api on screen)",
                listedRows() == [r1Row, r2Row, apiRow] && app.sidebar.filtered?.onScreen == apiRow && listsOnScreenHeading(), label(listedRows()))
        r.check("…typed keys never reach the pane", !pane(f.api).contains("review-"))
        r.check("…a collapsed section with a match shows expanded", app.sidebar.outline.isItemExpanded(sectionItem("Reviews")))
        r.check("…without changing its saved collapsed state", app.daemon.state.sidebar.sections.first { $0.id == reviews }?.collapsed == true)
        r.check("…matched characters are underlined in the accent colour", {
            guard let item = rowItem(f.review1) else { return false }
            let row = app.sidebar.outline.row(forItem: item)
            guard let cell = app.sidebar.outline.view(atColumn: 0, row: row, makeIfNecessary: true) as? SidebarCellView else { return false }
            let a = cell.nameField.attributedStringValue
            guard a.length > 0 else { return false }
            return a.attribute(.underlineStyle, at: 0, effectiveRange: nil) != nil && a.attribute(.underlineColor, at: 0, effectiveRange: nil) as? NSColor == app.theme.accent
        }())
        let onScreenTrailing = trailing(app.current, row: apiRow)
        r.check("…the row on screen is listed under On screen, saying so", onScreenTrailing == "on screen", onScreenTrailing ?? "no cell")
        shot("50-filter-typing")

        // Return: the chip, the keyboard back in the terminal.
        await type("\n")
        await waitUntil(2) { app.focus.exception == nil }
        r.check("filter: Return keeps the filter and hands the keyboard back to the active terminal",
                app.focus.exception == nil && firstResponderIsActiveTerminal && app.sidebar.filtered?.query == "review-")
        r.check("…the field is a chip with no ring, saying 2 of \(allRows)", !filterBar.isHidden && !filterBar.editing && filterBar.layer?.borderWidth == 0
                && filterBar.count.stringValue == "2 of \(allRows)", filterBar.count.stringValue)
        r.check("…the magnifier shows it's applied, and the footer that Cmd+1…9 follow it",
                app.sidebar.view.header.filterApplied && !app.sidebar.view.footer.hint.isHidden)
        await type("after-filter\n")
        r.check("…typing reaches the pane", await paneShows(f.api, "got: after-filter"))
        shot("51-filter-applied")

        // Cmd+1…9 number the rows you can see.
        await cmd("2")
        await waitUntil(2) { ws.currentRow == r2Row }
        r.check("filter: Cmd+2 shows the second listed row (review-2)", ws.currentRow == r2Row && firstResponderIsActiveTerminal)
        r.check("…api, no longer on screen and not matching, drops out of the list", listedRows() == [r1Row, r2Row] && !listsOnScreenHeading(),
                label(listedRows()))
        await cmd("3")
        r.check("…Cmd+3 has no third row to go to", ws.currentRow == r2Row)
        await cmd("1")
        await waitUntil(2) { ws.currentRow == r1Row }
        r.check("…Cmd+1 the first (review-1)", ws.currentRow == r1Row)

        // Shift+Up/Down walk the listed rows only.
        var stepped: [String] = []
        for _ in 0..<3 {
            await arrow(125, [.shift])
            await sleep(60)
            stepped.append(ws.currentRow ?? "-")
        }
        r.check("filter: Shift+Down steps through the listed rows only", stepped == [r2Row, r1Row, r2Row], label(stepped))

        // Option+Cmd+F again: the field, with the query selected; a click elsewhere keeps it.
        await cmd("f", .option)
        await waitUntil(2) { app.focus.exception == .filter }
        let sel = (w.firstResponder as? NSTextView)?.selectedRange()
        r.check("filter: Option+Cmd+F on the chip gives the field the keyboard again, the query selected",
                app.focus.exception == .filter && filterFieldHasKeyboard && sel == NSRange(location: 0, length: 7), "\(String(describing: sel))")
        if let p = sidebarRowPoint(r1Row) { await click(p) }
        await waitUntil(2) { app.focus.exception == nil && ws.currentRow == r1Row }
        r.check("filter: a click on a row keeps the filter, selects the row and hands the keyboard back",
                app.focus.exception == nil && ws.currentRow == r1Row && firstResponderIsActiveTerminal && app.sidebar.filtered?.query == "review-")

        // A jump to a row the filter hides (a notification click): it shows under On screen, the filter stays.
        app.notificationClicked(userInfo: ["session": f.parser])
        await waitUntil(3) { ws.currentRow == parserRow }
        await sleep(150)
        r.check("filter: a notification click onto a hidden row shows it, listed last under On screen",
                ws.currentRow == parserRow && listedRows() == [r1Row, r2Row, parserRow] && listsOnScreenHeading() && app.sidebar.filtered?.onScreen == parserRow,
                label(listedRows()))
        r.check("…and the filter is still applied", app.sidebar.filtered?.query == "review-" && !filterBar.isHidden)
        r.check("…the keyboard is in its pane", firstResponderIsActiveTerminal && ws.activeSession == f.parser)
        shot("52-filter-on-screen")

        // Cmd+K from the field keeps the filter and opens the palette.
        await cmd("f", .option)
        await waitUntil(2) { app.focus.exception == .filter }
        await cmd("k")
        await waitUntil(2) { app.paletteOpen }
        r.check("filter: Cmd+K from the field keeps the filter and opens the palette",
                app.paletteOpen && app.focus.exception == .palette && !filterBar.editing && app.sidebar.filtered?.query == "review-")
        await key(53, "\u{1b}")
        await waitUntil(2) { !app.paletteOpen }
        r.check("…Esc closes the palette, the keyboard back in the terminal", !app.paletteOpen && firstResponderIsActiveTerminal)

        // A rename while the chip shows: allowed, and it never touches the filter.
        app.sidebar.beginRename(session: f.parser)
        await waitUntil(2) { app.focus.exception == .rename }
        r.check("filter: a rename can start while the chip shows", app.focus.exception == .rename && app.sidebar.filtered?.query == "review-")
        await key(53, "\u{1b}")
        await waitUntil(2) { app.focus.exception == nil }
        r.check("…and Esc ends it with the filter still applied", app.focus.exception == nil && app.sidebar.filtered?.query == "review-" && firstResponderIsActiveTerminal)

        // No matches: says so, offers Clear, and the row on screen is still listed.
        await cmd("f", .option)
        await waitUntil(2) { app.focus.exception == .filter }
        await type("zzq")
        await sleep(150)
        r.check("filter: no matches says so, with a Clear link", app.sidebar.filtered?.isEmpty == true && !app.sidebar.view.emptyFilter.isHidden
                && app.sidebar.view.emptyFilter.label.stringValue.contains("zzq"))
        r.check("…and the row on screen is still listed", listedRows() == [parserRow] && listsOnScreenHeading(), label(listedRows()))
        r.check("…the message doesn't cover the row", {
            guard let item = rowItem(f.parser) else { return false }
            let o = app.sidebar.outline
            let rr = o.convert(o.rect(ofRow: o.row(forItem: item)), to: app.sidebar.view)
            return rr.minY >= app.sidebar.view.emptyFilter.frame.maxY - 1
        }())
        shot("53-filter-empty")

        // Esc clears it.
        await key(53, "\u{1b}")
        await waitUntil(2) { app.focus.exception == nil }
        r.check("filter: Esc clears the filter, closes the field and hands the keyboard back",
                app.sidebar.filtered == nil && filterBar.isHidden && app.focus.exception == nil && firstResponderIsActiveTerminal)
        r.check("…every row is back, and the collapsed section is collapsed again", listedRows() == app.daemon.state.sidebar.visibleRows.map(\.id)
                && !app.sidebar.outline.isItemExpanded(sectionItem("Reviews")), label(listedRows()))
        r.check("…the magnifier and the footer say nothing's applied", !app.sidebar.view.header.filterApplied && app.sidebar.view.footer.hint.isHidden)
        app.setSectionCollapsed(reviews, false)
        await waitUntil(3) { app.daemon.state.sidebar.sections.first { $0.id == reviews }?.collapsed == false }

        // Option+Cmd+F on an empty field closes it; Return on an empty field too.
        await cmd("f", .option)
        await waitUntil(2) { app.focus.exception == .filter }
        await cmd("f", .option)
        await waitUntil(2) { app.focus.exception == nil }
        r.check("filter: Option+Cmd+F with the field empty closes it", filterBar.isHidden && app.focus.exception == nil && firstResponderIsActiveTerminal)
        await cmd("f", .option)
        await waitUntil(2) { app.focus.exception == .filter }
        await type("\n")
        await waitUntil(2) { app.focus.exception == nil }
        r.check("…and so does Return", filterBar.isHidden && app.sidebar.filtered == nil && firstResponderIsActiveTerminal)

        // The magnifier opens it for the mouse, without a ring of its own.
        let mag = app.sidebar.view.header.magnifier
        await click(mag.convert(NSPoint(x: mag.bounds.midX, y: mag.bounds.midY), to: nil))
        await waitUntil(2) { app.focus.exception == .filter }
        r.check("filter: the header's magnifier opens the field too", app.focus.exception == .filter && filterFieldHasKeyboard)
        await type("review-\n")
        await waitUntil(2) { app.focus.exception == nil }

        // Each window has its own filter.
        await showRow(f.api)
        let first = app.current!
        await cmd("n")
        await waitUntil(3) { app.windows.count == 2 }
        if app.windows.count == 2 {
            let second = app.windows[1]
            await waitUntil(3) { ws.activeTerminal?.hasPaintedContent == true }
            r.check("filter: a second window starts with no filter of its own", second.sidebar.filtered == nil && second.sidebar.view.filterBar.isHidden
                    && first.sidebar.filtered?.query == "review-")
            // The first window's row in the second's sidebar: ⧉ with a tooltip that says what a click does.
            let apiTip = cellTrailingTip(second, row: apiRow)
            r.check("the ⧉ mark has a tooltip saying what a click does", apiTip?.contains("click to bring it forward") == true, apiTip ?? "no tooltip")
            await cmd("f", .option)
            await waitUntil(2) { second.focus.exception == .filter }
            await type("doc", window: second.window)
            await type("\n", window: second.window)
            await waitUntil(2) { second.focus.exception == nil }
            r.check("…its own filter applies to its own sidebar only", second.sidebar.filtered?.query == "doc" && first.sidebar.filtered?.query == "review-"
                    && first.focus.exception == nil, "second \(second.sidebar.filtered?.query ?? "none"), first \(first.sidebar.filtered?.query ?? "none")")
            r.check("…with the keyboard back in the second window's terminal", firstResponderIsActiveTerminal && w === second.window)
            second.window.performClose(nil)
            await waitUntil(3) { app.windows.count == 1 }
        } else {
            r.check("filter: Cmd+N opens a second window", false)
        }
        r.check("…the first window keeps its filter", app.current === first && first.sidebar.filtered?.query == "review-")

        // The chip's clear button.
        let clear = filterBar.clear
        await click(clear.convert(NSPoint(x: clear.bounds.midX, y: clear.bounds.midY), to: nil))
        await waitUntil(2) { app.sidebar.filtered == nil }
        r.check("filter: the chip's clear button clears it, the keyboard still in the terminal",
                app.sidebar.filtered == nil && filterBar.isHidden && firstResponderIsActiveTerminal)

        // Hover: the row under the pointer is filled lightly; the section chevron has a tooltip.
        // A posted move never passes through the outline's tracking area, so it's handed to the outline as one would.
        func hover(_ p: NSPoint?) async {
            guard let p, let e = mouse(.mouseMoved, p) else { return }
            app.sidebar.outline.mouseMoved(with: e)
            await sleep(30)
        }
        await hover(sidebarRowPoint(r1Row))
        r.check("the row under the pointer is the hovered one (a light fill)", app.sidebar.hoveredItem?.kind == .row(r1Row))
        shot("54-hover-and-selected-row")
        await hover(sectionPoint(reviews))
        r.check("…a section header never is", app.sidebar.hoveredItem == nil)
        app.sidebar.hovered(row: -1)
        r.check("the section chevron has a tooltip", chevronTip(reviews)?.contains("section") == true, chevronTip(reviews) ?? "no chevron")
        await move(NSPoint(x: w.frame.width - 40, y: 200))
        ws.focusActiveTerminal()
        r.check("filter: the keyboard ends in the active terminal", firstResponderIsActiveTerminal)
    }

    func cellTrailingTip(_ bw: BosunWindow, row id: String) -> String? {
        let o = bw.sidebar.outline
        for i in 0..<o.numberOfRows {
            guard let it = o.item(atRow: i) as? SidebarItem, it.kind == .row(id) else { continue }
            return (o.view(atColumn: 0, row: i, makeIfNecessary: true) as? SidebarCellView)?.trailingTip
        }
        return nil
    }

    func chevronTip(_ section: String) -> String? {
        let o = app.sidebar.outline
        for i in 0..<o.numberOfRows {
            guard let it = o.item(atRow: i) as? SidebarItem, it.kind == .section(section),
                  let rv = o.rowView(atRow: i, makeIfNecessary: true) else { continue }
            func find(_ v: NSView) -> String? {
                if let b = v as? NSButton, b.identifier == NSOutlineView.disclosureButtonIdentifier { return b.toolTip }
                for s in v.subviews { if let t = find(s) { return t } }
                return nil
            }
            return find(rv)
        }
        return nil
    }

    // MARK: Header

    /// The masthead: beside the traffic lights and never under them, the wordmark gone in a narrow
    /// sidebar, the magnifier taking its own clicks and the rest of it dragging the window.
    func headerLayout() async {
        r.say("")
        r.say("# Sidebar header")
        let root = app.current.root
        let header = app.sidebar.view.header
        let original = root.sidebarWidth
        func check(_ width: CGFloat) -> [String] {
            root.sidebarWidth = width
            root.layoutSubtreeIfNeeded()
            header.layoutSubtreeIfNeeded()
            var bad: [String] = []
            guard let lights = header.trafficLights else { return ["no traffic lights"] }
            let tile = header.tile.frame
            if tile.minX < lights.maxX + 8 { bad.append("tile at x \(Int(tile.minX)) under the traffic lights (to \(Int(lights.maxX)))") }
            if abs(tile.midY - lights.midY) > 1.5 { bad.append("tile centre y \(tile.midY) not level with the lights' \(lights.midY)") }
            let mag = header.magnifier.frame
            if mag.maxX > header.bounds.width - 4 || mag.minX < tile.maxX + 4 { bad.append("magnifier \(mag) collides with the tile or the edge") }
            if !header.wordmark.isHidden, header.wordmark.frame.maxX > mag.minX - 2 { bad.append("wordmark runs into the magnifier") }
            if header.status.frame.minY < lights.maxY { bad.append("status line overlaps the traffic lights") }
            if header.status.frame.maxY > header.bounds.height { bad.append("status line below the header") }
            return bad
        }
        let wide = check(248)
        r.check("header at 248 pt: tile and wordmark beside the traffic lights, nothing overlaps", wide.isEmpty && !header.wordmark.isHidden, wide.joined(separator: "; "))
        let narrow = check(180)
        r.check("header at 180 pt: the wordmark hides, the tile and magnifier still clear the lights", narrow.isEmpty && header.wordmark.isHidden,
                narrow.joined(separator: "; "))
        shot("55-header-narrow")
        root.sidebarWidth = original
        root.layoutSubtreeIfNeeded()
        let magCentre = header.convert(NSPoint(x: header.magnifier.frame.midX, y: header.magnifier.frame.midY), to: header.superview)
        let empty = header.convert(NSPoint(x: header.bounds.width / 2, y: SidebarHeader.titleRowHeight + 4), to: header.superview)
        r.check("header: the magnifier takes its own click, the empty area drags the window",
                header.hitTest(magCentre) === header.magnifier && header.hitTest(empty) === header && header.mouseDownCanMoveWindow)
        r.check("header: the magnifier never takes the keyboard", header.magnifier.refusesFirstResponder && !header.magnifier.acceptsFirstResponder)
        let text = header.status.stringValue
        let live = app.daemon.state.sidebar.allRows.filter { c in c.tabs.contains { app.isAlive($0.session) } }.count
        r.check("header: the status line counts the sessions (\(text))", text.hasPrefix("\(live) session") && text.hasSuffix("· dev"))
    }

    /// The header follows the theme: the tile is the accent with ink by the luminance rule, the gradient
    /// runs accent to done, softer on a light theme.
    func headerShowsTheme() -> (ok: Bool, detail: String) {
        let h = app.sidebar.view.header
        func hex(_ c: CGColor?) -> String {
            guard let c, let s = NSColor(cgColor: c)?.usingColorSpace(.sRGB) else { return "-" }
            return RGB(r: Double(s.redComponent), g: Double(s.greenComponent), b: Double(s.blueComponent)).hex
        }
        let p = app.theme.palette
        let ink = RGB(hex: p.accent)?.ink.hex ?? "?"
        let colors = (h.layer?.sublayers?.first { $0 is CAGradientLayer } as? CAGradientLayer)?.colors as? [CGColor] ?? []
        let alphas = SidebarHeader.gradientAlphas(dark: app.theme.isDark)
        let ok = hex(h.tile.backgroundColor) == p.accent.lowercased() && hex(h.anchor.strokeColor) == ink
            && colors.count == 3 && hex(colors[0]) == p.accent.lowercased() && abs((colors[0].alpha) - alphas.accent) < 0.01
            && hex(colors[1]) == p.statusDone.lowercased() && h.wordmark.textColor == app.theme.text
        return (ok, "\(p.name): tile \(hex(h.tile.backgroundColor)) ink \(hex(h.anchor.strokeColor)) (want \(ink)) gradient \(colors.map { hex($0) + String(format: "@%.2f", $0.alpha) })")
    }
}
