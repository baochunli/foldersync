import Foundation

public let snapshotSchemaVersion = 1
public let defaultStaleAfterSeconds = 300

public enum MappingState: String, Codable, Equatable {
    case synced
    case syncing
    case attention
    case paused
    case diskMissing = "disk_missing"

    public var badgeState: BadgeState {
        switch self {
        case .synced: return .synced
        case .syncing: return .syncing
        case .attention: return .attention
        case .paused: return .paused
        case .diskMissing: return .diskMissing
        }
    }
}

public enum BadgeState: String, CaseIterable, Equatable {
    case synced
    case syncing
    case attention
    case paused
    case diskMissing = "disk_missing"
    case stale

    public var displayLabel: String {
        switch self {
        case .synced: return "Synced"
        case .syncing: return "Syncing"
        case .attention: return "Needs attention"
        case .paused: return "Paused"
        case .diskMissing: return "Disk missing"
        case .stale: return "FolderSync unavailable"
        }
    }
}

public struct MappingSnapshot: Codable, Equatable {
    public let id: String
    public let rootPath: String
    public let state: MappingState
    public let label: String
    public let lastRunSummary: String?

    public init(
        id: String,
        rootPath: String,
        state: MappingState,
        label: String,
        lastRunSummary: String? = nil
    ) {
        self.id = id
        self.rootPath = rootPath
        self.state = state
        self.label = label
        self.lastRunSummary = lastRunSummary
    }

    enum CodingKeys: String, CodingKey {
        case id
        case rootPath = "root_path"
        case state
        case label
        case lastRunSummary = "last_run_summary"
    }
}

public struct StatusSnapshot: Codable, Equatable {
    public let version: Int
    public let generatedAt: String
    public let staleAfterSeconds: Int?
    public let mappings: [MappingSnapshot]

    public init(
        version: Int,
        generatedAt: String,
        staleAfterSeconds: Int? = nil,
        mappings: [MappingSnapshot]
    ) {
        self.version = version
        self.generatedAt = generatedAt
        self.staleAfterSeconds = staleAfterSeconds
        self.mappings = mappings
    }

    enum CodingKeys: String, CodingKey {
        case version
        case generatedAt = "generated_at"
        case staleAfterSeconds = "stale_after_seconds"
        case mappings
    }
}

public enum SnapshotEvaluation: Equatable {
    case fresh(StatusSnapshot)
    case stale(StatusSnapshot?, reason: String)

    public var snapshot: StatusSnapshot? {
        switch self {
        case .fresh(let snap): return snap
        case .stale(let snap, _): return snap
        }
    }
}

public enum ActionKind: String, Codable, Equatable {
    case syncNow = "sync_now"
    case pause = "pause"
    case openApp = "open_app"
    case showLastRun = "show_last_run"
}

public struct ActionRequest: Codable, Equatable {
    public let version: Int
    public let id: String
    public let requestedAt: String
    public let action: ActionKind
    public let mappingId: String?

    public init(
        version: Int = snapshotSchemaVersion,
        id: String = UUID().uuidString,
        requestedAt: String,
        action: ActionKind,
        mappingId: String?
    ) {
        self.version = version
        self.id = id
        self.requestedAt = requestedAt
        self.action = action
        self.mappingId = mappingId
    }

    enum CodingKeys: String, CodingKey {
        case version
        case id
        case requestedAt = "requested_at"
        case action
        case mappingId = "mapping_id"
    }
}

public enum SnapshotEvaluator {
    public static func formatISO8601(_ date: Date) -> String {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime]
        return formatter.string(from: date)
    }

    public static func parseISO8601(_ value: String) -> Date? {
        let withFrac = ISO8601DateFormatter()
        withFrac.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let date = withFrac.date(from: value) {
            return date
        }
        let plain = ISO8601DateFormatter()
        plain.formatOptions = [.withInternetDateTime]
        return plain.date(from: value)
    }

    public static func decode(data: Data) -> StatusSnapshot? {
        guard let snapshot = try? JSONDecoder().decode(StatusSnapshot.self, from: data) else {
            return nil
        }
        guard snapshot.version == snapshotSchemaVersion else {
            return nil
        }
        guard parseISO8601(snapshot.generatedAt) != nil else {
            return nil
        }
        return snapshot
    }

    public static func evaluate(
        snapshot: StatusSnapshot?,
        now: Date,
        hostAppRunning: Bool
    ) -> SnapshotEvaluation {
        guard let snapshot else {
            return .stale(nil, reason: "Status unavailable")
        }
        guard hostAppRunning else {
            return .stale(snapshot, reason: "FolderSync is not running")
        }
        guard let generatedDate = parseISO8601(snapshot.generatedAt) else {
            return .stale(snapshot, reason: "Status timestamp is invalid")
        }
        let maxAge = TimeInterval(max(1, snapshot.staleAfterSeconds ?? defaultStaleAfterSeconds))
        let age = now.timeIntervalSince(generatedDate)
        if age > maxAge {
            return .stale(snapshot, reason: "Status is outdated")
        }
        return .fresh(snapshot)
    }

    public static func canonicalPath(_ path: String) -> String {
        let url = URL(fileURLWithPath: path).standardizedFileURL.resolvingSymlinksInPath()
        var normalized = url.path
        while normalized.count > 1 && normalized.hasSuffix("/") {
            normalized.removeLast()
        }
        return normalized
    }

    public static func rootMapping(for url: URL, in snapshot: StatusSnapshot) -> MappingSnapshot? {
        let candidate = canonicalPath(url.path)
        for mapping in snapshot.mappings {
            if canonicalPath(mapping.rootPath) == candidate {
                return mapping
            }
        }
        return nil
    }

    public static func owningMapping(for url: URL, in snapshot: StatusSnapshot) -> MappingSnapshot? {
        let candidate = canonicalPath(url.path)
        for mapping in snapshot.mappings {
            let root = canonicalPath(mapping.rootPath)
            if candidate == root {
                return mapping
            }
            let prefix = root == "/" ? "/" : root + "/"
            if candidate.hasPrefix(prefix) {
                return mapping
            }
        }
        return nil
    }

    public static func badgeState(
        for url: URL,
        evaluation: SnapshotEvaluation
    ) -> BadgeState? {
        switch evaluation {
        case .fresh(let snapshot):
            guard let mapping = rootMapping(for: url, in: snapshot) else {
                return nil
            }
            return mapping.state.badgeState
        case .stale(let snapshot, _):
            guard let snapshot, rootMapping(for: url, in: snapshot) != nil else {
                return nil
            }
            return .stale
        }
    }

    public static func writeActionRequest(
        _ request: ActionRequest,
        toDirectory directory: URL
    ) throws -> URL {
        let fm = FileManager.default
        try fm.createDirectory(at: directory, withIntermediateDirectories: true)
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        let data = try encoder.encode(request)

        let safeId = request.id.replacingOccurrences(of: "/", with: "_")
        let finalURL = directory.appendingPathComponent("action-\(safeId).json")
        let tmpURL = directory.appendingPathComponent(".action-\(safeId).tmp")
        try data.write(to: tmpURL, options: .atomic)
        if fm.fileExists(atPath: finalURL.path) {
            try fm.removeItem(at: finalURL)
        }
        try fm.moveItem(at: tmpURL, to: finalURL)
        return finalURL
    }
}
