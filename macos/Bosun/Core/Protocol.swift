import Foundation

// Wire types for the `bosun serve` socket protocol, version 1 (docs/protocol.md).
// Unknown fields are ignored by Codable; unknown message kinds decode to `.unknown`.

public let protocolVersion: UInt32 = 1

// MARK: - JSON value

public enum JSONValue: Codable, Equatable, Sendable {
    case null
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    public init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let b = try? c.decode(Bool.self) { self = .bool(b) }
        else if let n = try? c.decode(Double.self) { self = .number(n) }
        else if let s = try? c.decode(String.self) { self = .string(s) }
        else if let a = try? c.decode([JSONValue].self) { self = .array(a) }
        else { self = .object(try c.decode([String: JSONValue].self)) }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let b): try c.encode(b)
        case .number(let n): try c.encode(n)
        case .string(let s): try c.encode(s)
        case .array(let a): try c.encode(a)
        case .object(let o): try c.encode(o)
        }
    }

    public var boolValue: Bool? { if case .bool(let b) = self { return b }; return nil }
    public var stringValue: String? { if case .string(let s) = self { return s }; return nil }
}

// MARK: - Snapshot parts

public struct ServerInfo: Codable, Equatable, Sendable {
    public var version: String
    public var protocolMajor: UInt32
    public var tmuxSocket: String?

    enum CodingKeys: String, CodingKey {
        case version
        case protocolMajor = "protocol"
        case tmuxSocket = "tmux_socket"
    }

    public init(version: String, protocolMajor: UInt32, tmuxSocket: String?) {
        self.version = version
        self.protocolMajor = protocolMajor
        self.tmuxSocket = tmuxSocket
    }
}

public struct SessionInfo: Codable, Equatable, Sendable {
    public var name: String
    public var display: String
    public var agent: String?
    public var path: String?
    public var windows: UInt32
    public var attached: Bool
    public var created: Int64?
    public var containerID: String?
    public var worktreePath: String?
    public var branch: String?
    public var paneTitle: String?
    public var paneCommand: String?

    enum CodingKeys: String, CodingKey {
        case name, display, agent, path, windows, attached, created, branch
        case containerID = "container_id"
        case worktreePath = "worktree_path"
        case paneTitle = "pane_title"
        case paneCommand = "pane_command"
    }

    public init(name: String, display: String, agent: String? = nil, path: String? = nil, windows: UInt32 = 1,
                attached: Bool = false, created: Int64? = nil, containerID: String? = nil, worktreePath: String? = nil,
                branch: String? = nil, paneTitle: String? = nil, paneCommand: String? = nil) {
        self.name = name; self.display = display; self.agent = agent; self.path = path; self.windows = windows
        self.attached = attached; self.created = created; self.containerID = containerID
        self.worktreePath = worktreePath; self.branch = branch; self.paneTitle = paneTitle; self.paneCommand = paneCommand
    }
}

public enum AgentState: String, Codable, Equatable, Sendable, CaseIterable {
    case running, waiting, done, idle, error, unknown
    /// A done turn you've read (`SeenTracker`): drawn, never sent by the daemon.
    case seen

    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = AgentState(rawValue: raw) ?? .unknown
    }
}

public struct StatusInfo: Codable, Equatable, Sendable {
    public var state: AgentState
    public var source: String
    public var detail: String?
    /// When the agent's hook reported that it finished its turn, Unix ms (hook states only; `state` is idle then).
    public var doneAt: UInt64?
    /// When the current turn started, Unix ms: sent while running or waiting (through permission waits).
    public var startedAt: UInt64?

    enum CodingKeys: String, CodingKey {
        case state, source, detail
        case doneAt = "done_at"
        case startedAt = "started_at"
    }

    public init(state: AgentState, source: String = "detector", detail: String? = nil, doneAt: UInt64? = nil, startedAt: UInt64? = nil) {
        self.state = state
        self.source = source
        self.detail = detail
        self.doneAt = doneAt
        self.startedAt = startedAt
    }

    public var isHook: Bool { source == "hook" }
}

public struct Tab: Codable, Equatable, Sendable {
    public var session: String
    public var display: String
    public var alive: Bool
    /// For an exited tab: the recents entry it was created from, to restart it from.
    public var recent: Int64?

    public init(session: String, display: String = "", alive: Bool = true, recent: Int64? = nil) {
        self.session = session
        self.display = display
        self.alive = alive
        self.recent = recent
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        session = try c.decode(String.self, forKey: .session)
        display = try c.decodeIfPresent(String.self, forKey: .display) ?? ""
        alive = try c.decodeIfPresent(Bool.self, forKey: .alive) ?? false
        recent = try c.decodeIfPresent(Int64.self, forKey: .recent)
    }
}

public struct Container: Codable, Equatable, Sendable {
    public var id: String
    public var name: String
    public var active: String
    public var tabs: [Tab]
    /// Set while the row is in the Snoozed or Docked section.
    public var parked: Parked?

    public init(id: String, name: String, active: String, tabs: [Tab], parked: Parked? = nil) {
        self.id = id
        self.name = name
        self.active = active
        self.tabs = tabs
        self.parked = parked
    }
}

/// The built-in sections rows are parked in: snoozed rows keep running and come back by themselves; docked
/// rows' sessions are killed on the way in and resumed on the way out.
public enum SectionRole: String, Codable, Sendable {
    case snoozed, docked
}

