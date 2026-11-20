import Cocoa
import FinderSync
import os.log

@objc(FolderSyncFinderExtension)
final class FolderSyncFinderExtension: FIFinderSync {
    private let logger = Logger(subsystem: "dev.foldersync.app.finder-extension", category: "extension")
    private let stateLock = NSLock()

    private let sharedDirectoryURL: URL
    private let snapshotFileURL: URL
    private let actionsDirectoryURL: URL
    private let hostBundleIdentifier: String
    private let snapshotUpdatedNotification: String
    private let actionRequestedNotification: String

    private var cachedSnapshot: StatusSnapshot?
    private var lastEvaluation: SnapshotEvaluation = .stale(nil, reason: "Status unavailable")
    private var currentDirectoryURLs: Set<URL> = []
    private var observedRootURLs: Set<URL> = []
    private var refreshTimer: DispatchSourceTimer?

    override init() {
        let info = Bundle.main.infoDictionary ?? [:]
        let configuredSharedDir = (info["FolderSyncSharedDir"] as? String) ?? ""
        if configuredSharedDir.isEmpty {
            let realHome = Self.realUserHomePath()
            self.sharedDirectoryURL = URL(fileURLWithPath: realHome)
                .appendingPathComponent("Library/Application Support/FolderSync/finder", isDirectory: true)
        } else {
            self.sharedDirectoryURL = URL(fileURLWithPath: configuredSharedDir, isDirectory: true)
        }

        self.snapshotFileURL = self.sharedDirectoryURL.appendingPathComponent("status.json")
        self.actionsDirectoryURL = self.sharedDirectoryURL.appendingPathComponent("actions", isDirectory: true)

        if let configuredHost = info["FolderSyncHostBundleIdentifier"] as? String, !configuredHost.isEmpty {
            self.hostBundleIdentifier = configuredHost
        } else {
            self.hostBundleIdentifier = Self.derivedHostBundleIdentifier()
        }

        self.snapshotUpdatedNotification =
            (info["FolderSyncDarwinSnapshotNotification"] as? String)
            ?? "dev.foldersync.finder.snapshot-updated"
        self.actionRequestedNotification =
            (info["FolderSyncDarwinActionNotification"] as? String)
            ?? "dev.foldersync.finder.action-requested"

        super.init()

        registerBadges()
        reloadSnapshotAndRefresh(updateDirectoryURLs: true)
        subscribeToSnapshotNotifications()
        startStalenessTimer()

        logger.notice(
            "FolderSyncFinderExtension initialized sharedDir=\(self.sharedDirectoryURL.path, privacy: .public) host=\(self.hostBundleIdentifier, privacy: .public)"
        )
    }

    deinit {
        refreshTimer?.cancel()
        let center = CFNotificationCenterGetDarwinNotifyCenter()
        CFNotificationCenterRemoveObserver(center, Unmanaged.passUnretained(self).toOpaque(), nil, nil)
    }

    // MARK: - Setup

    private static func realUserHomePath() -> String {
        if let pw = getpwuid(getuid()), let dir = pw.pointee.pw_dir {
            return String(cString: dir)
        }
        return NSHomeDirectory()
    }

    private static func derivedHostBundleIdentifier() -> String {
        guard let extId = Bundle.main.bundleIdentifier else {
            return "dev.foldersync.app"
        }
        let parts = extId.split(separator: ".")
        if parts.count > 1 {
            return parts.dropLast().joined(separator: ".")
        }
        return extId
    }

    private func registerBadges() {
        let controller = FIFinderSyncController.default()
        for state in BadgeState.allCases {
            controller.setBadgeImage(
                BadgeRenderer.image(for: state),
                label: state.displayLabel,
                forBadgeIdentifier: state.rawValue
            )
        }
    }

    private func subscribeToSnapshotNotifications() {
        let center = CFNotificationCenterGetDarwinNotifyCenter()
        let observer = Unmanaged.passUnretained(self).toOpaque()
        CFNotificationCenterAddObserver(
            center,
            observer,
            { _, observerPtr, _, _, _ in
                guard let observerPtr else { return }
                let ext = Unmanaged<FolderSyncFinderExtension>.fromOpaque(observerPtr).takeUnretainedValue()
                DispatchQueue.main.async {
                    ext.reloadSnapshotAndRefresh(updateDirectoryURLs: true)
                }
            },
            snapshotUpdatedNotification as CFString,
            nil,
            .deliverImmediately
        )
    }

