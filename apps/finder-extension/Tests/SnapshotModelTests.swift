import Foundation

private var passed = 0
private var failed = 0

private func expect(_ condition: @autoclosure () -> Bool, _ message: String, file: String = #file, line: Int = #line) {
    if condition() {
        passed += 1
    } else {
        failed += 1
        fputs("FAIL [\(file):\(line)]: \(message)\n", stderr)
    }
}

private func parseISO(_ value: String) -> Date {
    let f = ISO8601DateFormatter()
    f.formatOptions = [.withInternetDateTime]
    guard let d = f.date(from: value) else {
        fatalError("Invalid test timestamp: \(value)")
    }
    return d
}

@main
struct SnapshotModelTestsMain {
    static func main() {
// 1. Valid v1 snapshot decodes all 5 v1 states
do {
    let json = """
    {
      "version": 1,
      "generated_at": "2026-11-20T18:00:00Z",
      "stale_after_seconds": 300,
      "mappings": [
        {"id": "m1", "root_path": "/Volumes/WorkSSD/Research", "state": "synced", "label": "Synced · 17:58"},
        {"id": "m2", "root_path": "/Volumes/WorkSSD/Family", "state": "syncing", "label": "Syncing…"},
        {"id": "m3", "root_path": "/Volumes/WorkSSD/Docs", "state": "attention", "label": "Needs attention"},
        {"id": "m4", "root_path": "/Volumes/WorkSSD/Archive", "state": "paused", "label": "Paused"},
        {"id": "m5", "root_path": "/Volumes/PhotoSSD/Photos", "state": "disk_missing", "label": "Waiting for PhotoSSD"}
      ]
    }
    """
    let snap = SnapshotEvaluator.decode(data: Data(json.utf8))
    expect(snap != nil, "valid v1 snapshot should decode")
    expect(snap?.mappings.count == 5, "should decode all 5 mappings")
    expect(snap?.mappings[0].state == .synced, "m1 state should be synced")
    expect(snap?.mappings[1].state == .syncing, "m2 state should be syncing")
    expect(snap?.mappings[2].state == .attention, "m3 state should be attention")
    expect(snap?.mappings[3].state == .paused, "m4 state should be paused")
    expect(snap?.mappings[4].state == .diskMissing, "m5 state should be disk_missing")
}

// 2. Unsupported schema version or corrupt JSON rejected
do {
    let v2 = """
    {"version": 2, "generated_at": "2026-11-20T18:00:00Z", "mappings": []}
    """
    expect(SnapshotEvaluator.decode(data: Data(v2.utf8)) == nil, "unsupported version 2 should return nil")
    expect(SnapshotEvaluator.decode(data: Data("{not json".utf8)) == nil, "corrupt JSON should return nil")
}

// 3. Staleness evaluation: fresh vs expired vs host not running vs nil
do {
    let json = """
    {
      "version": 1,
      "generated_at": "2026-11-20T18:00:00Z",
      "stale_after_seconds": 300,
      "mappings": [
        {"id": "m1", "root_path": "/Volumes/WorkSSD/Research", "state": "synced", "label": "Synced"}
      ]
    }
    """
    let snap = SnapshotEvaluator.decode(data: Data(json.utf8))
    let freshEval = SnapshotEvaluator.evaluate(snapshot: snap, now: parseISO("2026-11-20T18:04:59Z"), hostAppRunning: true)
    if case .fresh = freshEval {
        passed += 1
    } else {
        failed += 1
        fputs("FAIL: snapshot 299s old with running host should be fresh\n", stderr)
    }

    let expiredEval = SnapshotEvaluator.evaluate(snapshot: snap, now: parseISO("2026-11-20T18:05:01Z"), hostAppRunning: true)
    if case .stale(let s, _, let readOk) = expiredEval, s != nil, readOk {
        passed += 1
    } else {
        failed += 1
        fputs("FAIL: snapshot 301s old should be stale with readSucceeded=true\n", stderr)
    }

    let hostDeadEval = SnapshotEvaluator.evaluate(snapshot: snap, now: parseISO("2026-11-20T18:00:10Z"), hostAppRunning: false)
    if case .stale(let s, _, let readOk) = hostDeadEval, s != nil, readOk {
        passed += 1
    } else {
        failed += 1
        fputs("FAIL: snapshot when host app is not running should be stale with readSucceeded=true\n", stderr)
    }

    let nilEval = SnapshotEvaluator.evaluate(snapshot: nil, now: parseISO("2026-11-20T18:00:10Z"), hostAppRunning: true)
    if case .stale(nil, _, let readOk) = nilEval, !readOk {
        passed += 1
    } else {
        failed += 1
        fputs("FAIL: nil snapshot should evaluate to stale(nil, readSucceeded: false)\n", stderr)
    }
}

// 3b. Valid-to-invalid transition: failed read keeps cached snapshot ONLY for stale root badges;
// menu mappings and directoryURLs updates come strictly from successful reads.
do {
    let tempRoot = FileManager.default.temporaryDirectory.appendingPathComponent("fs-transition-\(UUID().uuidString)")
    let mappedDir = tempRoot.appendingPathComponent("Research")
    let childFile = mappedDir.appendingPathComponent("doc.txt")
    try! FileManager.default.createDirectory(at: mappedDir, withIntermediateDirectories: true)
    try! Data("hi".utf8).write(to: childFile)
    defer { try? FileManager.default.removeItem(at: tempRoot) }

    let priorSnap = StatusSnapshot(
        version: 1,
        generatedAt: "2026-11-20T18:00:00Z",
        staleAfterSeconds: 300,
        mappings: [
            MappingSnapshot(id: "research", rootPath: mappedDir.path, state: .synced, label: "Synced")
        ]
    )
    let stillFreshNow = parseISO("2026-11-20T18:00:30Z")
    let expiredNow = parseISO("2026-11-20T18:10:00Z")

    // Initially valid & fresh -> synced badge, menu mapping present, directoryURLs present
    let initialEval = SnapshotEvaluator.evaluate(
        readResult: .valid(priorSnap),
        lastKnownSnapshot: priorSnap,
        now: stillFreshNow,
        hostAppRunning: true
    )
    expect(SnapshotEvaluator.badgeState(for: mappedDir, evaluation: initialEval) == .synced, "initial valid snapshot should badge synced")
    expect(SnapshotEvaluator.menuMapping(for: mappedDir, evaluation: initialEval)?.id == "research", "initial valid snapshot should supply menu mapping")
    expect(SnapshotEvaluator.directoryRootURLs(for: initialEval)?.count == 1, "initial valid snapshot should supply directoryURLs")

    // Next read fails (deleted / corrupt / unsupported version) while host runs and prior TTL hasn't elapsed
    let failedEval = SnapshotEvaluator.evaluate(
        readResult: .failed(reason: "Status unavailable (invalid snapshot)"),
        lastKnownSnapshot: priorSnap,
        now: stillFreshNow,
        hostAppRunning: true
    )
    if case .stale(let retained, let reason, let readOk) = failedEval {
        expect(retained == priorSnap, "failed read must retain lastKnownSnapshot only for stale root badges")
        expect(reason == "Status unavailable (invalid snapshot)", "failed read reason should propagate to menu")
        expect(!readOk, "failed read must mark readSucceeded=false")
    } else {
        failed += 1
        fputs("FAIL: failed snapshot read must NOT evaluate as fresh even when prior snapshot is within TTL\n", stderr)
    }
    expect(SnapshotEvaluator.badgeState(for: mappedDir, evaluation: failedEval) == .stale, "valid-to-invalid transition must badge mapped root as stale")
    expect(SnapshotEvaluator.badgeState(for: childFile, evaluation: failedEval) == nil, "valid-to-invalid transition must still NOT badge child files")
    expect(SnapshotEvaluator.menuMapping(for: mappedDir, evaluation: failedEval) == nil, "failed read must supply NO mapping to Finder menu (only Open FolderSync without mapping ID)")
    expect(SnapshotEvaluator.directoryRootURLs(for: failedEval) == nil, "failed read must NOT supply directoryURLs updates")

    // Contrast: a valid-but-expired read still succeeded, so menuMapping and directoryURLs come from that successful read
    let validExpiredEval = SnapshotEvaluator.evaluate(
        readResult: .valid(priorSnap),
        lastKnownSnapshot: priorSnap,
        now: expiredNow,
        hostAppRunning: true
    )
    expect(SnapshotEvaluator.badgeState(for: mappedDir, evaluation: validExpiredEval) == .stale, "expired valid snapshot should badge root as stale")
    expect(SnapshotEvaluator.menuMapping(for: mappedDir, evaluation: validExpiredEval)?.id == "research", "successful expired read may supply menu mapping")
    expect(SnapshotEvaluator.directoryRootURLs(for: validExpiredEval)?.count == 1, "successful expired read may supply directoryURLs")
}

// 4. Root-only badge rule: mapped root gets badge; child file/subfolder gets nil (no badge)
do {
    let tempRoot = FileManager.default.temporaryDirectory.appendingPathComponent("fs-test-\(UUID().uuidString)")
    let mappedDir = tempRoot.appendingPathComponent("Research")
    let nestedDir = mappedDir.appendingPathComponent("Subfolder")
    let nestedFile = nestedDir.appendingPathComponent("paper.pdf")
    let siblingDir = tempRoot.appendingPathComponent("Research-Extra")
    try! FileManager.default.createDirectory(at: nestedDir, withIntermediateDirectories: true)
    try! FileManager.default.createDirectory(at: siblingDir, withIntermediateDirectories: true)
    try! Data("pdf".utf8).write(to: nestedFile)
    defer { try? FileManager.default.removeItem(at: tempRoot) }

    let snap = StatusSnapshot(
        version: 1,
        generatedAt: "2026-11-20T18:00:00Z",
        staleAfterSeconds: 300,
        mappings: [
            MappingSnapshot(id: "research", rootPath: mappedDir.path, state: .attention, label: "Needs attention", lastRunSummary: nil)
        ]
    )
    let freshEval = SnapshotEvaluation.fresh(snap)
    let staleEval = SnapshotEvaluation.stale(snap, reason: "Outdated", readSucceeded: true)

    // Resolve /var -> /private/var to verify symlink equivalence
    let resolvedRoot = mappedDir.resolvingSymlinksInPath()
    expect(SnapshotEvaluator.badgeState(for: mappedDir, evaluation: freshEval) == .attention, "mapped root should get attention badge")
    expect(SnapshotEvaluator.badgeState(for: resolvedRoot, evaluation: freshEval) == .attention, "resolved symlink mapped root should match")
    expect(SnapshotEvaluator.badgeState(for: nestedDir, evaluation: freshEval) == nil, "nested subfolder must NOT get a badge (roots only)")
    expect(SnapshotEvaluator.badgeState(for: nestedFile, evaluation: freshEval) == nil, "nested file must NOT get a badge (roots only)")
    expect(SnapshotEvaluator.badgeState(for: siblingDir, evaluation: freshEval) == nil, "sibling prefix-sharing dir must NOT match")
    expect(SnapshotEvaluator.badgeState(for: mappedDir, evaluation: staleEval) == .stale, "stale snapshot must badge mapped root as stale")
    expect(SnapshotEvaluator.badgeState(for: nestedFile, evaluation: staleEval) == nil, "stale snapshot must still NOT badge child files")

    // Context menu owningMapping matches root AND descendants, but NOT prefix-sharing sibling
    expect(SnapshotEvaluator.owningMapping(for: mappedDir, in: snap)?.id == "research", "owningMapping should match root")
    expect(SnapshotEvaluator.owningMapping(for: nestedFile, in: snap)?.id == "research", "owningMapping should match child for context menu")
    expect(SnapshotEvaluator.owningMapping(for: siblingDir, in: snap) == nil, "owningMapping must NOT match sibling Research-Extra")
}

// 5. Accessible labels are non-empty plain text for all BadgeStates
do {
    let all: [BadgeState] = [.synced, .syncing, .attention, .paused, .diskMissing, .stale]
    for state in all {
        expect(!state.displayLabel.isEmpty, "badge state \(state.rawValue) must have non-empty accessible label")
    }
}

// 6. Action contract enforced at construction, serialization, and decoding (only open_app may omit mapping_id)
do {
    let tempDir = FileManager.default.temporaryDirectory.appendingPathComponent("fs-act-\(UUID().uuidString)")
    try! FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: tempDir) }