/// Where a parked row came from, and when a snoozed one wakes.
public struct Parked: Codable, Equatable, Sendable {
    /// When it was parked, Unix ms.
    public var at: UInt64
    public var fromSection: String?
    public var fromIndex: Int
    /// Snoozed: when it wakes, Unix ms.
    public var wakeAt: UInt64?
    /// Snoozed: it wakes when one of its agents needs you.
    public var untilNeeded: Bool

    enum CodingKeys: String, CodingKey {
        case at
        case fromSection = "from_section", fromIndex = "from_index", wakeAt = "wake_at", untilNeeded = "until_needed"
    }

    public init(at: UInt64 = 0, fromSection: String? = nil, fromIndex: Int = 0, wakeAt: UInt64? = nil, untilNeeded: Bool = false) {
        self.at = at; self.fromSection = fromSection; self.fromIndex = fromIndex; self.wakeAt = wakeAt; self.untilNeeded = untilNeeded
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        at = try c.decodeIfPresent(UInt64.self, forKey: .at) ?? 0
        fromSection = try c.decodeIfPresent(String.self, forKey: .fromSection)
        fromIndex = try c.decodeIfPresent(Int.self, forKey: .fromIndex) ?? 0
        wakeAt = try c.decodeIfPresent(UInt64.self, forKey: .wakeAt)
        untilNeeded = try c.decodeIfPresent(Bool.self, forKey: .untilNeeded) ?? false
    }
}

public struct Section: Codable, Equatable, Sendable {
    public var id: String
    public var name: String
    public var collapsed: Bool
    public var bannerFont: String?
    /// What a new session made in this section starts with (`set_section_defaults`).
    public var defaults: SectionDefaults?
    /// The built-in Snoozed or Docked section; nil for the user's own.
    public var role: SectionRole?
    public var members: [Container]

    enum CodingKeys: String, CodingKey {
        case id, name, collapsed, members, defaults, role
        case bannerFont = "banner_font"
    }

    public init(id: String, name: String, collapsed: Bool = false, bannerFont: String? = nil, defaults: SectionDefaults? = nil,
                members: [Container] = []) {
        self.id = id
        self.name = name
        self.collapsed = collapsed
        self.bannerFont = bannerFont
        self.defaults = defaults
        self.members = members
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        name = try c.decode(String.self, forKey: .name)
        collapsed = try c.decodeIfPresent(Bool.self, forKey: .collapsed) ?? false
        bannerFont = try c.decodeIfPresent(String.self, forKey: .bannerFont)
        defaults = try c.decodeIfPresent(SectionDefaults.self, forKey: .defaults)
        // A role this build doesn't know shows as an ordinary section.
        role = (try? c.decodeIfPresent(SectionRole.self, forKey: .role)) ?? nil
        members = try c.decodeIfPresent([Container].self, forKey: .members) ?? []
    }
}

/// A section's starting values for a new session. An empty path or args, or no agent, leaves that field to
/// the sheet's usual default.
public struct SectionDefaults: Codable, Equatable, Sendable {
    public var path: String
    public var agent: String?
    public var args: String
    public var options: SpecOptions

    public init(path: String = "", agent: String? = nil, args: String = "", options: SpecOptions = SpecOptions()) {
        self.path = path; self.agent = agent; self.args = args; self.options = options
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        path = try c.decodeIfPresent(String.self, forKey: .path) ?? ""
        agent = try c.decodeIfPresent(String.self, forKey: .agent)
        args = try c.decodeIfPresent(String.self, forKey: .args) ?? ""
        options = try c.decodeIfPresent(SpecOptions.self, forKey: .options) ?? SpecOptions()
    }
}

public struct Sidebar: Codable, Equatable, Sendable {
    public var ungrouped: [Container]
    public var sections: [Section]

    public init(ungrouped: [Container] = [], sections: [Section] = []) {
        self.ungrouped = ungrouped
        self.sections = sections
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        ungrouped = try c.decodeIfPresent([Container].self, forKey: .ungrouped) ?? []
        sections = try c.decodeIfPresent([Section].self, forKey: .sections) ?? []
    }
}

public enum SessionMode: String, Codable, Equatable, Sendable, CaseIterable {
    case new, `continue`, resume

    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = SessionMode(rawValue: raw) ?? .new
    }

    public var label: String {
        switch self {
        case .new: "New"
        case .continue: "Continue"
        case .resume: "Resume"
        }
    }
}

public struct ClaudeOptions: Codable, Equatable, Sendable {
    public var sessionMode: SessionMode = .new
    public var skipPermissions = false
    public var remoteControl = false
    enum CodingKeys: String, CodingKey {
        case sessionMode = "session_mode", skipPermissions = "skip_permissions", remoteControl = "remote_control"
    }
    public init() {}
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        sessionMode = try c.decodeIfPresent(SessionMode.self, forKey: .sessionMode) ?? .new
        skipPermissions = try c.decodeIfPresent(Bool.self, forKey: .skipPermissions) ?? false
        remoteControl = try c.decodeIfPresent(Bool.self, forKey: .remoteControl) ?? false
    }
}

public struct ModeYoloOptions: Codable, Equatable, Sendable {
    public var sessionMode: SessionMode = .new
    public var yolo = false
    enum CodingKeys: String, CodingKey { case sessionMode = "session_mode", yolo }
    public init() {}
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        sessionMode = try c.decodeIfPresent(SessionMode.self, forKey: .sessionMode) ?? .new
        yolo = try c.decodeIfPresent(Bool.self, forKey: .yolo) ?? false
    }
}

