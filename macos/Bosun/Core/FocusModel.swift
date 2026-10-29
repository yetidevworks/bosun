import CoreGraphics
import Foundation

/// The keyboard-ownership rules from the design (section 7b), with no views involved.
///
/// The active tile's terminal owns the keyboard while the window is key. Only an open
/// "exception" (in-place rename, the palette, a sheet, the sidebar filter's field) borrows it, and when that closes
/// the keyboard goes straight back to the active terminal.
public struct FocusModel: Equatable, Sendable {
    public enum Exception: String, Equatable, Sendable {
        case rename, palette, sheet
        /// The sidebar filter's field while it has the ring. What was typed can stay applied after the
        /// keyboard has gone back (a chip with no ring), but only the field with its ring holds the keyboard.
        case filter
        /// The find bar on the active pane (Cmd+F) while its field has the keyboard.
        case find
    }

    public enum ClickResult: Equatable, Sendable {
        /// The tile was inactive: it becomes active and the click is swallowed.
        case activateAndConsume(TileID)
        /// The tile is already active: the click goes to the terminal.
        case passThrough
    }

    public private(set) var active: TileID?
    public private(set) var tileCount = 1
    public private(set) var exception: Exception?
    public private(set) var windowKey = true
    public var followsMouse = true
    private var lastPointer: CGPoint?

    public init(active: TileID? = nil) {
        self.active = active
    }

    /// The terminal should be first responder exactly when this is true.
    public var terminalOwnsKeyboard: Bool { exception == nil && active != nil }

    public mutating func setTiles(count: Int, active: TileID?) {
        tileCount = count
        self.active = active
    }

    public mutating func activate(_ tile: TileID) { active = tile }

    public mutating func setWindowKey(_ key: Bool) {
        windowKey = key
        // A window that just became key must see real movement before hover activates anything.
        lastPointer = nil
    }

    /// Opening an exception borrows the keyboard. Only one at a time; a second request replaces the first.
    public mutating func begin(_ e: Exception) { exception = e }

    /// Closing returns the keyboard. Returns true when the caller should make the active terminal first responder.
    @discardableResult
    public mutating func end(_ e: Exception) -> Bool {
        guard exception == e else { return false }
        exception = nil
        return true
    }

    public func click(on tile: TileID) -> ClickResult {
        tile == active ? .passThrough : .activateAndConsume(tile)
    }

    /// A mouse-moved event at `point` (screen coordinates) over `tile`. Returns the tile to activate, if any.
    /// Only real pointer movement counts: an event at the same point as the last one (a tile moved under
    /// a still pointer) never activates.
    public mutating func pointerMoved(to point: CGPoint, over tile: TileID?, buttonsDown: Bool) -> TileID? {
        defer { lastPointer = point }
        guard let last = lastPointer, last != point else { return nil }
        guard followsMouse, tileCount >= 2, windowKey, exception == nil, !buttonsDown else { return nil }
        guard let tile, tile != active else { return nil }
        return tile
    }

    /// A drop on `tile` always activates it (whatever was active before).
    public func dropTarget(_ tile: TileID?) -> TileID? { tile }

    /// The ring is drawn only when there is something to tell apart.
    public var showsRing: Bool { tileCount >= 2 }
}