    // Valid mapping-scoped action
    let req = ActionRequest(
        version: 1,
        id: "req-123",
        requestedAt: "2026-11-20T18:01:00Z",
        action: .syncNow,
        mappingId: "research"
    )
    expect(req != nil, "sync_now with mapping_id should construct")
    if let req {
        do {
            let writtenURL = try SnapshotEvaluator.writeActionRequest(req, toDirectory: tempDir)
            let data = try Data(contentsOf: writtenURL)
            let decoded = SnapshotEvaluator.decodeValidActionRequest(data: data)
            expect(decoded == req, "valid action request should round-trip via decodeValidActionRequest")
            expect(writtenURL.pathExtension == "json", "written action file should be .json")
        } catch {
            failed += 1
            fputs("FAIL: writeActionRequest threw error for valid sync_now: \(error)\n", stderr)
        }
    }

    // Valid open_app without mapping_id
    let openReq = ActionRequest(
        version: 1,
        id: "req-open",
        requestedAt: "2026-11-20T18:01:05Z",
        action: .openApp,
        mappingId: nil
    )
    expect(openReq != nil, "open_app without mapping_id should construct")
    if let openReq {
        do {
            let writtenURL = try SnapshotEvaluator.writeActionRequest(openReq, toDirectory: tempDir)
            let data = try Data(contentsOf: writtenURL)
            expect(SnapshotEvaluator.decodeValidActionRequest(data: data) == openReq, "open_app without mapping_id should decode as valid")
        } catch {
            failed += 1
            fputs("FAIL: writeActionRequest threw error for valid open_app: \(error)\n", stderr)
        }
    }

