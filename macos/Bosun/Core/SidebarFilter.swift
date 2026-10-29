import Foundation

/// The sidebar filter (Option+Cmd+F): which rows a query keeps, and where the matched characters of their
/// names are. Names (the row's and its tabs') match as the palette does, by subsequence; the agent, the
/// last two components of the directory and the section name need the query as a whole, since a
/// subsequence of a long path matches almost anything; and a query of two letters or more that
/// starts a state word (waiting, done, running, idle, error, exited) keeps the rows in that state.
/// No views: the sidebar builds its outline from a `FilteredSidebar`, and Cmd+1…9 and previous/next
/// row number and walk its `visibleRows`, so what you see and what the keys reach are worked out once.
public enum SidebarFilter {
    /// What the filter looks at for one tab: the name the sidebar shows and the session's details.
    public struct TabInfo: Equatable, Sendable {
        public var name: String
        public var path: String?
        public var agent: String?
        public var state: AgentState
        public var alive: Bool

        public init(name: String, path: String? = nil, agent: String? = nil, state: AgentState = .idle, alive: Bool = true) {
            self.name = name
            self.path = path
            self.agent = agent
            self.state = state
            self.alive = alive
        }
    }

    /// The words that match a state rather than a name ("wait" narrows to the rows that need you).
    static func stateWords(_ t: TabInfo) -> [String] {
        guard t.alive else { return ["exited"] }
        switch t.state {
        case .waiting: return ["waiting"]
        case .done: return ["done"]
        case .seen: return ["done", "read"]
        case .running: return ["running"]
        case .error: return ["error"]
        case .idle, .unknown: return ["idle"]
        }
    }

    /// The last two components of a directory ("~/Projects/bosun/src" → "bosun/src").
    static func pathTail(_ path: String?) -> String? {
        guard let path, !path.isEmpty else { return nil }
        let parts = path.split(separator: "/").map(String.init).filter { !$0.isEmpty }
        return parts.suffix(2).joined(separator: "/")
    }

    /// The characters of `text` that `query` matches, as offsets in `text`'s Characters: the first
    /// case-insensitive run if the query appears whole, else the palette's subsequence. nil when it
    /// doesn't match.
    public static func matchOffsets(_ query: String, in text: String) -> [Int]? {
        let q = Array(query.lowercased())
        guard !q.isEmpty else { return [] }
        let t = Array(text.lowercased())
        if t.count >= q.count {
            for start in 0...(t.count - q.count) where Array(t[start..<(start + q.count)]) == q {
                return Array(start..<(start + q.count))
            }
        }
        guard Fuzzy.score(query, text) != nil else { return nil }
        var out: [Int] = []
        var qi = 0
        for (i, c) in t.enumerated() where qi < q.count && c == q[qi] {
            out.append(i)
            qi += 1
        }
        return qi == q.count ? out : nil
    }

    /// Whether a query (trimmed) filters anything.
    public static func normalized(_ query: String) -> String? {
        let q = query.trimmingCharacters(in: .whitespacesAndNewlines)
        return q.isEmpty ? nil : q
    }

    /// Apply `query` to the sidebar. `label` is the name a row shows, `info` a tab's details (by session),
    /// and `current` the row on screen in this window, which is never hidden: when it doesn't match it is
    /// listed on its own at the end (`onScreen`). nil when the query is blank (no filter).
    public static func apply(_ query: String, sidebar: Sidebar, current: String?,
                             label: (Container) -> String, info: (String) -> TabInfo) -> FilteredSidebar? {
        guard let q = normalized(query) else { return nil }
        let ql = q.lowercased()
        var names: [String: [Int]] = [:]
        var tabNames: [String: [Int]] = [:]

        func matches(_ c: Container, section: String?) -> Bool {
            var hit = false
            if let m = matchOffsets(q, in: label(c)) {
                names[c.id] = m
                hit = true
            }
            let tabs = c.tabs.map { info($0.session) }
            // Every tab's name, so each matching tab of a multi-tab row is underlined. A one-tab row's
            // name is its tab's: the row's own underline says it.
            for (t, i) in zip(c.tabs, tabs) {
                guard let m = matchOffsets(q, in: i.name) else { continue }
                if c.tabs.count > 1 { tabNames[t.session] = m }
                hit = true
            }
            for i in tabs where !hit {
                if let a = i.agent, a != "terminal", a.lowercased().contains(ql) { hit = true }
                if let p = pathTail(i.path), p.lowercased().contains(ql) { hit = true }
                if ql.count >= 2, stateWords(i).contains(where: { $0.hasPrefix(ql) }) { hit = true }
            }
            if !hit, let section, section.lowercased().contains(ql) { hit = true }
            return hit
        }

        var out = FilteredSidebar(query: q)
        out.total = sidebar.allRows.count
        for c in sidebar.ungrouped where matches(c, section: nil) { out.ungrouped.append(c.id) }
        for s in sidebar.sections {
            let rows = s.members.filter { matches($0, section: s.name) }.map(\.id)
            if !rows.isEmpty { out.sections.append(.init(id: s.id, rows: rows)) }
        }
        out.nameMatches = names
        out.tabMatches = tabNames
        if let current, sidebar.container(id: current) != nil, !out.matchingRows.contains(current) { out.onScreen = current }
        return out
    }
}

/// What the sidebar shows with a filter applied.
public struct FilteredSidebar: Equatable, Sendable {
    public struct Section: Equatable, Sendable {
        public var id: String
        public var rows: [String]
        public init(id: String, rows: [String]) {
            self.id = id
            self.rows = rows
        }
    }

    public var query: String
    /// Matching ungrouped rows, in sidebar order.
    public var ungrouped: [String] = []
    /// Sections with at least one matching row (shown expanded, whatever their saved state), with those rows.
    public var sections: [Section] = []
    /// The row on screen when it doesn't match: listed last, under "On screen".
    public var onScreen: String?
    /// Matched characters of row names (by row id) and of tab names in multi-tab rows (by session).
    public var nameMatches: [String: [Int]] = [:]
    public var tabMatches: [String: [Int]] = [:]
    /// Every row in the sidebar, for "2 of 8".
    public var total = 0

    public init(query: String) { self.query = query }

    /// The matching rows, in sidebar order (the on-screen extra not included).
    public var matchingRows: [String] { ungrouped + sections.flatMap(\.rows) }

    /// Nothing matches (the on-screen row may still be listed).
    public var isEmpty: Bool { ungrouped.isEmpty && sections.isEmpty }

    /// The rows you can see, in the order shown: what Cmd+1…9 number and previous/next row walk.
    public var visibleRows: [String] { matchingRows + (onScreen.map { [$0] } ?? []) }

    /// "2 of 8".
    public var countLabel: String { "\(matchingRows.count) of \(total)" }
}

extension Sidebar {
    /// The rows a person can see, in order: with a filter, its rows (the on-screen one last); without,
    /// every row not inside a collapsed section.
    public func visibleRows(filter: FilteredSidebar?) -> [Container] {
        guard let filter else { return visibleRows }
        return filter.visibleRows.compactMap { container(id: $0) }
    }
}