public struct OpencodeOptions: Codable, Equatable, Sendable {
    public var sessionMode: SessionMode = .new
    public var auto = false
    enum CodingKeys: String, CodingKey { case sessionMode = "session_mode", auto }
    public init() {}
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        sessionMode = try c.decodeIfPresent(SessionMode.self, forKey: .sessionMode) ?? .new
        auto = try c.decodeIfPresent(Bool.self, forKey: .auto) ?? false
    }
}

public struct SpecOptions: Codable, Equatable, Sendable {
    public var claude = ClaudeOptions()
    public var codex = ModeYoloOptions()
    public var kimi = ModeYoloOptions()
    public var opencode = OpencodeOptions()
    public var qwen = ModeYoloOptions()
    public init() {}
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        claude = try c.decodeIfPresent(ClaudeOptions.self, forKey: .claude) ?? ClaudeOptions()
        codex = try c.decodeIfPresent(ModeYoloOptions.self, forKey: .codex) ?? ModeYoloOptions()
        kimi = try c.decodeIfPresent(ModeYoloOptions.self, forKey: .kimi) ?? ModeYoloOptions()
        opencode = try c.decodeIfPresent(OpencodeOptions.self, forKey: .opencode) ?? OpencodeOptions()
        qwen = try c.decodeIfPresent(ModeYoloOptions.self, forKey: .qwen) ?? ModeYoloOptions()
    }
}

public let agentNames = ["claude", "codex", "kimi", "opencode", "qwen", "terminal"]

public struct Spec: Codable, Equatable, Sendable {
    public var name: String
    public var path: String
    public var agent: String
    public var args: String
    public var options: SpecOptions
    public var containerID: String?
    public var resume: Bool
    public var worktreeBranch: String?

    enum CodingKeys: String, CodingKey {
        case name, path, agent, args, options, resume
        case containerID = "container_id"
        case worktreeBranch = "worktree_branch"
    }

    public init(name: String, path: String = "", agent: String = "claude", args: String = "", options: SpecOptions = SpecOptions(),
                containerID: String? = nil, resume: Bool = false, worktreeBranch: String? = nil) {
        self.name = name; self.path = path; self.agent = agent; self.args = args; self.options = options
        self.containerID = containerID; self.resume = resume; self.worktreeBranch = worktreeBranch
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        name = try c.decode(String.self, forKey: .name)
        path = try c.decodeIfPresent(String.self, forKey: .path) ?? ""
        agent = try c.decode(String.self, forKey: .agent)
        args = try c.decodeIfPresent(String.self, forKey: .args) ?? ""
        options = try c.decodeIfPresent(SpecOptions.self, forKey: .options) ?? SpecOptions()
        containerID = try c.decodeIfPresent(String.self, forKey: .containerID)
        resume = try c.decodeIfPresent(Bool.self, forKey: .resume) ?? false
        worktreeBranch = try c.decodeIfPresent(String.self, forKey: .worktreeBranch)
    }
}

public struct Recent: Codable, Equatable, Sendable {
    public var id: Int64
    public var name: String
    public var path: String
    public var agent: String
    public var args: String
    public var options: SpecOptions
    public var lastUsedAt: Int64
    public var useCount: Int64

    enum CodingKeys: String, CodingKey {
        case id, name, path, agent, args, options
        case lastUsedAt = "last_used_at"
        case useCount = "use_count"
    }

    public init(id: Int64, name: String, path: String, agent: String, args: String = "", options: SpecOptions = SpecOptions(),
                lastUsedAt: Int64 = 0, useCount: Int64 = 1) {
        self.id = id; self.name = name; self.path = path; self.agent = agent; self.args = args
        self.options = options; self.lastUsedAt = lastUsedAt; self.useCount = useCount
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(Int64.self, forKey: .id)
        name = try c.decode(String.self, forKey: .name)
        path = try c.decodeIfPresent(String.self, forKey: .path) ?? ""
        agent = try c.decode(String.self, forKey: .agent)
        args = try c.decodeIfPresent(String.self, forKey: .args) ?? ""
        options = try c.decodeIfPresent(SpecOptions.self, forKey: .options) ?? SpecOptions()
        lastUsedAt = try c.decodeIfPresent(Int64.self, forKey: .lastUsedAt) ?? 0
        useCount = try c.decodeIfPresent(Int64.self, forKey: .useCount) ?? 0
    }

    public var spec: Spec { Spec(name: name, path: path, agent: agent, args: args, options: options) }
}

public struct Setting: Codable, Equatable, Sendable {
    public var key: String
    public var value: JSONValue
    public var choices: [String]?
    public var pinnedBy: String?
    public var readOnly: Bool

    enum CodingKeys: String, CodingKey {
        case key, value, choices
        case pinnedBy = "pinned_by"
        case readOnly = "read_only"
    }

    public init(key: String, value: JSONValue, choices: [String]? = nil, pinnedBy: String? = nil, readOnly: Bool = false) {
        self.key = key; self.value = value; self.choices = choices; self.pinnedBy = pinnedBy; self.readOnly = readOnly
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        key = try c.decode(String.self, forKey: .key)
        value = try c.decodeIfPresent(JSONValue.self, forKey: .value) ?? .null
        choices = try c.decodeIfPresent([String].self, forKey: .choices)
        pinnedBy = try c.decodeIfPresent(String.self, forKey: .pinnedBy)
        readOnly = try c.decodeIfPresent(Bool.self, forKey: .readOnly) ?? false
    }
}

public struct ConfigView: Codable, Equatable, Sendable {
    public var settings: [Setting]
    public init(settings: [Setting] = []) { self.settings = settings }
    public func value(_ key: String) -> JSONValue? { settings.first { $0.key == key }?.value }
}

