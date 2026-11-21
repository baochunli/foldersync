//! Local-to-local integration tests exercising the pinned rclone executable.
//!
//! All tests run inside fresh temporary directories with an explicit throwaway
//! `--config` file, isolated environment (`env_clear`), and launch-boundary
//! SHA-256 verification, and skip cleanly when the pinned binary is absent.

use std::{
    collections::BTreeMap,
    env,
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use rclone_adapter::{
    BackupDir2, BisyncMarker, BisyncRequest, EngineIdentity, InitPreference, LaunchError, LogEvent,
    Outcome, Path2, ProcessError, RealProcessRunner, RunMode, SpawnOptions, classify_run_outcome,
    pinned_binary_path, qualify_binary, sha256_file,
};
use sha2::{Digest, Sha256};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempWorkspace {
    root: PathBuf,
    path1: PathBuf,
    path2: PathBuf,
    workdir: PathBuf,
    config_path: PathBuf,
    filters_file: PathBuf,
    backups: PathBuf,
    truth: PathBuf,
    run_seq: u32,
}

impl TempWorkspace {
    fn new(name: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "foldersync-adapter-it-{name}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let path1 = root.join("path1");
        let path2 = root.join("path2");
        let workdir = root.join("workdir");
        let backups = root.join("backups");
        let truth = root.join("truth");
        let config_path = root.join("rclone.conf");
        let filters_file = root.join("filters.txt");

        fs::create_dir_all(&path1).unwrap();
        fs::create_dir_all(&path2).unwrap();
        fs::create_dir_all(&workdir).unwrap();
        fs::create_dir_all(&backups).unwrap();
        fs::create_dir_all(&truth).unwrap();
        fs::write(&config_path, b"").unwrap();
        fs::write(&filters_file, b"").unwrap();

        let ws = Self {
            root,
            path1,
            path2,
            workdir,
            config_path,
            filters_file,
            backups,
            truth,
            run_seq: 0,
        };
        ws.write_access_markers();
        ws
    }

    fn write_access_markers(&self) {
        let marker = b"foldersync-access-marker-v1\n";
        fs::write(self.path1.join("RCLONE_TEST"), marker).unwrap();
        fs::write(self.path2.join("RCLONE_TEST"), marker).unwrap();
    }

    fn next_backup_dirs(&mut self) -> (PathBuf, PathBuf) {
        self.run_seq += 1;
        let b1 = self.backups.join(format!("run-{}-b1", self.run_seq));
        let b2 = self.backups.join(format!("run-{}-b2", self.run_seq));
        (b1, b2)
    }

    fn build_request(
        &mut self,
        engine: &EngineIdentity,
        mode: RunMode,
        max_delete_percent: u8,
    ) -> (BisyncRequest, PathBuf, PathBuf) {
        let (b1, b2) = self.next_backup_dirs();
        let req = BisyncRequest::builder()
            .engine(engine.clone())
            .mode(mode)
            .path1(&self.path1)
            .path2(Path2::LocalDir(self.path2.clone()))
            .config_path(&self.config_path)
            .workdir(&self.workdir)
            .filters_file(&self.filters_file)
            .check_filename("RCLONE_TEST")
            .max_delete_percent(max_delete_percent)
            .backup_dir1(&b1)
            .backup_dir2(BackupDir2::LocalDir(b2.clone()))
            .build()
            .expect("test BisyncRequest should be valid");
        (req, b1, b2)
    }

    fn record_truth(&self, label: &str, bytes: &[u8]) -> String {
        let path = self.truth.join(label);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, bytes).unwrap();
        sha256_bytes(bytes)
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn qualified_pinned_engine(test_name: &str) -> Option<EngineIdentity> {
    let Some(path) = pinned_binary_path() else {
        eprintln!(
            "Skipping {test_name}: pinned rclone binary is absent; run scripts/fetch-rclone.sh"
        );
        return None;
    };
    Some(qualify_binary(&path).expect("pinned binary should qualify"))
}

fn run_bisync(req: &BisyncRequest) -> (Outcome, rclone_adapter::RunOutput) {
    let runner = RealProcessRunner;
    let handle = req
        .spawn(&runner, SpawnOptions::default())
        .expect("verified spawn should succeed");
    let run_out = handle.wait().expect("wait should succeed");
    let outcome = classify_run_outcome(&run_out);
    (outcome, run_out)
}

fn write_file(root: &Path, rel: &str, bytes: &[u8]) {
    let full = root.join(rel);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&full, bytes).unwrap();
}

/// Ensures modification times advance across filesystem timestamp granularity.
fn bump_mtime_wait() {
    thread::sleep(Duration::from_millis(1100));
}

fn tree_hashes(root: &Path) -> BTreeMap<String, String> {
    fn walk(base: &Path, current: &Path, out: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(current).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if rel == "RCLONE_TEST" {
                    continue;
                }
                out.insert(rel, sha256_file(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn assert_trees_match_truth(ws: &TempWorkspace, expected: &BTreeMap<String, String>) {
    let h1 = tree_hashes(&ws.path1);
    let h2 = tree_hashes(&ws.path2);
    assert_eq!(&h1, expected, "path1 does not match truth set");
    assert_eq!(&h2, expected, "path2 does not match truth set");
}

#[test]
fn initializes_with_path1_and_path2_preference_and_holds_on_second_cycle() {
    let Some(engine) = qualified_pinned_engine(
        "initializes_with_path1_and_path2_preference_and_holds_on_second_cycle",
    ) else {
        return;
    };

    // Case A: Path1 preference (local has data, path2 is empty).
    let mut ws1 = TempWorkspace::new("init-path1");
    let doc_a = b"path1-initial-upload-doc-alpha-001";
    let doc_b = b"path1-initial-upload-nested-beta-002";
    let mut truth1 = BTreeMap::new();
    truth1.insert("alpha.txt".to_string(), ws1.record_truth("p1_alpha", doc_a));
    truth1.insert(
        "sub/beta.txt".to_string(),
        ws1.record_truth("p1_beta", doc_b),
    );
    write_file(&ws1.path1, "alpha.txt", doc_a);
    write_file(&ws1.path1, "sub/beta.txt", doc_b);

    let (init1, _, _) = ws1.build_request(&engine, RunMode::Initialize(InitPreference::Path1), 10);
    let (outcome, _) = run_bisync(&init1);
    assert_eq!(outcome, Outcome::Success);
    assert_trees_match_truth(&ws1, &truth1);

    // Second ordinary cycle must stay clean and identical.
    let (cycle2, _, _) = ws1.build_request(&engine, RunMode::Normal, 10);
    let (outcome2, _) = run_bisync(&cycle2);
    assert_eq!(outcome2, Outcome::Success);
    assert_trees_match_truth(&ws1, &truth1);

    // Case B: Path2 preference (cloud/path2 has data, path1 is empty).
    let mut ws2 = TempWorkspace::new("init-path2");
    let cloud_a = b"path2-initial-download-gamma-003";
    let cloud_b = b"path2-initial-download-nested-delta-004";
    let mut truth2 = BTreeMap::new();
    truth2.insert(
        "gamma.txt".to_string(),
        ws2.record_truth("p2_gamma", cloud_a),
    );
    truth2.insert(
        "nested/delta.txt".to_string(),
        ws2.record_truth("p2_delta", cloud_b),
    );
    write_file(&ws2.path2, "gamma.txt", cloud_a);
    write_file(&ws2.path2, "nested/delta.txt", cloud_b);

    let (init2, _, _) = ws2.build_request(&engine, RunMode::Initialize(InitPreference::Path2), 10);
    let (outcome, _) = run_bisync(&init2);
    assert_eq!(outcome, Outcome::Success);
    assert_trees_match_truth(&ws2, &truth2);

    // Second ordinary cycle stays clean and identical.
    let (cycle2, _, _) = ws2.build_request(&engine, RunMode::Normal, 10);
    let (outcome2, _) = run_bisync(&cycle2);
    assert_eq!(outcome2, Outcome::Success);
    assert_trees_match_truth(&ws2, &truth2);
}

#[test]
fn propagates_add_edit_delete_both_directions_and_captures_backups() {
    let Some(engine) =
        qualified_pinned_engine("propagates_add_edit_delete_both_directions_and_captures_backups")
    else {
        return;
    };

    let mut ws = TempWorkspace::new("bidirectional-backups");
    let mut truth = BTreeMap::new();
    // Seed 12 files so deleting 1 file per side stays well below the 10% delete guard.
    for i in 0..12 {
        let rel = format!("item_{i:02}.txt");
        let payload = format!("initial-seed-payload-for-item-{i:02}-v1");
        truth.insert(
            rel.clone(),
            ws.record_truth(&format!("seed_{i:02}"), payload.as_bytes()),
        );
        write_file(&ws.path1, &rel, payload.as_bytes());
    }

    let (init_req, _, _) =
        ws.build_request(&engine, RunMode::Initialize(InitPreference::Path1), 10);
    let (outcome, _) = run_bisync(&init_req);
    assert_eq!(outcome, Outcome::Success);
    assert_trees_match_truth(&ws, &truth);

    bump_mtime_wait();

    // Record original hashes of files that will be overwritten or deleted so we can verify backups.
    let orig_item_00_hash = truth["item_00.txt"].clone();
    let orig_item_01_hash = truth["item_01.txt"].clone();
    let orig_item_02_hash = truth["item_02.txt"].clone();
    let orig_item_03_hash = truth["item_03.txt"].clone();

    // Path1 changes: add new_from_p1.txt, edit item_00.txt, delete item_01.txt.
    let new_p1 = b"brand-new-file-created-on-path1";
    let edit_00 = b"edited-on-path1-version-2-longer-content";
    write_file(&ws.path1, "new_from_p1.txt", new_p1);
    write_file(&ws.path1, "item_00.txt", edit_00);
    fs::remove_file(ws.path1.join("item_01.txt")).unwrap();

    // Path2 changes: add new_from_p2.txt, edit item_02.txt, delete item_03.txt.
    let new_p2 = b"brand-new-file-created-on-path2";
    let edit_02 = b"edited-on-path2-version-2-longer-content";
    write_file(&ws.path2, "new_from_p2.txt", new_p2);
    write_file(&ws.path2, "item_02.txt", edit_02);
    fs::remove_file(ws.path2.join("item_03.txt")).unwrap();

    // Update truth set.
    truth.insert(
        "new_from_p1.txt".to_string(),
        ws.record_truth("new_p1", new_p1),
    );
    truth.insert(
        "item_00.txt".to_string(),
        ws.record_truth("edit_00", edit_00),
    );
    truth.remove("item_01.txt");
    truth.insert(
        "new_from_p2.txt".to_string(),
        ws.record_truth("new_p2", new_p2),
    );
    truth.insert(
        "item_02.txt".to_string(),
        ws.record_truth("edit_02", edit_02),
    );
    truth.remove("item_03.txt");

    let (run_req, b1, b2) = ws.build_request(&engine, RunMode::Normal, 20);
    let (outcome, _) = run_bisync(&run_req);
    assert_eq!(outcome, Outcome::Success);
    assert_trees_match_truth(&ws, &truth);

    // Verify --backup-dir1 captured Path1's overwritten (item_02.txt) and deleted (item_03.txt) copies.
    assert_eq!(
        sha256_file(&b1.join("item_02.txt")).unwrap(),
        orig_item_02_hash
    );
    assert_eq!(
        sha256_file(&b1.join("item_03.txt")).unwrap(),
        orig_item_03_hash
    );

    // Verify --backup-dir2 captured Path2's overwritten (item_00.txt) and deleted (item_01.txt) copies.
    assert_eq!(
        sha256_file(&b2.join("item_00.txt")).unwrap(),
        orig_item_00_hash
    );
    assert_eq!(
        sha256_file(&b2.join("item_01.txt")).unwrap(),
        orig_item_01_hash
    );

    // Second cycle must keep the truth set intact without resurrecting deleted files.
    let (cycle2, _, _) = ws.build_request(&engine, RunMode::Normal, 20);
    let (outcome2, _) = run_bisync(&cycle2);
    assert_eq!(outcome2, Outcome::Success);
    assert_trees_match_truth(&ws, &truth);
}

#[test]
fn preserves_both_versions_on_same_file_conflict_and_reports_exact_conflict_paths() {
    let Some(engine) = qualified_pinned_engine(
        "preserves_both_versions_on_same_file_conflict_and_reports_exact_conflict_paths",
    ) else {
        return;
    };

    let mut ws = TempWorkspace::new("conflict");
    // Exercise a filename containing ` - ` (conflict path parser boundary) alongside a benign
    // filename containing `must run --resync` (prefix-only diagnostic parser boundary).
    let conflict_rel = "notes/draft - final.txt";
    let benign_rel = "notes/must run --resync.txt";
    let v0 = b"shared-initial-document-v0";
    let benign_v0 = b"benign-v0";
    write_file(&ws.path1, conflict_rel, v0);
    write_file(&ws.path1, benign_rel, benign_v0);
    ws.record_truth("v0", v0);

    let (init_req, _, _) =
        ws.build_request(&engine, RunMode::Initialize(InitPreference::Path1), 10);
    let (outcome, _) = run_bisync(&init_req);
    assert_eq!(outcome, Outcome::Success);

    bump_mtime_wait();

    let v1_p1 = b"path1-concurrent-edit-unique-payload-11111";
    let v1_p2 = b"path2-concurrent-edit-unique-payload-22222-extra";
    let benign_v1 = b"benign-updated-on-path1";
    let hash_p1 = ws.record_truth("v1_p1", v1_p1);
    let hash_p2 = ws.record_truth("v1_p2", v1_p2);
    let hash_benign = ws.record_truth("benign_v1", benign_v1);
    write_file(&ws.path1, conflict_rel, v1_p1);
    write_file(&ws.path2, conflict_rel, v1_p2);
    write_file(&ws.path1, benign_rel, benign_v1);

    let (conflict_req, _, _) = ws.build_request(&engine, RunMode::Normal, 10);
    let (outcome, _) = run_bisync(&conflict_req);
    let Outcome::SuccessWithConflicts { paths } = outcome else {
        panic!("expected SuccessWithConflicts, got {outcome:?}");
    };
    let expected_c1 = ws
        .path1
        .join("notes/draft - final.txt.conflict1")
        .to_string_lossy()
        .into_owned();
    let expected_c2 = ws
        .path2
        .join("notes/draft - final.txt.conflict2")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        paths,
        vec![expected_c1, expected_c2],
        "reported conflict paths must match exact filenames containing ' - '"
    );

    // Verify both conflict versions and the benign file exist on BOTH path1 and path2 by hash.
    let h1 = tree_hashes(&ws.path1);
    let h2 = tree_hashes(&ws.path2);
    assert_eq!(h1, h2);
    assert_eq!(
        h1.get("notes/draft - final.txt.conflict1"),
        Some(&hash_p1),
        "path1 version mismatch: {h1:?}"
    );
    assert_eq!(
        h1.get("notes/draft - final.txt.conflict2"),
        Some(&hash_p2),
        "path2 version mismatch: {h1:?}"
    );
    assert_eq!(h1.get(benign_rel), Some(&hash_benign));

    // Second cycle must keep both preserved conflict copies intact.
    let (cycle2, _, _) = ws.build_request(&engine, RunMode::Normal, 10);
    let (outcome2, _) = run_bisync(&cycle2);
    assert_eq!(outcome2, Outcome::Success);
    assert_eq!(tree_hashes(&ws.path1), h1);
    assert_eq!(tree_hashes(&ws.path2), h2);
}

#[test]
fn aborts_when_deletions_exceed_percent_limit_and_deletes_nothing() {
    let Some(engine) =
        qualified_pinned_engine("aborts_when_deletions_exceed_percent_limit_and_deletes_nothing")
    else {
        return;
    };

    let mut ws = TempWorkspace::new("max-delete");
    let mut truth = BTreeMap::new();
    for i in 0..10 {
        let rel = format!("doc_{i:02}.txt");
        let payload = format!("protected-content-{i:02}");
        truth.insert(rel.clone(), ws.record_truth(&rel, payload.as_bytes()));
        write_file(&ws.path1, &rel, payload.as_bytes());
    }

    let (init_req, _, _) =
        ws.build_request(&engine, RunMode::Initialize(InitPreference::Path1), 10);
    let (outcome, _) = run_bisync(&init_req);
    assert_eq!(outcome, Outcome::Success);
    assert_trees_match_truth(&ws, &truth);

    // Delete 4 of 10 files (40% > 10% limit) on Path1.
    for i in 0..4 {
        fs::remove_file(ws.path1.join(format!("doc_{i:02}.txt"))).unwrap();
    }

    let (abort_req, _, _) = ws.build_request(&engine, RunMode::Normal, 10);
    let (outcome, _) = run_bisync(&abort_req);
    assert!(
        matches!(outcome, Outcome::DeletionGuardAborted { .. }),
        "expected DeletionGuardAborted, got {outcome:?}"
    );

    // Path2 must still contain ALL 10 files with exact original hashes.
    assert_eq!(tree_hashes(&ws.path2), truth);

    // Restoring the 4 files locally and running a normal cycle must succeed cleanly.
    for i in 0..4 {
        let rel = format!("doc_{i:02}.txt");
        let payload = format!("protected-content-{i:02}");
        write_file(&ws.path1, &rel, payload.as_bytes());
    }
    let (recover_req, _, _) = ws.build_request(&engine, RunMode::Normal, 10);
    let (outcome2, _) = run_bisync(&recover_req);
    assert_eq!(outcome2, Outcome::Success);
    assert_trees_match_truth(&ws, &truth);
}

#[test]
fn detects_missing_access_check_marker_without_propagating_deletes() {
    let Some(engine) =
        qualified_pinned_engine("detects_missing_access_check_marker_without_propagating_deletes")
    else {
        return;
    };

    let mut ws = TempWorkspace::new("access-check");
    let mut truth = BTreeMap::new();
    let payload = b"guarded-by-check-access";
    truth.insert("keep.txt".to_string(), ws.record_truth("keep", payload));
    write_file(&ws.path1, "keep.txt", payload);

    let (init_req, _, _) =
        ws.build_request(&engine, RunMode::Initialize(InitPreference::Path1), 10);
    let (outcome, _) = run_bisync(&init_req);
    assert_eq!(outcome, Outcome::Success);

    // Remove the access marker from Path2 to simulate an unmounted or wrong destination.
    fs::remove_file(ws.path2.join("RCLONE_TEST")).unwrap();
    let (fail_req, _, _) = ws.build_request(&engine, RunMode::Normal, 10);
    let (outcome, _) = run_bisync(&fail_req);
    assert!(
        matches!(outcome, Outcome::AccessCheckFailed { .. }),
        "expected AccessCheckFailed, got {outcome:?}"
    );
    assert_trees_match_truth(&ws, &truth);
}

#[test]
fn sigint_interrupts_larger_run_and_records_subsequent_recovery_behavior() {
    let Some(engine) = qualified_pinned_engine(
        "sigint_interrupts_larger_run_and_records_subsequent_recovery_behavior",
    ) else {
        return;
    };

    const TIMEOUT: Duration = Duration::from_secs(120);
    let mut file_count = 800;
    let (mut ws, truth) = loop {
        let mut ws = TempWorkspace::new("sigint-recovery");
        // Initialize an empty pair so this is an ordinary `--resilient --recover` run.
        let (init_req, _, _) =
            ws.build_request(&engine, RunMode::Initialize(InitPreference::Path1), 10);
        let (outcome, _) = run_bisync(&init_req);
        assert_eq!(outcome, Outcome::Success);

        let mut truth = BTreeMap::new();
        let mut bytes = vec![0u8; 32 * 1024];
        for (idx, byte) in bytes.iter_mut().enumerate() {
            *byte = (idx % 251) as u8;
        }
        for i in 0..file_count {
            let rel = format!("blob_{i:04}.bin");
            let header = format!("FS-UNIQUE-BLOB-{i:04}-");
            bytes[..header.len()].copy_from_slice(header.as_bytes());
            truth.insert(rel.clone(), sha256_bytes(&bytes));
            write_file(&ws.path1, &rel, &bytes);
        }

        let (large_req, _, _) = ws.build_request(&engine, RunMode::Normal, 10);
        let handle = large_req
            .spawn(&RealProcessRunner, SpawnOptions::default())
            .expect("spawn large run");
        let stopper = handle.stop_handle();
        let waiter = thread::spawn(move || handle.wait().expect("wait large run"));

        // Count completed destination files, excluding rclone's temporary files and the marker.
        let started = Instant::now();
        let mut stop = None;
        while !waiter.is_finished() {
            if started.elapsed() >= TIMEOUT {
                stopper.force_stop(Duration::from_secs(2)).unwrap();
                let run_out = waiter.join().unwrap();
                panic!(
                    "timed out waiting for partial destination progress: {:?}",
                    run_out.exit_status
                );
            }
            let copied = fs::read_dir(&ws.path2)
                .unwrap()
                .filter(|entry| {
                    entry
                        .as_ref()
                        .unwrap()
                        .path()
                        .extension()
                        .is_some_and(|ext| ext == "bin")
                })
                .count();
            if (1..file_count).contains(&copied) {
                eprintln!(
                    "SIGINT at {copied}/{file_count} copied files after {:?}",
                    started.elapsed()
                );
                stop = Some(stopper.request_stop());
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        let run_out = waiter.join().unwrap();
        let outcome = classify_run_outcome(&run_out);
        let sent_stop = stop
            .map(|result| result.expect("SIGINT should succeed"))
            .is_some();
        eprintln!(
            "SIGINT observation: exit={:?}, outcome={outcome:?}",
            run_out.exit_status
        );
        if outcome == Outcome::Success && file_count == 800 {
            eprintln!("The {file_count}-file run completed before it could be interrupted");
            file_count = 3200;
            continue;
        }
        assert!(
            sent_stop,
            "expected partial progress and SIGINT, got {outcome:?}"
        );
        assert!(
            matches!(
                outcome,
                Outcome::Interrupted {
                    needs_recovery: true,
                    ..
                }
            ),
            "expected Outcome::Interrupted {{ needs_recovery: true, .. }}, got {outcome:?}"
        );
        assert!(
            run_out.exit_status.signal == Some(libc::SIGINT)
                || run_out.exit_status.code == Some(128 + libc::SIGINT)
                || run_out.output.diagnostic_events.iter().any(|event| {
                    matches!(event, LogEvent::Structured(ev) if matches!(ev.marker, Some(BisyncMarker::SignalReceived { .. })))
                }),
            "rclone did not report receiving SIGINT"
        );
        break (ws, truth);
    };

    // Next run with normal `--resilient --recover` should recover and finish propagating the batch.
    let (next_req, _, _) = ws.build_request(&engine, RunMode::Normal, 10);
    let (next_outcome, next_out) = run_bisync(&next_req);
    eprintln!(
        "Post-SIGINT next run: exit={:?}, outcome={next_outcome:?}",
        next_out.exit_status
    );
    // Timing-dependent: if SIGINT lands before rclone saved the prior listings, the pinned engine
    // refuses `--recover` with "cannot find prior Path1 or Path2 listings". That is the intended
    // hold (RecoveryRequired; reinitialization is a separate reviewed workflow), not a failure.
    // In that case verify nothing was lost or corrupted and stop here.
    if let Outcome::Critical {
        requires_resync: true,
        reason,
    } = &next_outcome
    {
        assert!(
            reason.contains("cannot find prior Path1 or Path2 listings"),
            "unexpected critical outcome after SIGINT: {reason}"
        );
        assert_eq!(
            tree_hashes(&ws.path1),
            truth,
            "path1 must keep every original file after an interrupted run"
        );
        for (rel, hash) in tree_hashes(&ws.path2) {
            assert_eq!(
                truth.get(&rel),
                Some(&hash),
                "path2 holds an unexpected or corrupted file: {rel}"
            );
        }
        return;
    }
    assert_eq!(
        next_outcome,
        Outcome::Success,
        "next --resilient --recover run should complete cleanly after graceful SIGINT"
    );
    assert_trees_match_truth(&ws, &truth);

    // Second clean cycle after recovery must remain exact.
    let (verify_req, _, _) = ws.build_request(&engine, RunMode::Normal, 10);
    let (verify_outcome, _) = run_bisync(&verify_req);
    assert_eq!(verify_outcome, Outcome::Success);
    assert_trees_match_truth(&ws, &truth);
}

#[test]
fn env_isolation_subproc_helper() {
    if env::var("FOLDERSYNC_IT_SUBPROC").as_deref() != Ok("env_isolation") {
        return;
    }
    assert_eq!(env::var("RCLONE_FORCE").as_deref(), Ok("true"));
    assert_eq!(env::var("RCLONE_RESYNC").as_deref(), Ok("true"));

    let engine = qualified_pinned_engine("env_isolation_subproc_helper")
        .expect("parent already checked binary presence");

    // 1. Fresh uninitialized workdir with RunMode::Normal MUST fail with requires_resync=true,
    // proving parent RCLONE_RESYNC=true never reached rclone.
    let mut ws = TempWorkspace::new("env-isolation");
    let mut truth = BTreeMap::new();
    for i in 0..10 {
        let rel = format!("doc_{i:02}.txt");
        let payload = format!("isolated-env-content-{i:02}");
        truth.insert(rel.clone(), ws.record_truth(&rel, payload.as_bytes()));
        write_file(&ws.path1, &rel, payload.as_bytes());
    }
    let (normal_uninit, _, _) = ws.build_request(&engine, RunMode::Normal, 10);
    let (uninit_outcome, _) = run_bisync(&normal_uninit);
    assert!(
        matches!(
            uninit_outcome,
            Outcome::Critical {
                requires_resync: true,
                ..
            }
        ),
        "RCLONE_RESYNC=true in parent env must not trigger resync; got {uninit_outcome:?}"
    );
    assert!(
        tree_hashes(&ws.path2).is_empty(),
        "path2 must remain empty after blocked uninitialized normal run"
    );

    // 2. Initialize legitimately, then delete 4 of 10 files (40% > 10%). Normal run MUST abort
    // with DeletionGuardAborted, proving parent RCLONE_FORCE=true never reached rclone.
    let (init_req, _, _) =
        ws.build_request(&engine, RunMode::Initialize(InitPreference::Path1), 10);
    let (init_outcome, _) = run_bisync(&init_req);
    assert_eq!(init_outcome, Outcome::Success);
    assert_trees_match_truth(&ws, &truth);

    for i in 0..4 {
        fs::remove_file(ws.path1.join(format!("doc_{i:02}.txt"))).unwrap();
    }
    let (del_req, _, _) = ws.build_request(&engine, RunMode::Normal, 10);
    let (del_outcome, _) = run_bisync(&del_req);
    assert!(
        matches!(del_outcome, Outcome::DeletionGuardAborted { .. }),
        "RCLONE_FORCE=true in parent env must not bypass --max-delete; got {del_outcome:?}"
    );
    assert_eq!(tree_hashes(&ws.path2), truth);
}

#[test]
fn parent_rclone_force_and_resync_env_cannot_bypass_guards() {
    if qualified_pinned_engine("parent_rclone_force_and_resync_env_cannot_bypass_guards").is_none()
    {
        return;
    }

    let mut cmd = Command::new(env::current_exe().unwrap());
    cmd.args(["--exact", "env_isolation_subproc_helper", "--nocapture"])
        .env("FOLDERSYNC_IT_SUBPROC", "env_isolation")
        .env("RCLONE_FORCE", "true")
        .env("RCLONE_RESYNC", "true")
        .stdin(Stdio::null());
    if let Some(override_bin) = env::var_os("FOLDERSYNC_RCLONE") {
        cmd.env("FOLDERSYNC_RCLONE", override_bin);
    }
    let output = cmd.output().expect("subprocess should run");
    assert!(
        output.status.success(),
        "env isolation subprocess failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn refuses_launch_when_pinned_binary_is_replaced_after_qualification() {
    let Some(real_path) = pinned_binary_path() else {
        eprintln!(
            "Skipping refuses_launch_when_pinned_binary_is_replaced_after_qualification: pinned rclone binary is absent; run scripts/fetch-rclone.sh"
        );
        return;
    };

    let mut ws = TempWorkspace::new("replace-after-qual");
    let copied_bin = ws.root.join("rclone-copy");
    fs::copy(&real_path, &copied_bin).unwrap();

    let engine = qualify_binary(&copied_bin).unwrap();
    let (req, _, _) = ws.build_request(&engine, RunMode::Initialize(InitPreference::Path1), 10);

    // Replace the copied binary AFTER qualification succeeded.
    fs::write(&copied_bin, b"replaced binary bytes after qualification").unwrap();
    let err = req
        .spawn(&RealProcessRunner, SpawnOptions::default())
        .unwrap_err();
    assert!(
        matches!(
            err,
            LaunchError::Process(ProcessError::BinaryIntegrityMismatch { .. })
        ),
        "expected the process runner to refuse the replacement before spawn, got {err:?}"
    );
}

#[test]
fn demonstrates_same_size_same_mtime_edit_is_missed_by_size_modtime_compare() {
    let Some(engine) = qualified_pinned_engine(
        "demonstrates_same_size_same_mtime_edit_is_missed_by_size_modtime_compare",
    ) else {
        return;
    };

    let mut ws = TempWorkspace::new("same-size-same-mtime");
    let original = b"AAAA-1111";
    let mutated = b"BBBB-2222";
    assert_eq!(original.len(), mutated.len());
    write_file(&ws.path1, "fixed.txt", original);

    let (init_req, _, _) =
        ws.build_request(&engine, RunMode::Initialize(InitPreference::Path1), 10);
    let (init_outcome, _) = run_bisync(&init_req);
    assert_eq!(init_outcome, Outcome::Success);
    assert_eq!(fs::read(ws.path2.join("fixed.txt")).unwrap(), original);

    // Mutate content in place while preserving BOTH file size and modification timestamp (§10).
    let p1_file = ws.path1.join("fixed.txt");
    let orig_mtime = fs::metadata(&p1_file).unwrap().modified().unwrap();
    fs::write(&p1_file, mutated).unwrap();
    File::options()
        .write(true)
        .open(&p1_file)
        .unwrap()
        .set_modified(orig_mtime)
        .unwrap();

    // Under `--compare size,modtime`, rclone does not select the file for transfer.
    let (sync_req, _, _) = ws.build_request(&engine, RunMode::Normal, 10);
    let (sync_outcome, _) = run_bisync(&sync_req);
    assert_eq!(sync_outcome, Outcome::Success);
    assert_eq!(
        fs::read(&p1_file).unwrap(),
        mutated,
        "path1 retains the in-place same-size/same-mtime edit"
    );
    assert_eq!(
        fs::read(ws.path2.join("fixed.txt")).unwrap(),
        original,
        "known §10 limitation: --compare size,modtime does not detect a change preserving both size and mtime"
    );
}