    // Mapping-required actions without mapping_id (or wrong version) CANNOT be constructed
    let invalidCtorCases: [(Int, ActionKind, String?)] = [
        (1, .syncNow, nil),
        (1, .syncNow, "   "),
        (1, .pause, nil),
        (1, .showLastRun, nil),
        (1, .showLastRun, ""),
        (2, .openApp, nil),
    ]
    for (idx, (ver, kind, badMapping)) in invalidCtorCases.enumerated() {
        let constructed = ActionRequest(
            version: ver,
            id: "req-bad-\(idx)",
            requestedAt: "2026-11-20T18:01:10Z",
            action: kind,
            mappingId: badMapping
        )
        expect(constructed == nil, "ActionRequest init must reject v\(ver) \(kind.rawValue) with invalid mapping_id")
    }

    // Raw JSON payloads violating the contract must fail both JSONDecoder and decodeValidActionRequest
    let invalidRawPayloads: [String] = [
        #"{"version":1,"id":"r1","requested_at":"2026-11-20T18:01:10Z","action":"sync_now","mapping_id":null}"#,
        #"{"version":1,"id":"r2","requested_at":"2026-11-20T18:01:10Z","action":"sync_now","mapping_id":"  "}"#,
        #"{"version":1,"id":"r3","requested_at":"2026-11-20T18:01:10Z","action":"pause"}"#,
        #"{"version":1,"id":"r4","requested_at":"2026-11-20T18:01:10Z","action":"show_last_run","mapping_id":""}"#,
        #"{"version":2,"id":"r5","requested_at":"2026-11-20T18:01:10Z","action":"open_app"}"#,
    ]
    for (idx, raw) in invalidRawPayloads.enumerated() {
        let data = Data(raw.utf8)
        let directDecode = try? JSONDecoder().decode(ActionRequest.self, from: data)
        expect(directDecode == nil, "JSONDecoder must reject invalid action payload #\(idx)")
        expect(SnapshotEvaluator.decodeValidActionRequest(data: data) == nil, "decodeValidActionRequest must reject invalid payload #\(idx)")
    }
}