/// A theme's terminal colours (the `[terminal]` table of a theme file), as the daemon sends them. Absent from
/// themes of an older daemon.
public struct TerminalPalette: Codable, Equatable, Sendable {
    /// The Ghostty theme the colours were taken from; for reference, the colours below are what the app uses.
    public var ghostty: String?
    public var background: String
    public var foreground: String
    public var cursor: String
    public var selectionBg: String
    public var selectionFg: String
    /// ANSI 0-7 normal, then 8-15 bright.
    public var palette: [String]

    enum CodingKeys: String, CodingKey {
        case ghostty, background, foreground, cursor, palette
        case selectionBg = "selection_bg", selectionFg = "selection_fg"
    }

    public init(ghostty: String? = nil, background: String, foreground: String, cursor: String, selectionBg: String,
                selectionFg: String, palette: [String]) {
        self.ghostty = ghostty; self.background = background; self.foreground = foreground; self.cursor = cursor
        self.selectionBg = selectionBg; self.selectionFg = selectionFg; self.palette = palette
    }
}

public struct ThemePalette: Codable, Equatable, Sendable {
    public var name: String
    public var bg: String
    public var panel: String
    public var panelAlt: String
    public var selectionBg: String
    public var text: String
    public var textMuted: String
    public var accent: String
    public var shadow: String
    public var dimFg: String
    public var statusRunning: String
    public var statusWaiting: String
    public var statusIdle: String
    public var statusError: String
    public var statusDone: String
    /// The theme's terminal colours; nil from a daemon that predates the `[terminal]` table.
    public var terminal: TerminalPalette?

    enum CodingKeys: String, CodingKey {
        case name, bg, panel, text, accent, shadow, terminal
        case panelAlt = "panel_alt", selectionBg = "selection_bg", textMuted = "text_muted", dimFg = "dim_fg"
        case statusRunning = "status_running", statusWaiting = "status_waiting", statusIdle = "status_idle"
        case statusError = "status_error", statusDone = "status_done"
    }

    /// The opencode palette (`themes/opencode.toml`), used before any snapshot has arrived. A test keeps it equal
    /// to the file.
    public static let fallback = ThemePalette(
        name: "opencode", bg: "#0b0d12", panel: "#11141b", panelAlt: "#131722", selectionBg: "#1e2433",
        text: "#e6e9ef", textMuted: "#7c8495", accent: "#7c5cff", shadow: "#05070b", dimFg: "#3c4254",
        statusRunning: "#62d98c", statusWaiting: "#f4c169", statusIdle: "#7c8495", statusError: "#ff5d6b",
        statusDone: "#6ec8ff",
        terminal: TerminalPalette(
            background: "#0b0d12", foreground: "#e6e9ef", cursor: "#7c5cff", selectionBg: "#1e2433", selectionFg: "#e6e9ef",
            palette: ["#25272d", "#ff5d6b", "#62d98c", "#f4c169", "#6ec8ff", "#b692b5", "#68d0c6", "#e6e9ef",
                      "#7c8495", "#ff7a86", "#7ee0a1", "#f6cc84", "#88d2ff", "#c4a6c2", "#83d9d0", "#ebedf2"]))

    public init(name: String, bg: String, panel: String, panelAlt: String, selectionBg: String, text: String, textMuted: String,
                accent: String, shadow: String, dimFg: String, statusRunning: String, statusWaiting: String,
                statusIdle: String, statusError: String, statusDone: String, terminal: TerminalPalette? = nil) {
        self.name = name; self.bg = bg; self.panel = panel; self.panelAlt = panelAlt; self.selectionBg = selectionBg
        self.text = text; self.textMuted = textMuted; self.accent = accent; self.shadow = shadow; self.dimFg = dimFg
        self.statusRunning = statusRunning; self.statusWaiting = statusWaiting; self.statusIdle = statusIdle
        self.statusError = statusError; self.statusDone = statusDone; self.terminal = terminal
    }
}

public struct ThemeEntry: Codable, Equatable, Sendable {
    public var name: String
    public var source: String
    public init(name: String, source: String = "bundled") { self.name = name; self.source = source }
}

/// The TUI's `[keybindings]` table as the daemon sends it. A field that is absent keeps the default;
/// an explicit `null` disables the binding.
public struct KeyBindingsWire: Codable, Equatable, Sendable {
    public enum Value: Equatable, Sendable {
        case unset
        case disabled
        case chord(String)
    }

    public var previousTab: Value = .unset
    public var nextTab: Value = .unset
    public var previousSession: Value = .unset
    public var nextSession: Value = .unset
    public var sendNextKey: Value = .unset

    enum CodingKeys: String, CodingKey {
        case previousTab = "previous_tab", nextTab = "next_tab", previousSession = "previous_session"
        case nextSession = "next_session", sendNextKey = "send_next_key"
    }

    public init() {}

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        func read(_ k: CodingKeys) throws -> Value {
            guard c.contains(k) else { return .unset }
            if try c.decodeNil(forKey: k) { return .disabled }
            let s = try c.decode(String.self, forKey: k)
            let t = s.trimmingCharacters(in: .whitespaces)
            if t.isEmpty || t.lowercased() == "none" { return .disabled }
            return .chord(s)
        }
        previousTab = try read(.previousTab)
        nextTab = try read(.nextTab)
        previousSession = try read(.previousSession)
        nextSession = try read(.nextSession)
        sendNextKey = try read(.sendNextKey)
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        func write(_ v: Value, _ k: CodingKeys) throws {
            switch v {
            case .unset: break
            case .disabled: try c.encodeNil(forKey: k)
            case .chord(let s): try c.encode(s, forKey: k)
            }
        }
        try write(previousTab, .previousTab)
        try write(nextTab, .nextTab)
        try write(previousSession, .previousSession)
        try write(nextSession, .nextSession)
        try write(sendNextKey, .sendNextKey)
    }
}