    private func startStalenessTimer() {
        let timer = DispatchSource.makeTimerSource(queue: DispatchQueue.main)
        timer.schedule(deadline: .now() + 30, repeating: 30, leeway: .seconds(5))
        timer.setEventHandler { [weak self] in
            self?.reloadSnapshotAndRefresh(updateDirectoryURLs: false)
        }
        timer.resume()
        refreshTimer = timer
    }

    private func isHostAppRunning() -> Bool {
        !NSRunningApplication.runningApplications(withBundleIdentifier: hostBundleIdentifier).isEmpty
    }

    private func reloadSnapshotAndRefresh(updateDirectoryURLs: Bool) {
        let loaded: StatusSnapshot?
        do {
            let data = try Data(contentsOf: snapshotFileURL)
            loaded = SnapshotEvaluator.decode(data: data)
            if loaded == nil {
                logger.warning("Snapshot JSON invalid or unsupported version at \(self.snapshotFileURL.path, privacy: .public)")
            }
        } catch {
            loaded = nil
            logger.notice("Snapshot unreadable at \(self.snapshotFileURL.path, privacy: .public): \(error.localizedDescription, privacy: .public)")
        }

        let evaluation = SnapshotEvaluator.evaluate(
            snapshot: loaded ?? cachedSnapshot,
            now: Date(),
            hostAppRunning: isHostAppRunning()
        )

        stateLock.lock()
        if let loaded {
            cachedSnapshot = loaded
        }
        lastEvaluation = evaluation
        let snapshotForRoots = evaluation.snapshot
        let previousDirectoryURLs = currentDirectoryURLs
        let existingObserved = observedRootURLs
        stateLock.unlock()

        let controller = FIFinderSyncController.default()
        if updateDirectoryURLs, let snapshotForRoots {
            // Do not probe arbitrary user paths with FileManager inside the app sandbox;
            // Finder resolves directoryURLs directly in the host process.
            let newRootURLs = Set(snapshotForRoots.mappings.map {
                URL(fileURLWithPath: SnapshotEvaluator.canonicalPath($0.rootPath))
            })
            if newRootURLs != previousDirectoryURLs {
                stateLock.lock()
                currentDirectoryURLs = newRootURLs
                stateLock.unlock()
                controller.directoryURLs = newRootURLs
                logger.notice("Updated directoryURLs count=\(newRootURLs.count)")
            }
        }

        // Per FinderSync.h: only push badge updates to URLs Finder has already requested.
        for rootURL in existingObserved {
            let badgeId = SnapshotEvaluator.badgeState(for: rootURL, evaluation: evaluation)?.rawValue ?? ""
            controller.setBadgeIdentifier(badgeId, for: rootURL)
            logger.notice("Updated badge root=\(rootURL.path, privacy: .public) badge=\(badgeId, privacy: .public)")
        }
    }

    // MARK: - FIFinderSync callbacks

    override func beginObservingDirectory(at url: URL) {
        logger.notice("beginObservingDirectory path=\(url.path, privacy: .public)")
    }

    override func endObservingDirectory(at url: URL) {
        logger.notice("endObservingDirectory path=\(url.path, privacy: .public)")
    }

    override func requestBadgeIdentifier(for url: URL) {
        // Re-evaluate freshness cheaply so stale status surfaces promptly.
        stateLock.lock()
        let snapshot = cachedSnapshot
        stateLock.unlock()

        let evaluation = SnapshotEvaluator.evaluate(
            snapshot: snapshot,
            now: Date(),
            hostAppRunning: isHostAppRunning()
        )

        stateLock.lock()
        lastEvaluation = evaluation
        if let badge = SnapshotEvaluator.badgeState(for: url, evaluation: evaluation) {
            observedRootURLs.insert(url)
            stateLock.unlock()
            FIFinderSyncController.default().setBadgeIdentifier(badge.rawValue, for: url)
            logger.notice("Badged root path=\(url.path, privacy: .public) badge=\(badge.rawValue, privacy: .public)")
        } else {
            stateLock.unlock()
            // v1 ruling: mapped-folder ROOTS only; clear badge on children.
            FIFinderSyncController.default().setBadgeIdentifier("", for: url)
        }
    }

    // MARK: - Toolbar and Context Menu

    override var toolbarItemName: String { "FolderSync" }
    override var toolbarItemToolTip: String { "FolderSync status and actions" }
    override var toolbarItemImage: NSImage { BadgeRenderer.toolbarImage() }