// 7. Production bundle IDs, Darwin notification names, and <state dir>/finder resolution
do {
    expect(productionHostBundleIdentifier == "dev.foldersync.app", "production host bundle ID must be dev.foldersync.app")
    expect(productionExtensionBundleIdentifier == "dev.foldersync.app.finder-extension", "production extension bundle ID must be dev.foldersync.app.finder-extension")
    expect(darwinSnapshotUpdatedNotification == "dev.foldersync.finder.snapshot-updated", "snapshot Darwin notification must match ADR 0002")
    expect(darwinActionRequestedNotification == "dev.foldersync.finder.action-requested", "action Darwin notification must match ADR 0002")

    let fakeHome = URL(fileURLWithPath: "/tmp/foldersync-fake-home", isDirectory: true)
    let defaultResolved = SnapshotEvaluator.resolveSharedDirectoryURL(
        configuredAbsolutePath: nil,
        configuredHomeRelativePath: nil,
        realHomeDirectory: fakeHome
    )
    expect(
        defaultResolved.path == "/tmp/foldersync-fake-home/Library/Application Support/FolderSync/finder",
        "default shared dir must resolve to <home>/Library/Application Support/FolderSync/finder, got \(defaultResolved.path)"
    )

    let leadingSlashResolved = SnapshotEvaluator.resolveSharedDirectoryURL(
        configuredAbsolutePath: "   ",
        configuredHomeRelativePath: "/Library/Application Support/FolderSync/finder/",
        realHomeDirectory: fakeHome
    )
    expect(
        leadingSlashResolved.path == "/tmp/foldersync-fake-home/Library/Application Support/FolderSync/finder",
        "leading/trailing slashes in home-relative path must normalize cleanly"
    )

    let traversalFallback = SnapshotEvaluator.resolveSharedDirectoryURL(
        configuredAbsolutePath: nil,
        configuredHomeRelativePath: "../../etc/passwd",
        realHomeDirectory: fakeHome
    )
    expect(
        traversalFallback.path == "/tmp/foldersync-fake-home/Library/Application Support/FolderSync/finder",
        "traversal in home-relative path must fall back to safe default"
    )

    let absOverride = SnapshotEvaluator.resolveSharedDirectoryURL(
        configuredAbsolutePath: "/tmp/custom-finder-handoff",
        configuredHomeRelativePath: "Library/Application Support/FolderSync/finder",
        realHomeDirectory: fakeHome
    )
    expect(
        absOverride.path == "/tmp/custom-finder-handoff",
        "explicit FolderSyncSharedDir must override home-relative path"
    )
}