public struct Snapshot: Codable, Equatable, Sendable {
    public var seq: UInt64
    public var server: ServerInfo
    public var sessions: [SessionInfo]
    public var statuses: [String: StatusInfo]
    public var sidebar: Sidebar
    public var recents: [Recent]
    public var config: ConfigView
    public var theme: ThemePalette
    /// The Light/Dark pair's palettes (`theme_light` / `theme_dark`); nil is unset, "use `theme`".
    public var themeLight: ThemePalette?
    public var themeDark: ThemePalette?
    public var themes: [ThemeEntry]
    public var keybindings: KeyBindingsWire?
    /// Warnings that hold while the daemon runs (an out-of-date tmux server), also sent as `notice`
    /// before this client connected.
    public var notices: [String]

    enum CodingKeys: String, CodingKey {
        case seq, server, sessions, statuses, sidebar, recents, config, theme, themes, keybindings, notices
        case themeLight = "theme_light", themeDark = "theme_dark"
    }

    public init(seq: UInt64, server: ServerInfo, sessions: [SessionInfo], statuses: [String: StatusInfo], sidebar: Sidebar,
                recents: [Recent], config: ConfigView, theme: ThemePalette, themeLight: ThemePalette? = nil,
                themeDark: ThemePalette? = nil, themes: [ThemeEntry], keybindings: KeyBindingsWire?,
                notices: [String] = []) {
        self.seq = seq; self.server = server; self.sessions = sessions; self.statuses = statuses; self.sidebar = sidebar
        self.recents = recents; self.config = config; self.theme = theme; self.themeLight = themeLight; self.themeDark = themeDark
        self.themes = themes; self.keybindings = keybindings; self.notices = notices
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        seq = try c.decode(UInt64.self, forKey: .seq)
        server = try c.decode(ServerInfo.self, forKey: .server)
        sessions = try c.decodeIfPresent([SessionInfo].self, forKey: .sessions) ?? []
        statuses = try c.decodeIfPresent([String: StatusInfo].self, forKey: .statuses) ?? [:]
        sidebar = try c.decodeIfPresent(Sidebar.self, forKey: .sidebar) ?? Sidebar()
        recents = try c.decodeIfPresent([Recent].self, forKey: .recents) ?? []
        config = try c.decodeIfPresent(ConfigView.self, forKey: .config) ?? ConfigView()
        theme = try c.decodeIfPresent(ThemePalette.self, forKey: .theme) ?? .fallback
        themeLight = try c.decodeIfPresent(ThemePalette.self, forKey: .themeLight)
        themeDark = try c.decodeIfPresent(ThemePalette.self, forKey: .themeDark)
        themes = try c.decodeIfPresent([ThemeEntry].self, forKey: .themes) ?? []
        keybindings = try c.decodeIfPresent(KeyBindingsWire.self, forKey: .keybindings)
        notices = try c.decodeIfPresent([String].self, forKey: .notices) ?? []
    }
}

public struct PendingLaunch: Codable, Equatable, Sendable {
    public var session: String
    public var resume: Bool?
}

/// One step of a search through a session's history (tmux's copy mode).
public enum SearchAction: String, Codable, Sendable {
    /// Search for the text as typed so far, towards older output.
    case find
    /// The next match towards older output.
    case older
    /// The next match towards newer output.
    case newer
    /// Leave copy mode, back to the live output.
    case end
}

/// How a search marks matches, as `#rrggbb`: the match the pane is on, and every other.
public struct SearchColors: Codable, Equatable, Sendable {
    public var currentBg, currentFg, matchBg, matchFg: String

    public init(currentBg: String, currentFg: String, matchBg: String, matchFg: String) {
        self.currentBg = currentBg; self.currentFg = currentFg; self.matchBg = matchBg; self.matchFg = matchFg
    }

    enum CodingKeys: String, CodingKey {
        case currentBg = "current_bg", currentFg = "current_fg", matchBg = "match_bg", matchFg = "match_fg"
    }
}

/// Where a search stands after a step.
public struct SearchState: Codable, Equatable, Sendable {
    public var found: Bool
    /// Matches in the whole history; nil when tmux doesn't count them (3.4 doesn't).
    public var count: Int?
    /// tmux stopped counting early: there are at least `count`.
    public var partial: Bool

    public init(found: Bool, count: Int? = nil, partial: Bool = false) {
        self.found = found; self.count = count; self.partial = partial
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        found = try c.decode(Bool.self, forKey: .found)
        count = try c.decodeIfPresent(Int.self, forKey: .count)
        partial = try c.decodeIfPresent(Bool.self, forKey: .partial) ?? false
    }

    /// What the find bar says: "12 matches", "1,000+ matches", "No matches", or "Found" when tmux can't count.
    public var label: String {
        guard found else { return "No matches" }
        guard let count else { return "Found" }
        let n = count.formatted()
        return "\(n)\(partial ? "+" : "") \(count == 1 && !partial ? "match" : "matches")"
    }
}

