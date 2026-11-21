import Darwin
import Foundation

public let snapshotSchemaVersion = 1
public let defaultStaleAfterSeconds = 300
public let productionHostBundleIdentifier = "dev.foldersync.app"
public let productionExtensionBundleIdentifier = "dev.foldersync.app.finder-extension"
public let defaultSharedHomeRelativePath = "Library/Application Support/FolderSync/finder"
public let darwinSnapshotUpdatedNotification = "dev.foldersync.finder.snapshot-updated"
public let darwinActionRequestedNotification = "dev.foldersync.finder.action-requested"

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

public enum SnapshotReadResult: Equatable {
    case valid(StatusSnapshot)
    case failed(reason: String)
}

public enum SnapshotEvaluation: Equatable {
    case fresh(StatusSnapshot)
    case stale(StatusSnapshot?, reason: String, readSucceeded: Bool)

    /// Returns the snapshot ONLY when it came from a successful read.
    /// Used for menu mappings and watch-list (`directoryURLs`) updates.
    public var successfulSnapshot: StatusSnapshot? {
        switch self {
        case .fresh(let snap):
            return snap
        case .stale(let snap, _, let readSucceeded):
            return readSucceeded ? snap : nil
        }
    }

    /// Returns the snapshot (including a failed-read cached fallback) strictly for
    /// displaying `.stale` root badges.
    public var badgeRootsSnapshot: StatusSnapshot? {
        switch self {
        case .fresh(let snap):
            return snap
        case .stale(let snap, _, _):
            return snap
        }
    }
}

public enum ActionKind: String, Codable, Equatable {
    case syncNow = "sync_now"
    case pause = "pause"
    case openApp = "open_app"
    case showLastRun = "show_last_run"

    /// Only `open_app` may omit `mapping_id`; all other actions require a mapping.
    public var requiresMapping: Bool {
        switch self {
        case .syncNow, .pause, .showLastRun:
            return true
        case .openApp:
            return false
        }
    }
}

public enum ActionValidationError: Error, Equatable {
    case invalidActionRequest
    case unsafeIdentifier
    case symlinkRefused
    case nonRegularFile
    case ioFailure(Int32)
}

/// Anchored handoff location resolved once at startup so every read, directory
/// creation, and atomic publish walks from `parentDirectory` through every
/// component in `relativeComponents` via `fstatat(..., AT_SYMLINK_NOFOLLOW)`
/// and `openat(..., O_DIRECTORY | O_NOFOLLOW)`.
public struct FinderHandoffLocation: Equatable {
    public let parentDirectory: URL
    public let relativeComponents: [String]

    public var resolvedURL: URL {
        relativeComponents.reduce(parentDirectory) { acc, comp in
            acc.appendingPathComponent(comp, isDirectory: true)
        }
    }
}

public struct ActionRequest: Codable, Equatable {
    public let version: Int
    public let id: String
    public let requestedAt: String
    public let action: ActionKind
    public let mappingId: String?