// 8. Symlink refusal on snapshot reads and action writes, plus safe action ID enforcement
do {
    let tempRoot = FileManager.default.temporaryDirectory.appendingPathComponent("fs-sec-\(UUID().uuidString)")
    let finderDir = tempRoot.appendingPathComponent("finder", isDirectory: true)
    let outsideDir = tempRoot.appendingPathComponent("outside", isDirectory: true)
    try! FileManager.default.createDirectory(at: finderDir, withIntermediateDirectories: true)
    try! FileManager.default.createDirectory(at: outsideDir, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: tempRoot) }

    // Valid status.json reads cleanly via readSnapshotData
    let validStatus = """
    {"version":1,"generated_at":"2026-11-20T18:00:00Z","stale_after_seconds":300,"mappings":[]}
    """
    let statusFile = finderDir.appendingPathComponent("status.json")
    try! Data(validStatus.utf8).write(to: statusFile)
    if case .valid(let decoded) = SnapshotEvaluator.readSnapshotData(at: statusFile) {
        expect(decoded.version == 1, "regular status.json should read cleanly")
    } else {
        failed += 1
        fputs("FAIL: regular status.json should decode cleanly via readSnapshotData\n", stderr)
    }

    // Symlinked status.json must be refused
    let outsideStatus = outsideDir.appendingPathComponent("real-status.json")
    try! Data(validStatus.utf8).write(to: outsideStatus)
    try! FileManager.default.removeItem(at: statusFile)
    try! FileManager.default.createSymbolicLink(at: statusFile, withDestinationURL: outsideStatus)
    if case .failed(let reason) = SnapshotEvaluator.readSnapshotData(at: statusFile) {
        expect(reason.contains("symlink"), "symlinked status.json must be refused with symlink reason, got \(reason)")
    } else {
        failed += 1
        fputs("FAIL: symlinked status.json must be refused\n", stderr)
    }

    // Unsafe action IDs (path traversal / slashes) must be rejected by writeActionRequest
    let actionsDir = finderDir.appendingPathComponent("actions", isDirectory: true)
    for badId in ["../escape", "nested/id", "", "-leading-dash", "has space"] {
        expect(!SnapshotEvaluator.isSafePathComponentId(badId), "unsafe id '\(badId)' must fail isSafePathComponentId")
        if let badReq = ActionRequest(
            version: 1,
            id: badId,
            requestedAt: "2026-11-20T18:01:00Z",
            action: .openApp,
            mappingId: nil
        ) {
            do {
                _ = try SnapshotEvaluator.writeActionRequest(badReq, toDirectory: actionsDir)
                failed += 1
                fputs("FAIL: writeActionRequest must reject unsafe id '\(badId)'\n", stderr)
            } catch ActionValidationError.unsafeIdentifier {
                passed += 1
            } catch {
                failed += 1
                fputs("FAIL: expected unsafeIdentifier for '\(badId)', got \(error)\n", stderr)
            }
        }
    }

    // Symlinked actions directory must be refused by writeActionRequest
    let symlinkActions = finderDir.appendingPathComponent("actions-link", isDirectory: true)
    try! FileManager.default.createSymbolicLink(at: symlinkActions, withDestinationURL: outsideDir)
    let goodReq = ActionRequest(
        version: 1,
        id: "safe-action-1",
        requestedAt: "2026-11-20T18:01:00Z",
        action: .openApp,
        mappingId: nil
    )!
    do {
        _ = try SnapshotEvaluator.writeActionRequest(goodReq, toDirectory: symlinkActions)
        failed += 1
        fputs("FAIL: writeActionRequest must refuse symlinked actions directory\n", stderr)
    } catch ActionValidationError.symlinkRefused {
        passed += 1
    } catch {
        failed += 1
        fputs("FAIL: expected symlinkRefused for symlinked actions dir, got \(error)\n", stderr)
    }
}