public struct Ack: Codable, Equatable, Sendable {
    public var id: UInt64
    public var ok: Bool
    public var error: String?
    public var session: String?
    public var spec: Spec?
    /// `get_theme`: the theme's palette.
    public var theme: ThemePalette?
    public var pendingLaunch: PendingLaunch?
    public var messages: [String]
    /// `search`: where the search stands.
    public var search: SearchState?

    enum CodingKeys: String, CodingKey {
        case id, ok, error, session, spec, theme, messages, search
        case pendingLaunch = "pending_launch"
    }

    public init(id: UInt64, ok: Bool, error: String? = nil, session: String? = nil, spec: Spec? = nil,
                pendingLaunch: PendingLaunch? = nil, messages: [String] = []) {
        self.id = id; self.ok = ok; self.error = error; self.session = session; self.spec = spec
        self.pendingLaunch = pendingLaunch; self.messages = messages
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(UInt64.self, forKey: .id)
        ok = try c.decode(Bool.self, forKey: .ok)
        error = try c.decodeIfPresent(String.self, forKey: .error)
        session = try c.decodeIfPresent(String.self, forKey: .session)
        spec = try c.decodeIfPresent(Spec.self, forKey: .spec)
        theme = try c.decodeIfPresent(ThemePalette.self, forKey: .theme)
        pendingLaunch = try c.decodeIfPresent(PendingLaunch.self, forKey: .pendingLaunch)
        search = try c.decodeIfPresent(SearchState.self, forKey: .search)
        messages = try c.decodeIfPresent([String].self, forKey: .messages) ?? []
    }
}

// MARK: - Daemon → client

public enum ServerMessage: Equatable, Sendable {
    case snapshot(Snapshot)
    case sessions(seq: UInt64, upsert: [SessionInfo], removed: [String])
    case status(seq: UInt64, session: String, info: StatusInfo)
    case sidebar(seq: UInt64, Sidebar)
    case recents(seq: UInt64, [Recent])
    case config(seq: UInt64, ConfigView)
    case theme(seq: UInt64, ThemePalette)
    case themes(seq: UInt64, [ThemeEntry])
    /// The Light/Dark pair changed; it replaces both sides (nil: unset).
    case themePair(seq: UInt64, light: ThemePalette?, dark: ThemePalette?)
    case keybindings(seq: UInt64, KeyBindingsWire)
    case notice(seq: UInt64, String)
    /// The tmux client on `tty` is now on `session` (nil: it detached). An event, not state.
    case client(seq: UInt64, tty: String, session: String?)
    case ack(Ack)
    case error(code: String, message: String)
    /// A kind this client doesn't know. Its `seq` still counts.
    case unknown(kind: String, seq: UInt64?)

    public var seq: UInt64? {
        switch self {
        case .snapshot(let s): s.seq
        case .sessions(let q, _, _), .status(let q, _, _), .sidebar(let q, _), .recents(let q, _), .config(let q, _),
             .theme(let q, _), .themes(let q, _), .themePair(let q, _, _), .keybindings(let q, _), .notice(let q, _),
             .client(let q, _, _): q
        case .ack, .error: nil
        case .unknown(_, let q): q
        }
    }

    private struct Head: Decodable {
        let t: String
        let seq: UInt64?
    }
    private struct SessionsBody: Decodable {
        let seq: UInt64
        let upsert: [SessionInfo]?
        let removed: [String]?
    }
    private struct StatusBody: Decodable {
        let seq: UInt64
        let session: String
        let state: AgentState
        let source: String?
        let detail: String?
        let doneAt: UInt64?
        let startedAt: UInt64?

        enum CodingKeys: String, CodingKey {
            case seq, session, state, source, detail
            case doneAt = "done_at"
            case startedAt = "started_at"
        }
    }
    private struct Body<T: Decodable>: Decodable {
        let seq: UInt64
        let value: T
        init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: AnyKey.self)
            seq = try c.decode(UInt64.self, forKey: AnyKey("seq"))
            let key = decoder.userInfo[Body.fieldKey] as? String ?? ""
            value = try c.decode(T.self, forKey: AnyKey(key))
        }
        static var fieldKey: CodingUserInfoKey { CodingUserInfoKey(rawValue: "bosun.field")! }
    }
    private struct ThemePairBody: Decodable {
        let seq: UInt64
        let light: ThemePalette?
        let dark: ThemePalette?

        enum CodingKeys: String, CodingKey {
            case seq
            case light = "theme_light", dark = "theme_dark"
        }
    }
    private struct ClientBody: Decodable {
        let seq: UInt64
        let tty: String
        let session: String?
    }
    private struct ErrorBody: Decodable {
        let code: String
        let message: String
    }

    public static func decode(_ data: Data) throws -> ServerMessage {
        let plain = JSONDecoder()
        let head = try plain.decode(Head.self, from: data)
        func field<T: Decodable>(_ type: T.Type, _ key: String) throws -> (UInt64, T) {
            let d = JSONDecoder()
            d.userInfo[Body<T>.fieldKey] = key
            let b = try d.decode(Body<T>.self, from: data)
            return (b.seq, b.value)
        }
        switch head.t {
        case "snapshot":
            return .snapshot(try plain.decode(Snapshot.self, from: data))
        case "sessions":
            let b = try plain.decode(SessionsBody.self, from: data)
            return .sessions(seq: b.seq, upsert: b.upsert ?? [], removed: b.removed ?? [])
        case "status":
            let b = try plain.decode(StatusBody.self, from: data)
            return .status(seq: b.seq, session: b.session, info: StatusInfo(state: b.state, source: b.source ?? "detector", detail: b.detail,
                                                                                      doneAt: b.doneAt, startedAt: b.startedAt))
        case "sidebar":
            let (q, v) = try field(Sidebar.self, "sidebar")
            return .sidebar(seq: q, v)
        case "recents":
            let (q, v) = try field([Recent].self, "recents")
            return .recents(seq: q, v)
        case "config":
            let (q, v) = try field(ConfigView.self, "config")
            return .config(seq: q, v)
        case "theme":
            let (q, v) = try field(ThemePalette.self, "theme")
            return .theme(seq: q, v)
        case "themes":
            let (q, v) = try field([ThemeEntry].self, "themes")
            return .themes(seq: q, v)
        case "theme_pair":
            let b = try plain.decode(ThemePairBody.self, from: data)
            return .themePair(seq: b.seq, light: b.light, dark: b.dark)
        case "keybindings":
            let (q, v) = try field(KeyBindingsWire.self, "keybindings")
            return .keybindings(seq: q, v)
        case "notice":
            let (q, v) = try field(String.self, "message")
            return .notice(seq: q, v)
        case "client_session", "client_detached":
            let b = try plain.decode(ClientBody.self, from: data)
            return .client(seq: b.seq, tty: b.tty, session: head.t == "client_session" ? b.session : nil)
        case "ack":
            return .ack(try plain.decode(Ack.self, from: data))
        case "error":
            let b = try plain.decode(ErrorBody.self, from: data)
            return .error(code: b.code, message: b.message)
        default:
            return .unknown(kind: head.t, seq: head.seq)
        }
    }
}

