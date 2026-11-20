import Cocoa
import FinderSync
import os.log

final class HostAppDelegate: NSObject, NSApplicationDelegate {
    private let logger = Logger(subsystem: "dev.foldersync.app", category: "host")
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
        record("HOST_READY extension_enabled=\(enabled) shared_dir=\(sharedDirectoryURL.path)")

        let center = CFNotificationCenterGetDarwinNotifyCenter()
        let observer = Unmanaged.passUnretained(self).toOpaque()
        CFNotificationCenterAddObserver(
            center,
            observer,
            { _, observerPtr, _, _, _ in
                guard let observerPtr else { return }
                let delegate = Unmanaged<HostAppDelegate>.fromOpaque(observerPtr).takeUnretainedValue()
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
               let req = try? JSONDecoder().decode(ActionRequest.self, from: data) {
                record("ACTION_RECEIVED action=\(req.action.rawValue) mapping_id=\(req.mappingId ?? "none") id=\(req.id)")
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
                ?? "dev.foldersync.finder.snapshot-updated"
            CFNotificationCenterPostNotification(
                CFNotificationCenterGetDarwinNotifyCenter(),
                CFNotificationName(notif as CFString),
                nil,
                nil,
                true
            )
            return
        }

        let sharedDirPath = (info["FolderSyncSharedDir"] as? String)
            ?? FileManager.default.temporaryDirectory.appendingPathComponent("foldersync-finder-shared").path
        let actionNotif = (info["FolderSyncDarwinActionNotification"] as? String)
            ?? "dev.foldersync.finder.action-requested"
        let actionLogPath = ProcessInfo.processInfo.environment["FOLDERSYNC_HOST_LOG"]

        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)
        let delegate = HostAppDelegate(
            sharedDirectoryURL: URL(fileURLWithPath: sharedDirPath, isDirectory: true),
            actionRequestedNotification: actionNotif,
            actionLogPath: actionLogPath
        )
        app.delegate = delegate
        app.run()
    }
}