    public static func isValid(version: Int, action: ActionKind, mappingId: String?) -> Bool {
        guard version == snapshotSchemaVersion else {
            return false
        }
        if action.requiresMapping {
            guard let mappingId,
                  !mappingId.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
                return false
            }
        }
        return true
    }

    /// Failable initializer enforcing the v1 action contract at construction:
    /// only `open_app` may omit `mappingId`.
    public init?(
        version: Int = snapshotSchemaVersion,
        id: String = UUID().uuidString,
        requestedAt: String,
        action: ActionKind,
        mappingId: String?
    ) {
        guard Self.isValid(version: version, action: action, mappingId: mappingId) else {
            return nil
        }
        self.version = version
        self.id = id
        self.requestedAt = requestedAt
        self.action = action
        self.mappingId = mappingId
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let version = try container.decode(Int.self, forKey: .version)
        let id = try container.decode(String.self, forKey: .id)
        let requestedAt = try container.decode(String.self, forKey: .requestedAt)
        let action = try container.decode(ActionKind.self, forKey: .action)
        let mappingId = try container.decodeIfPresent(String.self, forKey: .mappingId)
        guard Self.isValid(version: version, action: action, mappingId: mappingId) else {
            throw ActionValidationError.invalidActionRequest
        }
        self.version = version
        self.id = id
        self.requestedAt = requestedAt
        self.action = action
        self.mappingId = mappingId
    }

    public var isValid: Bool {
        Self.isValid(version: version, action: action, mappingId: mappingId)
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
    /// Returns the real user home directory even when invoked inside a macOS App Sandbox
    /// container (where `NSHomeDirectory()` points to `~/Library/Containers/<id>/Data`).
    public static func realUserHomeDirectoryURL() -> URL {
        if let pw = getpwuid(getuid()), let dir = pw.pointee.pw_dir {
            let path = String(cString: dir)
            if !path.isEmpty {
                return URL(fileURLWithPath: path, isDirectory: true)
            }
        }
        return FileManager.default.homeDirectoryForCurrentUser
    }

    /// Builds an anchored handoff location from an existing parent directory and
    /// relative components from the state root down (e.g., `["FolderSync", "finder"]`).
    public static func handoffLocation(
        parentDirectory: URL,
        relativeComponents: [String]
    ) -> FinderHandoffLocation {
        FinderHandoffLocation(
            parentDirectory: parentDirectory,
            relativeComponents: relativeComponents
        )
    }

    /// Resolves the Finder handoff location once at startup so the state root and
    /// every component beneath it are validated via `lstat`/`fstatat` + `openat`.
    public static func resolveHandoffLocation(
        configuredAbsolutePath: String?,
        configuredHomeRelativePath: String?,
        realHomeDirectory: URL? = nil
    ) -> FinderHandoffLocation {
        if let abs = configuredAbsolutePath?.trimmingCharacters(in: .whitespacesAndNewlines),
           !abs.isEmpty {
            let url = URL(fileURLWithPath: abs, isDirectory: true).standardizedFileURL
            return FinderHandoffLocation(
                parentDirectory: url.deletingLastPathComponent(),
                relativeComponents: [url.lastPathComponent]
            )
        }

        let home = realHomeDirectory ?? realUserHomeDirectoryURL()
        let rawRel = configuredHomeRelativePath?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        let trimmedRel = rawRel.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        let rawComponents = trimmedRel.split(separator: "/").map(String.init)
        let relIsSafe = !rawComponents.isEmpty
            && !rawComponents.contains("..")
            && !rawComponents.contains(".")
            && !trimmedRel.contains("\0")
        let effectiveComponents = relIsSafe
            ? rawComponents
            : defaultSharedHomeRelativePath.split(separator: "/").map(String.init)

        // Anchor at <realHome>/Library/Application Support when the default prefix is used,
        // validating from the FolderSync state root down (`["FolderSync", "finder"]`).
        if effectiveComponents.count >= 3
            && effectiveComponents[0] == "Library"
            && effectiveComponents[1] == "Application Support" {
            let parent = home
                .appendingPathComponent("Library", isDirectory: true)
                .appendingPathComponent("Application Support", isDirectory: true)
            return FinderHandoffLocation(
                parentDirectory: parent,
                relativeComponents: Array(effectiveComponents.dropFirst(2))
            )
        }
        return FinderHandoffLocation(
            parentDirectory: home,
            relativeComponents: effectiveComponents
        )
    }

    /// Resolves the Finder handoff directory (`<state dir>/finder`):
    /// 1. Explicit `FolderSyncSharedDir` absolute override (used in isolated dev/temp runs).
    /// 2. `FolderSyncSharedHomeRelativeDir` (defaults to `Library/Application Support/FolderSync/finder`)
    ///    resolved against the real user home directory (matching the temporary file exception).
    public static func resolveSharedDirectoryURL(
        configuredAbsolutePath: String?,
        configuredHomeRelativePath: String?,
        realHomeDirectory: URL? = nil
    ) -> URL {
        resolveHandoffLocation(
            configuredAbsolutePath: configuredAbsolutePath,
            configuredHomeRelativePath: configuredHomeRelativePath,
            realHomeDirectory: realHomeDirectory
        ).resolvedURL
    }

    /// Disposable default shared directory used by the throwaway mock host when
    /// `FolderSyncSharedDir` is not set in Info.plist.
    public static func defaultSpikeSharedDirectoryURL() -> URL {
        FileManager.default.temporaryDirectory
            .appendingPathComponent("foldersync-finder-spike-shared", isDirectory: true)
    }

    /// Validates an identifier used as a single path component (`[A-Za-z0-9][A-Za-z0-9_-]{0,127}`).
    public static func isSafePathComponentId(_ value: String) -> Bool {
        guard !value.isEmpty && value.count <= 128 else {
            return false
        }
        var scalars = value.unicodeScalars.makeIterator()
        guard let first = scalars.next() else {
            return false
        }
        let isAlnum: (UnicodeScalar) -> Bool = { s in
            (s >= "a" && s <= "z") || (s >= "A" && s <= "Z") || (s >= "0" && s <= "9")
        }
        guard isAlnum(first) else {
            return false
        }
        while let s = scalars.next() {
            if !(isAlnum(s) || s == "-" || s == "_") {
                return false
            }
        }
        return true
    }

    /// Returns true if `url` exists as a symbolic link (`lstat`, without following links).
    public static func isSymbolicLink(at url: URL) -> Bool {
        var st = stat()
        guard lstat(url.path, &st) == 0 else {
            return false
        }
        return (st.st_mode & S_IFMT) == S_IFLNK
    }

    private static let maxSnapshotBytes = 4 * 1024 * 1024

    /// Normalizes only macOS root OS aliases (`/var -> /private/var`, `/tmp -> /private/tmp`)
    /// at the very beginning of an absolute path, without following any user-controlled symlinks.
    private static func normalizeSystemRootPrefix(_ rawPath: String) -> String {
        var path = rawPath
        while path.count > 1 && path.hasSuffix("/") {
            path.removeLast()
        }
        let osAliases: [(String, String)] = [("/var", "/private/var"), ("/tmp", "/private/tmp")]
        for (alias, target) in osAliases {
            if path == alias || path.hasPrefix(alias + "/") {
                var buf = [CChar](repeating: 0, count: Int(PATH_MAX))
                let len = readlink(alias, &buf, buf.count - 1)
                if len > 0 {
                    let linkDest = String(cString: buf)
                    if linkDest == target.dropFirst() || linkDest == target {
                        return target + path.dropFirst(alias.count)
                    }
                }
            }
        }
        return path
    }

    /// Walks every path component from `/` down to `normalizedPath` using `lstat` ONLY
    /// (never calling `open()` on non-entitled parent directories inside the App Sandbox),
    /// refusing any symlinked component anywhere in the ancestor chain.
    private static func verifyNoSymlinkedAncestors(normalizedPath: String) throws -> stat {
        guard normalizedPath.hasPrefix("/") else {
            throw ActionValidationError.unsafeIdentifier
        }
        let parts = normalizedPath.split(separator: "/").map(String.init)
        guard !parts.isEmpty else {
            throw ActionValidationError.unsafeIdentifier
        }
        var current = ""
        var lastStat = stat()
        for part in parts {
            if part == "." || part == ".." {
                throw ActionValidationError.unsafeIdentifier
            }
            current += "/" + part
            var st = stat()
            if lstat(current, &st) != 0 {
                let err = errno
                // Inside macOS App Sandbox, lstat on a non-entitled intermediate ancestor
                // may return EPERM/EACCES; fail closed on ENOENT/ENOTDIR, while symlink
                // components return 0 with S_IFLNK when visible.
                throw ActionValidationError.ioFailure(err)
            }
            let ftype = st.st_mode & S_IFMT
            if ftype == S_IFLNK {
                throw ActionValidationError.symlinkRefused
            }
            if ftype != S_IFDIR {
                throw ActionValidationError.ioFailure(ENOTDIR)
            }
            lastStat = st
        }
        return lastStat
    }

    /// Validates all ancestors with `lstat` (without opening non-entitled parents),
    /// opens ONLY the entitled handoff directory itself with `O_RDONLY | O_DIRECTORY | O_NOFOLLOW`,
    /// re-verifies the ancestor chain and `(st_dev, st_ino)` identity, and optionally steps
    /// into fd-anchored child directories (`extraComponents`, e.g. `["actions"]`).
    private static func openValidatedDirectoryFD(
        location: FinderHandoffLocation,
        extraComponents: [String] = [],
        createExtra: Bool = false
    ) throws -> Int32 {
        let normalizedHandoff = normalizeSystemRootPrefix(location.resolvedURL.standardizedFileURL.path)
        _ = try verifyNoSymlinkedAncestors(normalizedPath: normalizedHandoff)

        var curFd = Darwin.open(normalizedHandoff, O_RDONLY | O_DIRECTORY | O_NOFOLLOW)
        guard curFd >= 0 else {
            if errno == ELOOP {
                throw ActionValidationError.symlinkRefused
            }
            throw ActionValidationError.ioFailure(errno)
        }

        // Re-check ancestors after open and verify the opened fd refers to the exact same directory.
        var openedSt = stat()
        guard fstat(curFd, &openedSt) == 0, (openedSt.st_mode & S_IFMT) == S_IFDIR else {
            let err = errno
            Darwin.close(curFd)
            throw ActionValidationError.ioFailure(err)
        }
        do {
            let postSt = try verifyNoSymlinkedAncestors(normalizedPath: normalizedHandoff)
            if postSt.st_dev != openedSt.st_dev || postSt.st_ino != openedSt.st_ino {
                Darwin.close(curFd)
                throw ActionValidationError.symlinkRefused
            }
        } catch {
            Darwin.close(curFd)
            throw error
        }

        for component in extraComponents {
            guard !component.isEmpty && component != "." && component != ".." && !component.contains("/") else {
                Darwin.close(curFd)
                throw ActionValidationError.unsafeIdentifier
            }
            var st = stat()
            if fstatat(curFd, component, &st, AT_SYMLINK_NOFOLLOW) == 0 {
                let ftype = st.st_mode & S_IFMT
                if ftype == S_IFLNK {
                    Darwin.close(curFd)
                    throw ActionValidationError.symlinkRefused
                }
                if ftype != S_IFDIR {
                    Darwin.close(curFd)
                    throw ActionValidationError.ioFailure(ENOTDIR)
                }
            } else if errno == ENOENT && createExtra {
                if mkdirat(curFd, component, 0o700) != 0 && errno != EEXIST {
                    let err = errno
                    Darwin.close(curFd)
                    throw ActionValidationError.ioFailure(err)
                }
                if fstatat(curFd, component, &st, AT_SYMLINK_NOFOLLOW) != 0 {
                    let err = errno
                    Darwin.close(curFd)
                    throw ActionValidationError.ioFailure(err)
                }
                if (st.st_mode & S_IFMT) == S_IFLNK {
                    Darwin.close(curFd)
                    throw ActionValidationError.symlinkRefused
                }
                if (st.st_mode & S_IFMT) != S_IFDIR {
                    Darwin.close(curFd)
                    throw ActionValidationError.ioFailure(ENOTDIR)
                }
            } else {
                let err = errno
                Darwin.close(curFd)
                throw ActionValidationError.ioFailure(err)
            }

            let nextFd = openat(curFd, component, O_RDONLY | O_DIRECTORY | O_NOFOLLOW)
            Darwin.close(curFd)
            guard nextFd >= 0 else {
                if errno == ELOOP {
                    throw ActionValidationError.symlinkRefused
                }
                throw ActionValidationError.ioFailure(errno)
            }
            var childSt = stat()
            guard fstat(nextFd, &childSt) == 0, (childSt.st_mode & S_IFMT) == S_IFDIR else {
                let err = errno
                Darwin.close(nextFd)
                throw ActionValidationError.ioFailure(err)
            }
            curFd = nextFd
        }
        return curFd
    }

    /// Reads and validates `status.json` via a validated directory descriptor,
    /// opening with `O_RDONLY | O_NONBLOCK | O_NOFOLLOW` and requiring a regular
    /// file (`fstat` `S_IFREG`) before reading so FIFOs/sockets never hang Finder.
    public static func readSnapshotData(location: FinderHandoffLocation) -> SnapshotReadResult {
        let dirFd: Int32
        do {
            dirFd = try openValidatedDirectoryFD(location: location)
        } catch ActionValidationError.symlinkRefused {
            return .failed(reason: "Status unavailable (symlink refused)")
        } catch {
            return .failed(reason: "Status unavailable")
        }
        defer { Darwin.close(dirFd) }

        var leafSt = stat()
        if fstatat(dirFd, "status.json", &leafSt, AT_SYMLINK_NOFOLLOW) == 0 {
            let ftype = leafSt.st_mode & S_IFMT
            if ftype == S_IFLNK {
                return .failed(reason: "Status unavailable (symlink refused)")
            }
            if ftype != S_IFREG {
                return .failed(reason: "Status unavailable (non-regular file)")
            }
        } else {
            return .failed(reason: "Status unavailable")
        }

        let fd = openat(dirFd, "status.json", O_RDONLY | O_NONBLOCK | O_NOFOLLOW)
        guard fd >= 0 else {
            if errno == ELOOP {
                return .failed(reason: "Status unavailable (symlink refused)")
            }
            return .failed(reason: "Status unavailable")
        }
        defer { Darwin.close(fd) }

        var st = stat()
        guard fstat(fd, &st) == 0 else {
            return .failed(reason: "Status unavailable")
        }
        guard (st.st_mode & S_IFMT) == S_IFREG else {
            return .failed(reason: "Status unavailable (non-regular file)")
        }
        if st.st_size > maxSnapshotBytes {
            return .failed(reason: "Status unavailable (snapshot too large)")
        }

        var data = Data()
        var buf = [UInt8](repeating: 0, count: 16384)
        while true {
            let n = Darwin.read(fd, &buf, buf.count)
            if n > 0 {
                if data.count + n > maxSnapshotBytes {
                    return .failed(reason: "Status unavailable (snapshot too large)")
                }
                data.append(buf, count: n)
            } else if n == 0 {
                break
            } else {
                if errno == EINTR { continue }
                return .failed(reason: "Status unavailable")
            }
        }

        guard let decoded = decode(data: data) else {
            return .failed(reason: "Status unavailable (invalid snapshot)")
        }
        return .valid(decoded)
    }

    /// Convenience wrapper for reading a `status.json` URL with fd-anchored validation.
    public static func readSnapshotData(at fileURL: URL) -> SnapshotReadResult {
        let dirURL = fileURL.deletingLastPathComponent().standardizedFileURL
        let loc = FinderHandoffLocation(
            parentDirectory: dirURL.deletingLastPathComponent(),
            relativeComponents: [dirURL.lastPathComponent]
        )
        return readSnapshotData(location: loc)
    }

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
            return .stale(nil, reason: "Status unavailable", readSucceeded: false)
        }
        return evaluate(
            readResult: .valid(snapshot),
            lastKnownSnapshot: snapshot,
            now: now,
            hostAppRunning: hostAppRunning
        )
    }

    /// Evaluates the latest snapshot read outcome. When reading or decoding fails,
    /// returns `.stale(..., readSucceeded: false)` so `lastKnownSnapshot` is used
    /// strictly for stale root badges and never supplies menu mappings or watch-list changes.
    public static func evaluate(
        readResult: SnapshotReadResult,
        lastKnownSnapshot: StatusSnapshot?,
        now: Date,
        hostAppRunning: Bool
    ) -> SnapshotEvaluation {
        switch readResult {
        case .failed(let reason):
            return .stale(lastKnownSnapshot, reason: reason, readSucceeded: false)
        case .valid(let snapshot):
            guard hostAppRunning else {
                return .stale(snapshot, reason: "FolderSync is not running", readSucceeded: true)
            }
            guard let generatedDate = parseISO8601(snapshot.generatedAt) else {
                return .stale(snapshot, reason: "Status timestamp is invalid", readSucceeded: true)
            }
            let maxAge = TimeInterval(max(1, snapshot.staleAfterSeconds ?? defaultStaleAfterSeconds))
            let age = now.timeIntervalSince(generatedDate)
            if age > maxAge {
                return .stale(snapshot, reason: "Status is outdated", readSucceeded: true)
            }
            return .fresh(snapshot)
        }
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

    /// Resolves the mapping for context/toolbar menu actions. Returns `nil` after a
    /// failed snapshot read so mapping-scoped actions are disabled and Open FolderSync
    /// carries no mapping ID.
    public static func menuMapping(
        for url: URL,
        evaluation: SnapshotEvaluation
    ) -> MappingSnapshot? {
        guard let snapshot = evaluation.successfulSnapshot else {
            return nil
        }
        return owningMapping(for: url, in: snapshot)
    }

    /// Derives `directoryURLs` updates only from successful snapshot reads.
    public static func directoryRootURLs(for evaluation: SnapshotEvaluation) -> Set<URL>? {
        guard let snapshot = evaluation.successfulSnapshot else {
            return nil
        }
        return Set(snapshot.mappings.map {
            URL(fileURLWithPath: canonicalPath($0.rootPath))
        })
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
        case .stale(let snapshot, _, _):
            guard let snapshot, rootMapping(for: url, in: snapshot) != nil else {
                return nil
            }
            return .stale
        }
    }

    public static func decodeValidActionRequest(data: Data) -> ActionRequest? {
        guard let request = try? JSONDecoder().decode(ActionRequest.self, from: data),
              request.isValid else {
            return nil
        }
        return request
    }

    /// Writes and publishes an action request strictly via validated directory
    /// descriptors (`openat` + `O_CREAT | O_EXCL | O_NOFOLLOW` + `renameat`),
    /// refusing any symlink from the state root down.
    public static func writeActionRequest(
        _ request: ActionRequest,
        toLocation location: FinderHandoffLocation
    ) throws -> URL {
        guard request.isValid else {
            throw ActionValidationError.invalidActionRequest
        }
        guard isSafePathComponentId(request.id) else {
            throw ActionValidationError.unsafeIdentifier
        }

        let actionsFd = try openValidatedDirectoryFD(
            location: location,
            extraComponents: ["actions"],
            createExtra: true
        )
        defer { Darwin.close(actionsFd) }

        let finalURL = location.resolvedURL
            .appendingPathComponent("actions", isDirectory: true)
            .appendingPathComponent("action-\(request.id).json")
        try writeActionFile(request, intoActionsFD: actionsFd)
        return finalURL
    }

    public static func writeActionRequest(
        _ request: ActionRequest,
        toDirectory directory: URL
    ) throws -> URL {
        guard request.isValid else {
            throw ActionValidationError.invalidActionRequest
        }
        guard isSafePathComponentId(request.id) else {
            throw ActionValidationError.unsafeIdentifier
        }
        let stdDir = directory.standardizedFileURL
        let parentDir = stdDir.deletingLastPathComponent()
        let grandParent = parentDir.deletingLastPathComponent()
        let loc = FinderHandoffLocation(
            parentDirectory: grandParent,
            relativeComponents: [parentDir.lastPathComponent]
        )
        let actionsFd = try openValidatedDirectoryFD(
            location: loc,
            extraComponents: [stdDir.lastPathComponent],
            createExtra: true
        )
        defer { Darwin.close(actionsFd) }

        try writeActionFile(request, intoActionsFD: actionsFd)
        return stdDir.appendingPathComponent("action-\(request.id).json")
    }

    private static func writeActionFile(
        _ request: ActionRequest,
        intoActionsFD actionsFd: Int32
    ) throws {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        let data = try encoder.encode(request)

        let finalName = "action-\(request.id).json"
        let tmpName = ".action-\(request.id).tmp"

        var st = stat()
        if fstatat(actionsFd, finalName, &st, AT_SYMLINK_NOFOLLOW) == 0,
           (st.st_mode & S_IFMT) == S_IFLNK {
            throw ActionValidationError.symlinkRefused
        }
        if fstatat(actionsFd, tmpName, &st, AT_SYMLINK_NOFOLLOW) == 0,
           (st.st_mode & S_IFMT) == S_IFLNK {
            throw ActionValidationError.symlinkRefused
        }

        let fd = openat(actionsFd, tmpName, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW, 0o600)
        guard fd >= 0 else {
            if errno == ELOOP {
                throw ActionValidationError.symlinkRefused
            }
            throw ActionValidationError.ioFailure(errno)
        }
        var writeFailed = false
        defer {
            Darwin.close(fd)
            if writeFailed {
                _ = unlinkat(actionsFd, tmpName, 0)
            }
        }
        try data.withUnsafeBytes { rawBuf in
            guard let base = rawBuf.baseAddress else { return }
            var offset = 0
            while offset < rawBuf.count {
                let written = Darwin.write(fd, base.advanced(by: offset), rawBuf.count - offset)
                if written < 0 {
                    if errno == EINTR { continue }
                    writeFailed = true
                    throw ActionValidationError.ioFailure(errno)
                }
                offset += written
            }
        }
        if fsync(fd) != 0 {
            writeFailed = true
            throw ActionValidationError.ioFailure(errno)
        }

        if fstatat(actionsFd, finalName, &st, AT_SYMLINK_NOFOLLOW) == 0,
           (st.st_mode & S_IFMT) == S_IFLNK {
            writeFailed = true
            throw ActionValidationError.symlinkRefused
        }
        if renameat(actionsFd, tmpName, actionsFd, finalName) != 0 {
            writeFailed = true
            throw ActionValidationError.ioFailure(errno)
        }
    }
}