struct AnyKey: CodingKey {
    var stringValue: String
    var intValue: Int? { nil }
    init(_ s: String) { stringValue = s }
    init?(stringValue: String) { self.stringValue = stringValue }
    init?(intValue: Int) { nil }
}

// MARK: - Client → daemon

public enum Command: Equatable, Sendable {
    /// A new session. `section` puts its new row at the end of that section (ignored for a tab of a row);
    /// `replaces` names an exited tab the session restarts, which it takes the place of.
    case create(spec: Spec, deferLaunch: Bool, section: String? = nil, replaces: String? = nil)
    case kill(session: String)
    case killContainer(sessions: [String])
    case killRemoveWorktree(session: String, worktreePath: String, branch: String, merge: Bool)
    case rename(session: String, display: String)
    case restart(session: String, resume: Bool, deferLaunch: Bool)
    case launchAgent(session: String, resume: Bool?)
    case getSpec(session: String)
    /// Any theme's palette, for a live preview (changes nothing).
    case getTheme(name: String)
    case modify(session: String, spec: Spec)
    case deleteRecent(id: Int64)
    case sidebarSave(Sidebar)
    case insertSection(name: String)
    case renameSection(section: String, name: String)
    /// A section's starting values for new sessions; nil clears them.
    case setSectionDefaults(section: String, defaults: SectionDefaults?)
    /// Park a row in Snoozed or Docked (docking kills its sessions). A snoozed row wakes at `wakeAt` (Unix ms)
    /// or, with `untilNeeded`, when one of its agents needs you.
    case parkRow(container: String, role: SectionRole, wakeAt: UInt64? = nil, untilNeeded: Bool = false)
    /// A parked row goes back where it came from; an undocked row's sessions are resumed.
    case unparkRow(container: String)
    case setTheme(name: String, persist: Bool)
    case applySetting(key: String, value: JSONValue)
    case clientFlags(tty: String, ignoreSize: Bool)
    case makeLatest(tty: String)
    /// Move one tab into row `container` (at `index` among its tabs), or into a new row of its own with nil:
    /// at `index` among the rows of `section` (the ungrouped rows with nil), or right after the old one with no `index`.
    case moveTab(session: String, container: String?, index: Int?, section: String? = nil)
    /// Move every tab of row `from` into row `into` (at `index` among its tabs, at the end with nil).
    case mergeRows(from: String, into: String, index: Int?)
    /// A step of a search through a session's history; the ack carries `search`. Case is ignored unless
    /// `matchCase`; `colors` (on a `find`) set how matches look.
    case search(session: String, action: SearchAction, text: String, matchCase: Bool = false, colors: SearchColors? = nil)

    public var op: String {
        switch self {
        case .create: "create"
        case .kill: "kill"
        case .killContainer: "kill_container"
        case .killRemoveWorktree: "kill_remove_worktree"
        case .rename: "rename"
        case .restart: "restart"
        case .launchAgent: "launch_agent"
        case .getSpec: "get_spec"
        case .getTheme: "get_theme"
        case .modify: "modify"
        case .deleteRecent: "delete_recent"
        case .sidebarSave: "sidebar_save"
        case .insertSection: "insert_section"
        case .renameSection: "rename_section"
        case .setSectionDefaults: "set_section_defaults"
        case .parkRow: "park_row"
        case .unparkRow: "unpark_row"
        case .setTheme: "set_theme"
        case .applySetting: "apply_setting"
        case .clientFlags: "client_flags"
        case .makeLatest: "make_latest"
        case .moveTab: "move_tab"
        case .mergeRows: "merge_rows"
        case .search: "search"
        }
    }
}

/// Saved form of a sidebar for `sidebar_save`: tabs carry only `session`.
private struct SaveSidebar: Encodable {
    struct T: Encodable { let session: String }
    struct C: Encodable { let id: String; let name: String; let active: String; let tabs: [T] }
    struct S: Encodable {
        let id: String; let name: String; let collapsed: Bool; let banner_font: String?; let members: [C]
    }
    let ungrouped: [C]
    let sections: [S]