// 9. Ancestor symlink refusal from state root down (Fix 5) and FIFO non-blocking refusal (Fix 6)
do {
    let tempRoot = FileManager.default.temporaryDirectory.appendingPathComponent("fs-fdsec-\(UUID().uuidString)")
    let outsideState = tempRoot.appendingPathComponent("outside-state", isDirectory: true)
    let outsideFinder = outsideState.appendingPathComponent("finder", isDirectory: true)
    try! FileManager.default.createDirectory(at: outsideFinder, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: tempRoot) }

    let validStatus = """
    {"version":1,"generated_at":"2026-11-20T18:00:00Z","stale_after_seconds":300,"mappings":[]}
    """
    try! Data(validStatus.utf8).write(to: outsideFinder.appendingPathComponent("status.json"))

    // 9a. Symlinked state root pointing to an outside dir with a valid regular finder/status.json
    let symlinkStateRoot = tempRoot.appendingPathComponent("state-link", isDirectory: true)
    try! FileManager.default.createSymbolicLink(at: symlinkStateRoot, withDestinationURL: outsideState)
    let symlinkedHandoff = SnapshotEvaluator.handoffLocation(
        parentDirectory: tempRoot,
        relativeComponents: ["state-link", "finder"]
    )
    if case .failed(let reason) = SnapshotEvaluator.readSnapshotData(location: symlinkedHandoff) {
        expect(reason.contains("symlink"), "symlinked state root must be refused on read, got \(reason)")
    } else {
        failed += 1
        fputs("FAIL: readSnapshotData must refuse symlinked state root ancestor\n", stderr)
    }

    let actionReq = ActionRequest(
        version: 1,
        id: "safe-ancestor-1",
        requestedAt: "2026-11-20T18:01:00Z",
        action: .openApp,
        mappingId: nil
    )!
    do {
        _ = try SnapshotEvaluator.writeActionRequest(actionReq, toLocation: symlinkedHandoff)
        failed += 1
        fputs("FAIL: writeActionRequest must refuse symlinked state root ancestor\n", stderr)
    } catch ActionValidationError.symlinkRefused {
        passed += 1
    } catch {
        failed += 1
        fputs("FAIL: expected symlinkRefused for symlinked state root, got \(error)\n", stderr)
    }

    // 9b. Real state root with symlinked finder/ subdirectory
    let realState = tempRoot.appendingPathComponent("real-state", isDirectory: true)
    try! FileManager.default.createDirectory(at: realState, withIntermediateDirectories: true)
    let symlinkFinder = realState.appendingPathComponent("finder", isDirectory: true)
    try! FileManager.default.createSymbolicLink(at: symlinkFinder, withDestinationURL: outsideFinder)
    let symlinkFinderLoc = SnapshotEvaluator.handoffLocation(
        parentDirectory: tempRoot,
        relativeComponents: ["real-state", "finder"]
    )
    if case .failed(let reason) = SnapshotEvaluator.readSnapshotData(location: symlinkFinderLoc) {
        expect(reason.contains("symlink"), "symlinked finder dir must be refused on read, got \(reason)")
    } else {
        failed += 1
        fputs("FAIL: readSnapshotData must refuse symlinked finder dir\n", stderr)
    }
    do {
        _ = try SnapshotEvaluator.writeActionRequest(actionReq, toLocation: symlinkFinderLoc)
        failed += 1
        fputs("FAIL: writeActionRequest must refuse symlinked finder dir\n", stderr)
    } catch ActionValidationError.symlinkRefused {
        passed += 1
    } catch {
        failed += 1
        fputs("FAIL: expected symlinkRefused for symlinked finder dir, got \(error)\n", stderr)
    }

    // 9c. FIFO at status.json with NO writer must NOT block and must evaluate as stale/failed
    let fifoState = tempRoot.appendingPathComponent("fifo-state", isDirectory: true)
    let fifoFinder = fifoState.appendingPathComponent("finder", isDirectory: true)
    try! FileManager.default.createDirectory(at: fifoFinder, withIntermediateDirectories: true)
    let fifoPath = fifoFinder.appendingPathComponent("status.json").path
    let mkfifoRc = Darwin.mkfifo(fifoPath, 0o600)
    expect(mkfifoRc == 0, "mkfifo should create test FIFO")
    let fifoLoc = SnapshotEvaluator.handoffLocation(
        parentDirectory: tempRoot,
        relativeComponents: ["fifo-state", "finder"]
    )
    let readStart = Date()
    let fifoRead = SnapshotEvaluator.readSnapshotData(location: fifoLoc)
    let elapsed = Date().timeIntervalSince(readStart)
    expect(elapsed < 2.0, "FIFO read must return immediately without blocking (elapsed=\(elapsed)s)")
    if case .failed(let reason) = fifoRead {
        expect(reason.contains("non-regular") || reason.contains("unavailable"), "FIFO must fail as non-regular/unavailable, got \(reason)")
    } else {
        failed += 1
        fputs("FAIL: FIFO status.json must NOT decode as valid\n", stderr)
    }
    let fifoEval = SnapshotEvaluator.evaluate(
        readResult: fifoRead,
        lastKnownSnapshot: nil,
        now: parseISO("2026-11-20T18:01:00Z"),
        hostAppRunning: true
    )
    if case .stale(_, _, let readOk) = fifoEval, !readOk {
        passed += 1
    } else {
        failed += 1
        fputs("FAIL: FIFO read result must evaluate to stale(readSucceeded: false)\n", stderr)
    }

    // 9d. Valid non-symlink hierarchy reads and writes cleanly through fd-anchored path
    let cleanState = tempRoot.appendingPathComponent("clean-state", isDirectory: true)
    let cleanFinder = cleanState.appendingPathComponent("finder", isDirectory: true)
    try! FileManager.default.createDirectory(at: cleanFinder, withIntermediateDirectories: true)
    try! Data(validStatus.utf8).write(to: cleanFinder.appendingPathComponent("status.json"))
    let cleanLoc = SnapshotEvaluator.handoffLocation(
        parentDirectory: tempRoot,
        relativeComponents: ["clean-state", "finder"]
    )
    if case .valid(let snap) = SnapshotEvaluator.readSnapshotData(location: cleanLoc) {
        expect(snap.version == 1, "clean fd-anchored status.json must decode")
    } else {
        failed += 1
        fputs("FAIL: clean fd-anchored status.json must decode\n", stderr)
    }
    do {
        let written = try SnapshotEvaluator.writeActionRequest(actionReq, toLocation: cleanLoc)
        let data = try Data(contentsOf: written)
        expect(SnapshotEvaluator.decodeValidActionRequest(data: data) == actionReq, "fd-anchored action request must round-trip")
    } catch {
        failed += 1
        fputs("FAIL: clean fd-anchored writeActionRequest failed: \(error)\n", stderr)
    }
}

print("SnapshotModelTests: \(passed) passed, \(failed) failed")
if failed > 0 {
    exit(1)
}
    }
}
