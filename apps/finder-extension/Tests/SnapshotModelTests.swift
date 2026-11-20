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
    if case .stale(let s, _) = expiredEval, s != nil {
        passed += 1
    } else {
        failed += 1
        fputs("FAIL: snapshot 301s old should be stale while preserving last known roots\n", stderr)
    }

    let hostDeadEval = SnapshotEvaluator.evaluate(snapshot: snap, now: parseISO("2026-11-20T18:00:10Z"), hostAppRunning: false)
    if case .stale(let s, _) = hostDeadEval, s != nil {
        passed += 1
    } else {
        failed += 1
        fputs("FAIL: snapshot when host app is not running should be stale\n", stderr)
    }

    let nilEval = SnapshotEvaluator.evaluate(snapshot: nil, now: parseISO("2026-11-20T18:00:10Z"), hostAppRunning: true)
    if case .stale(nil, _) = nilEval {
        passed += 1
    } else {
        failed += 1
        fputs("FAIL: nil snapshot should evaluate to stale(nil)\n", stderr)
    }
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
    let staleEval = SnapshotEvaluation.stale(snap, reason: "Outdated")

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

// 6. Atomic action request serialization (no credentials, valid JSON)
do {
    let tempDir = FileManager.default.temporaryDirectory.appendingPathComponent("fs-act-\(UUID().uuidString)")
    try! FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: tempDir) }

    let req = ActionRequest(
        version: 1,
        id: "req-123",
        requestedAt: "2026-11-20T18:01:00Z",
        action: .syncNow,
        mappingId: "research"
    )
    do {
        let writtenURL = try SnapshotEvaluator.writeActionRequest(req, toDirectory: tempDir)
        let data = try Data(contentsOf: writtenURL)
        let decoded = try JSONDecoder().decode(ActionRequest.self, from: data)
        expect(decoded == req, "action request should round-trip atomically")
        expect(writtenURL.pathExtension == "json", "written action file should be .json")
    } catch {
        failed += 1
        fputs("FAIL: writeActionRequest threw error: \(error)\n", stderr)
    }
}

print("SnapshotModelTests: \(passed) passed, \(failed) failed")
if failed > 0 {
    exit(1)
}
    }
}