    init(_ s: Sidebar) {
        func c(_ x: Container) -> C { C(id: x.id, name: x.name, active: x.active, tabs: x.tabs.map { T(session: $0.session) }) }
        ungrouped = s.ungrouped.map(c)
        sections = s.sections.map { S(id: $0.id, name: $0.name, collapsed: $0.collapsed, banner_font: $0.bannerFont, members: $0.members.map(c)) }
    }
}

public enum ClientMessage: Sendable {
    case hello(client: String)
    case resync
    case cmd(id: UInt64, Command)

    public func encoded() throws -> Data {
        let enc = JSONEncoder()
        enc.outputFormatting = [.sortedKeys]
        return try enc.encode(Wire(self))
    }

    private struct Wire: Encodable {
        let msg: ClientMessage
        init(_ m: ClientMessage) { msg = m }

        func encode(to encoder: Encoder) throws {
            var c = encoder.container(keyedBy: AnyKey.self)
            func put<T: Encodable>(_ k: String, _ v: T) throws { try c.encode(v, forKey: AnyKey(k)) }
            switch msg {
            case .hello(let client):
                try put("t", "hello")
                try put("protocol", protocolVersion)
                try put("client", client)
            case .resync:
                try put("t", "resync")
            case .cmd(let id, let cmd):
                try put("t", "cmd")
                try put("id", id)
                try put("op", cmd.op)
                switch cmd {
                case .create(let spec, let deferLaunch, let section, let replaces):
                    try put("spec", spec)
                    try put("defer_launch", deferLaunch)
                    if let section { try put("section", section) }
                    if let replaces { try put("replaces", replaces) }
                case .kill(let s):
                    try put("session", s)
                case .killContainer(let s):
                    try put("sessions", s)
                case .killRemoveWorktree(let s, let p, let b, let m):
                    try put("session", s); try put("worktree_path", p); try put("branch", b); try put("merge", m)
                case .rename(let s, let d):
                    try put("session", s); try put("display", d)
                case .restart(let s, let r, let d):
                    try put("session", s); try put("resume", r); try put("defer_launch", d)
                case .launchAgent(let s, let r):
                    try put("session", s)
                    if let r { try put("resume", r) } else { try c.encodeNil(forKey: AnyKey("resume")) }
                case .getSpec(let s):
                    try put("session", s)
                case .getTheme(let n):
                    try put("name", n)
                case .modify(let s, let spec):
                    try put("session", s); try put("spec", spec)
                case .deleteRecent(let id):
                    try put("recent", id)
                case .sidebarSave(let sb):
                    try put("sidebar", SaveSidebar(sb))
                case .insertSection(let n):
                    try put("name", n)
                case .renameSection(let s, let n):
                    try put("section", s); try put("name", n)
                case .parkRow(let row, let role, let wakeAt, let untilNeeded):
                    try put("container_id", row); try put("role", role)
                    if let wakeAt { try put("wake_at", wakeAt) }
                    if untilNeeded { try put("until_needed", true) }
                case .unparkRow(let row):
                    try put("container_id", row)
                case .setSectionDefaults(let s, let d):
                    try put("section", s)
                    if let d { try put("defaults", d) } else { try c.encodeNil(forKey: AnyKey("defaults")) }
                case .setTheme(let n, let p):
                    try put("name", n); try put("persist", p)
                case .applySetting(let k, let v):
                    try put("key", k); try put("value", v)
                case .clientFlags(let t, let i):
                    try put("tty", t); try put("ignore_size", i)
                case .makeLatest(let t):
                    try put("tty", t)
                case .moveTab(let s, let container, let index, let section):
                    try put("session", s)
                    if let container { try put("container_id", container) } else { try c.encodeNil(forKey: AnyKey("container_id")) }
                    if let index { try put("index", index) }
                    if let section { try put("section", section) }
                case .mergeRows(let from, let into, let index):
                    try put("from", from); try put("into", into)
                    if let index { try put("index", index) }
                case .search(let s, let action, let text, let matchCase, let colors):
                    try put("session", s); try put("action", action); try put("text", text)
                    if matchCase { try put("match_case", true) }
                    if let colors { try put("colors", colors) }
                }
            }
        }
    }
}

// MARK: - Framing

public enum FrameError: Error, Equatable {
    case tooLarge(UInt32)
}

public enum Frame {
    public static let maxLength: UInt32 = 16 * 1024 * 1024

    public static func encode(_ body: Data) -> Data {
        var out = Data(capacity: body.count + 4)
        let n = UInt32(body.count).bigEndian
        withUnsafeBytes(of: n) { out.append(contentsOf: $0) }
        out.append(body)
        return out
    }
}

/// Accumulates bytes from a stream socket and yields whole frames.
public struct FrameReader: Sendable {
    private var buffer = Data()
    public init() {}

    public mutating func append(_ data: Data) { buffer.append(data) }

    public mutating func next() throws -> Data? {
        guard buffer.count >= 4 else { return nil }
        let start = buffer.startIndex
        let len = buffer[start..<start + 4].reduce(UInt32(0)) { ($0 << 8) | UInt32($1) }
        if len > Frame.maxLength { throw FrameError.tooLarge(len) }
        guard buffer.count >= 4 + Int(len) else { return nil }
        let body = buffer.subdata(in: start + 4..<start + 4 + Int(len))
        buffer.removeSubrange(start..<start + 4 + Int(len))
        return body
    }
}
