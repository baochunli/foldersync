import Cocoa

enum BadgeRenderer {
    /// Creates an accessible badge image with a distinct symbol shape and high-contrast color
    /// for each state, requiring no Xcode asset catalog (`actool`).
    static func image(for state: BadgeState) -> NSImage {
        let spec: (symbol: String, color: NSColor)
        switch state {
        case .synced:
            spec = ("checkmark.circle.fill", NSColor.systemGreen)
        case .syncing:
            spec = ("arrow.triangle.2.circlepath.circle.fill", NSColor.systemBlue)
        case .attention:
            spec = ("exclamationmark.triangle.fill", NSColor.systemOrange)
        case .paused:
            spec = ("pause.circle.fill", NSColor.systemGray)
        case .diskMissing:
            spec = ("eject.circle.fill", NSColor.systemRed)
        case .stale:
            spec = ("questionmark.circle.fill", NSColor.secondaryLabelColor)
        }

        if let base = NSImage(systemSymbolName: spec.symbol, accessibilityDescription: state.displayLabel) {
            let config = NSImage.SymbolConfiguration(pointSize: 16, weight: .semibold)
                .applying(NSImage.SymbolConfiguration(paletteColors: [.white, spec.color]))
            if let styled = base.withSymbolConfiguration(config) {
                styled.isTemplate = false
                return styled
            }
            return base
        }

        // Fallback CoreGraphics badge if SF Symbol lookup fails.
        let size = NSSize(width: 16, height: 16)
        let fallback = NSImage(size: size)
        fallback.lockFocus()
        spec.color.setFill()
        NSBezierPath(ovalIn: NSRect(origin: .zero, size: size)).fill()
        fallback.unlockFocus()
        fallback.accessibilityDescription = state.displayLabel
        return fallback
    }

    static func toolbarImage() -> NSImage {
        if let symbol = NSImage(systemSymbolName: "folder.badge.gearshape", accessibilityDescription: "FolderSync") {
            symbol.isTemplate = true
            return symbol
        }
        return NSImage(size: NSSize(width: 16, height: 16))
    }
}
