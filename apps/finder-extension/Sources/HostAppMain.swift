import Cocoa
import FinderSync
import os.log

/// Throwaway mock host app for the Finder Sync feasibility spike.
///
/// Unlike the future v1 Rust manager (which will validate, dispatch manager
/// commands, and record runs), this mock host only validates incoming action
/// JSON files against the v1 action contract, logs them, and deletes them.
final class MockHostAppDelegate: NSObject, NSApplicationDelegate {
    private let logger = Logger(subsystem: "dev.foldersync.app", category: "mock-host")
    private let sharedDirectoryURL: URL
    private let actionsDirectoryURL: URL
    private let actionRequestedNotification: String
    private let actionLogPath: String?

    init(sharedDirectoryURL: URL, actionRequestedNotification: String, actionLogPath: String?) {
        self.sharedDirectoryURL = sharedDirectoryURL
        self.actionsDirectoryURL = sharedDirectoryURL.appendingPathComponent("actions", isDirectory: true)
        self.actionRequestedNotification = actionRequestedNotification
        self.actionLogPath = actionLogPath
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let enabled = FIFinderSyncController.isExtensionEnabled
        record("MOCK_HOST_READY extension_enabled=\(enabled) shared_dir=\(sharedDirectoryURL.path)")

        let center = CFNotificationCenterGetDarwinNotifyCenter()
        let observer = Unmanaged.passUnretained(self).toOpaque()
        CFNotificationCenterAddObserver(
            center,
            observer,
            { _, observerPtr, _, _, _ in
                guard let observerPtr else { return }
                let delegate = Unmanaged<MockHostAppDelegate>.fromOpaque(observerPtr).takeUnretainedValue()
                DispatchQueue.main.async {
                    delegate.drainPendingActions()
                }
            },
            actionRequestedNotification as CFString,
            nil,
            .deliverImmediately
        )

        drainPendingActions()
    }

    /// Mock-only drain: validates against the v1 action contract, logs, and deletes
    /// each queued action file without executing any sync operation.
    private func drainPendingActions() {
        let fm = FileManager.default
        guard let files = try? fm.contentsOfDirectory(
            at: actionsDirectoryURL,
            includingPropertiesForKeys: nil,
            options: [.skipsHiddenFiles]
        ) else {
            return
        }

        for fileURL in files.sorted(by: { $0.lastPathComponent < $1.lastPathComponent }) where fileURL.pathExtension == "json" {
            if let data = try? Data(contentsOf: fileURL),
               let req = SnapshotEvaluator.decodeValidActionRequest(data: data) {
                record("MOCK_ACTION_RECEIVED action=\(req.action.rawValue) mapping_id=\(req.mappingId ?? "none") id=\(req.id)")
            } else {
                record("MOCK_ACTION_REJECTED file=\(fileURL.lastPathComponent)")
            }
            try? fm.removeItem(at: fileURL)
        }
    }

    private func record(_ line: String) {
        logger.notice("\(line, privacy: .public)")
        guard let actionLogPath else { return }
        let entry = line + "\n"
        if let handle = FileHandle(forWritingAtPath: actionLogPath) {
            handle.seekToEndOfFile()
            handle.write(Data(entry.utf8))
            try? handle.close()
        } else {
            try? entry.write(toFile: actionLogPath, atomically: true, encoding: .utf8)
        }
    }
}

@main
struct FolderSyncHostMain {
    static func main() {
        let info = Bundle.main.infoDictionary ?? [:]
        let args = CommandLine.arguments

        // Optional utility mode: notify the running Finder extension that status.json changed.
        if args.contains("--notify-snapshot") {
            let notif = (info["FolderSyncDarwinSnapshotNotification"] as? String)
                ?? darwinSnapshotUpdatedNotification
            CFNotificationCenterPostNotification(
                CFNotificationCenterGetDarwinNotifyCenter(),
                CFNotificationName(notif as CFString),
                nil,
                nil,
                true
            )
            return
        }

        let sharedDirURL: URL
        if let configured = info["FolderSyncSharedDir"] as? String, !configured.isEmpty {
            sharedDirURL = URL(fileURLWithPath: configured, isDirectory: true)
        } else {
            sharedDirURL = SnapshotEvaluator.defaultSpikeSharedDirectoryURL()
        }

        let actionNotif = (info["FolderSyncDarwinActionNotification"] as? String)
            ?? darwinActionRequestedNotification
        let actionLogPath = ProcessInfo.processInfo.environment["FOLDERSYNC_HOST_LOG"]

        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)
        let delegate = MockHostAppDelegate(
            sharedDirectoryURL: sharedDirURL,
            actionRequestedNotification: actionNotif,
            actionLogPath: actionLogPath
        )
        app.delegate = delegate
        app.run()
    }
}
