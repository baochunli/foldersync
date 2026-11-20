//! Local-to-local integration tests exercising the pinned rclone executable.
//!
//! All tests run inside fresh temporary directories with an explicit throwaway
//! `--config` file and skip cleanly when the pinned binary is absent.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, SystemTime},
};

use rclone_adapter::{
    BackupDir2, BisyncRequest, EngineIdentity, InitPreference, Outcome, Path2, ProcessRunner,
    QualificationManifest, RealProcessRunner, RunMode, SpawnOptions, classify_outcome,
    parse_log_stream, pinned_binary_path, qualify_binary, sha256_file,
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
    let manifest = QualificationManifest::embedded().expect("embedded manifest should parse");
    Some(qualify_binary(&path, &manifest).expect("pinned binary should qualify"))
}

fn run_bisync(req: &BisyncRequest) -> (Outcome, rclone_adapter::RunOutput) {
    let runner = RealProcessRunner;
    let handle = runner
        .spawn(&req.to_process_spec(), SpawnOptions::default())
        .expect("spawn should succeed");
    let run_out = handle.wait().expect("wait should succeed");
    let stderr_lines: Vec<&str> = run_out
        .output
        .lines
        .iter()
        .filter(|l| l.stream == rclone_adapter::StreamKind::Stderr)
        .map(|l| l.text.as_str())
        .collect();
    let events = parse_log_stream(stderr_lines);
    let outcome = classify_outcome(run_out.exit_status, &events, run_out.stop_requested);
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
fn preserves_both_versions_on_same_file_conflict_and_reports_conflict_outcome() {
    let Some(engine) = qualified_pinned_engine(
        "preserves_both_versions_on_same_file_conflict_and_reports_conflict_outcome",
    ) else {
        return;
    };

    let mut ws = TempWorkspace::new("conflict");
    let v0 = b"shared-initial-document-v0";
    write_file(&ws.path1, "notes.txt", v0);
    ws.record_truth("v0", v0);

    let (init_req, _, _) =
        ws.build_request(&engine, RunMode::Initialize(InitPreference::Path1), 10);
    let (outcome, _) = run_bisync(&init_req);
    assert_eq!(outcome, Outcome::Success);

    bump_mtime_wait();

    let v1_p1 = b"path1-concurrent-edit-unique-payload-11111";
    let v1_p2 = b"path2-concurrent-edit-unique-payload-22222-extra";
    let hash_p1 = ws.record_truth("v1_p1", v1_p1);
    let hash_p2 = ws.record_truth("v1_p2", v1_p2);
    write_file(&ws.path1, "notes.txt", v1_p1);
    write_file(&ws.path2, "notes.txt", v1_p2);

    let (conflict_req, _, _) = ws.build_request(&engine, RunMode::Normal, 10);
    let (outcome, _) = run_bisync(&conflict_req);
    let Outcome::SuccessWithConflicts { paths } = outcome else {
        panic!("expected SuccessWithConflicts, got {outcome:?}");
    };
    assert!(
        paths.len() >= 2,
        "expected both conflict copies reported, got {paths:?}"
    );

    // Verify both versions exist on BOTH path1 and path2 by content hash.
    let h1 = tree_hashes(&ws.path1);
    let h2 = tree_hashes(&ws.path2);
    assert_eq!(h1, h2);
    let values: Vec<&String> = h1.values().collect();
    assert!(values.contains(&&hash_p1), "path1 version lost: {h1:?}");
    assert!(values.contains(&&hash_p2), "path2 version lost: {h1:?}");

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

    let mut ws = TempWorkspace::new("sigint-recovery");
    // Initialize empty pair first so the large run is an ordinary `--resilient --recover` run.
    let (init_req, _, _) =
        ws.build_request(&engine, RunMode::Initialize(InitPreference::Path1), 10);
    let (outcome, _) = run_bisync(&init_req);
    assert_eq!(outcome, Outcome::Success);

    // Populate a larger batch (400 x 32 KiB = ~12.8 MiB) of uniquely identifiable files so SIGINT
    // reliably lands mid-run when triggered on the first queuing/transfer log event without slowing CI.
    let mut truth = BTreeMap::new();
    let mut template = vec![0u8; 32 * 1024];
    for (idx, byte) in template.iter_mut().enumerate() {
        *byte = (idx % 251) as u8;
    }
    for i in 0..400u32 {
        let rel = format!("batch_{:02}/blob_{:03}.bin", i / 50, i);
        let mut bytes = template.clone();
        let header = format!("FS-UNIQUE-BLOB-{i:04}-");
        bytes[..header.len()].copy_from_slice(header.as_bytes());
        truth.insert(rel.clone(), sha256_bytes(&bytes));
        write_file(&ws.path1, &rel, &bytes);
    }

    let (large_req, _, _) = ws.build_request(&engine, RunMode::Normal, 10);
    let runner = RealProcessRunner;
    let (tx, rx) = mpsc::channel();
    let handle = runner
        .spawn(
            &large_req.to_process_spec(),
            SpawnOptions {
                limits: Default::default(),
                line_observer: Some(tx),
            },
        )
        .expect("spawn large run");
    let stopper = handle.stop_handle();
    let started_at = SystemTime::now();
    let waiter = thread::spawn(move || handle.wait().expect("wait large run"));

    // Send SIGINT once as soon as rclone begins queuing or transferring files.
    let mut sent_stop = false;
    while let Ok(line) = rx.recv_timeout(Duration::from_secs(15)) {
        if line.text.contains("Queue copy to")
            || line.text.contains("Applying changes")
            || line.text.contains("Copied")
        {
            stopper.request_stop().expect("SIGINT should succeed");
            sent_stop = true;
            break;
        }
    }
    assert!(
        sent_stop,
        "expected rclone to emit queuing/transfer log lines before finishing"
    );

    let run_out = waiter.join().unwrap();
    let stderr_lines: Vec<&str> = run_out
        .output
        .lines
        .iter()
        .filter(|l| l.stream == rclone_adapter::StreamKind::Stderr)
        .map(|l| l.text.as_str())
        .collect();
    let events = parse_log_stream(stderr_lines);
    let outcome = classify_outcome(run_out.exit_status, &events, run_out.stop_requested);
    let elapsed = started_at.elapsed().unwrap_or_default();

    eprintln!(
        "SIGINT observation: elapsed={elapsed:?}, exit={:?}, outcome={outcome:?}",
        run_out.exit_status
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

    // Next run with normal `--resilient --recover` should recover and finish propagating all 350 files.
    let (next_req, _, _) = ws.build_request(&engine, RunMode::Normal, 10);
    let (next_outcome, next_out) = run_bisync(&next_req);
    eprintln!(
        "Post-SIGINT next run: exit={:?}, outcome={next_outcome:?}",
        next_out.exit_status
    );
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