    override func menu(for menuKind: FIMenuKind) -> NSMenu? {
        let controller = FIFinderSyncController.default()
        let targetURL = controller.selectedItemURLs()?.first ?? controller.targetedURL()

        stateLock.lock()
        let snapshot = cachedSnapshot
        stateLock.unlock()

        let evaluation = SnapshotEvaluator.evaluate(
            snapshot: snapshot,
            now: Date(),
            hostAppRunning: isHostAppRunning()
        )

        let mapping: MappingSnapshot?
        if let targetURL, let snap = evaluation.snapshot {
            mapping = SnapshotEvaluator.owningMapping(for: targetURL, in: snap)
        } else {
            mapping = nil
        }

        let menu = NSMenu(title: "FolderSync")

        // Always communicate status in plain text, not color alone.
        let statusTitle: String
        switch evaluation {
        case .fresh:
            if let mapping {
                statusTitle = "Status: \(mapping.label)"
            } else {
                statusTitle = "FolderSync"
            }
        case .stale(_, let reason):
            statusTitle = "Status: \(reason)"
        }
        let statusItem = NSMenuItem(title: statusTitle, action: nil, keyEquivalent: "")
        statusItem.isEnabled = false
        menu.addItem(statusItem)
        menu.addItem(NSMenuItem.separator())

        let hasMapping = mapping != nil
        let syncItem = NSMenuItem(title: "Sync now", action: #selector(handleSyncNow(_:)), keyEquivalent: "")
        syncItem.target = self
        syncItem.representedObject = mapping?.id
        syncItem.isEnabled = hasMapping
        menu.addItem(syncItem)

        let pauseItem = NSMenuItem(title: "Pause", action: #selector(handlePause(_:)), keyEquivalent: "")
        pauseItem.target = self
        pauseItem.representedObject = mapping?.id
        pauseItem.isEnabled = hasMapping
        menu.addItem(pauseItem)

        let openItem = NSMenuItem(title: "Open FolderSync", action: #selector(handleOpenApp(_:)), keyEquivalent: "")
        openItem.target = self
        openItem.representedObject = mapping?.id
        openItem.isEnabled = true
        menu.addItem(openItem)

        let lastRunItem = NSMenuItem(title: "Show last run", action: #selector(handleShowLastRun(_:)), keyEquivalent: "")
        lastRunItem.target = self
        lastRunItem.representedObject = mapping?.id
        lastRunItem.isEnabled = hasMapping
        menu.addItem(lastRunItem)

        return menu
    }

    // MARK: - Narrow Action Dispatch (no credentials, no rclone)

    @objc private func handleSyncNow(_ sender: NSMenuItem) {
        dispatchAction(.syncNow, mappingId: sender.representedObject as? String, wakeHostIfNeeded: false)
    }

    @objc private func handlePause(_ sender: NSMenuItem) {
        dispatchAction(.pause, mappingId: sender.representedObject as? String, wakeHostIfNeeded: false)
    }

    @objc private func handleOpenApp(_ sender: NSMenuItem) {
        dispatchAction(.openApp, mappingId: sender.representedObject as? String, wakeHostIfNeeded: true)
    }

    @objc private func handleShowLastRun(_ sender: NSMenuItem) {
        dispatchAction(.showLastRun, mappingId: sender.representedObject as? String, wakeHostIfNeeded: true)
    }

    private func dispatchAction(_ action: ActionKind, mappingId: String?, wakeHostIfNeeded: Bool) {
        let request = ActionRequest(
            version: snapshotSchemaVersion,
            id: UUID().uuidString,
            requestedAt: SnapshotEvaluator.formatISO8601(Date()),
            action: action,
            mappingId: mappingId
        )

        do {
            let written = try SnapshotEvaluator.writeActionRequest(request, toDirectory: actionsDirectoryURL)
            let center = CFNotificationCenterGetDarwinNotifyCenter()
            CFNotificationCenterPostNotification(
                center,
                CFNotificationName(actionRequestedNotification as CFString),
                nil,
                nil,
                true
            )
            logger.notice("Dispatched action=\(action.rawValue, privacy: .public) mapping=\(mappingId ?? "none", privacy: .public) file=\(written.lastPathComponent, privacy: .public)")
        } catch {
            logger.error("Failed to write action request: \(error.localizedDescription, privacy: .public)")
        }

        if wakeHostIfNeeded && !isHostAppRunning() {
            let hostAppURL = Bundle.main.bundleURL
                .deletingLastPathComponent() // PlugIns
                .deletingLastPathComponent() // Contents
                .deletingLastPathComponent() // *.app
            let config = NSWorkspace.OpenConfiguration()
            config.activates = true
            NSWorkspace.shared.openApplication(at: hostAppURL, configuration: config) { _, error in
                if let error {
                    self.logger.notice("openApplication fallback failed: \(error.localizedDescription, privacy: .public)")
                }
            }
        }
    }
}
