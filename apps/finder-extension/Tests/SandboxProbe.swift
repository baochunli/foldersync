import Darwin
import Foundation

/// Ad hoc signed macOS App Sandbox probe (no pluginkit registration, temp layout only).
/// Verifies that inside a real macOS App Sandbox where ONLY the handoff directory is
/// entitled (and NOT its parent directories):
///   1. Reading a valid `status.json` succeeds via `open(O_DIRECTORY|O_NOFOLLOW)` + `openat`.
///   2. Writing an action request under `actions/` succeeds via fd-anchored `mkdirat`/`openat`/`renameat`.
///   3. Reading/writing through a symlinked ancestor is refused (`symlinkRefused`).
///   4. A FIFO at `status.json` fails non-blockingly as non-regular without hanging.
@main
struct SandboxProbeMain {
    static func main() {
        let args = CommandLine.arguments
        guard args.count == 3 else {
            fputs("Usage: SandboxProbe <entitled-handoff-dir> <symlink-ancestor-handoff-dir>\n", stderr)
            exit(2)
        }
        let entitledPath = args[1]
        let symlinkedPath = args[2]

        // 0. Prove the sandbox actively denies opening the non-entitled parent directory.
        let parentPath = URL(fileURLWithPath: entitledPath, isDirectory: true)
            .deletingLastPathComponent().path
        let parentFd = Darwin.open(parentPath, O_RDONLY | O_DIRECTORY)
        if parentFd >= 0 {
            Darwin.close(parentFd)
            fputs("FAIL: sandbox probe expected EPERM when opening non-entitled parent \(parentPath)\n", stderr)
            exit(1)
        }
        guard errno == EPERM else {
            fputs("FAIL: sandbox probe expected EPERM on parent open, got errno=\(errno)\n", stderr)
            exit(1)
        }

        // 1. Seed a valid status.json inside the entitled handoff dir and verify readSnapshotData succeeds.
        let validJSON = """
        {"version":1,"generated_at":"2026-11-20T18:00:00Z","stale_after_seconds":300,"mappings":[]}
        """
        let entitledURL = URL(fileURLWithPath: entitledPath, isDirectory: true)
        let statusURL = entitledURL.appendingPathComponent("status.json")
        do {
            try Data(validJSON.utf8).write(to: statusURL, options: .atomic)
        } catch {
            fputs("FAIL: sandbox probe could not seed status.json inside entitled dir: \(error)\n", stderr)
            exit(1)
        }

        let entitledLoc = SnapshotEvaluator.resolveHandoffLocation(
            configuredAbsolutePath: entitledPath,
            configuredHomeRelativePath: nil
        )
        guard case .valid(let snap) = SnapshotEvaluator.readSnapshotData(location: entitledLoc),
              snap.version == 1 else {
            fputs("FAIL: sandboxed readSnapshotData failed on entitled handoff dir \(entitledPath)\n", stderr)
            exit(1)
        }

        // 2. Verify writeActionRequest succeeds inside the entitled handoff dir under App Sandbox.
        let req = ActionRequest(
            version: 1,
            id: "sandbox-probe-1",
            requestedAt: "2026-11-20T18:01:00Z",
            action: .syncNow,
            mappingId: "research"
        )!
        do {
            let written = try SnapshotEvaluator.writeActionRequest(req, toLocation: entitledLoc)
            let data = try Data(contentsOf: written)
            guard SnapshotEvaluator.decodeValidActionRequest(data: data) == req else {
                fputs("FAIL: sandboxed action request did not round-trip\n", stderr)
                exit(1)
            }
        } catch {
            fputs("FAIL: sandboxed writeActionRequest failed on entitled handoff dir: \(error)\n", stderr)
            exit(1)
        }

        // 3. Verify symlinked ancestor is refused under App Sandbox.
        let symLoc = SnapshotEvaluator.resolveHandoffLocation(
            configuredAbsolutePath: symlinkedPath,
            configuredHomeRelativePath: nil
        )
        switch SnapshotEvaluator.readSnapshotData(location: symLoc) {
        case .failed(let reason) where reason.contains("symlink"):
            break
        case let other:
            fputs("FAIL: sandboxed readSnapshotData did not refuse symlinked ancestor: \(other)\n", stderr)
            exit(1)
        }
        do {
            _ = try SnapshotEvaluator.writeActionRequest(req, toLocation: symLoc)
            fputs("FAIL: sandboxed writeActionRequest did not refuse symlinked ancestor\n", stderr)
            exit(1)
        } catch ActionValidationError.symlinkRefused {
            // Expected
        } catch {
            fputs("FAIL: expected symlinkRefused for symlinked ancestor, got \(error)\n", stderr)
            exit(1)
        }

        // 4. Verify FIFO status.json inside entitled dir is rejected non-blockingly.
        try? FileManager.default.removeItem(at: statusURL)
        guard Darwin.mkfifo(statusURL.path, 0o600) == 0 else {
            fputs("FAIL: mkfifo inside entitled dir failed (errno=\(errno))\n", stderr)
            exit(1)
        }
        let t0 = Date()
        let fifoRes = SnapshotEvaluator.readSnapshotData(location: entitledLoc)
        let dt = Date().timeIntervalSince(t0)
        guard dt < 2.0, case .failed(let reason) = fifoRes, reason.contains("non-regular") else {
            fputs("FAIL: sandboxed FIFO read did not fail non-blockingly as non-regular (dt=\(dt), res=\(fifoRes))\n", stderr)
            exit(1)
        }

        print("SandboxProbe: 5/5 passed under macOS sandbox (parent open denied EPERM; entitled reads/writes OK; symlink ancestor & FIFO refused)")
    }
}
