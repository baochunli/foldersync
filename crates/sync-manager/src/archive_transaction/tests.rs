use super::*;
use std::{
    cell::Cell,
    io::{Read, Seek, SeekFrom, Write},
    sync::atomic::{AtomicBool, Ordering},
};

use crate::archive_writer::{verify_zip, write_zip};
use sync_core::archive::ArchiveFile;

struct Fixture {
    _scratch: tempfile::TempDir,
    volume: PathBuf,
    root: PathBuf,
    holding: PathBuf,
    journal: ArchiveJournal,
}

impl Fixture {
    fn new() -> Self {
        let scratch = tempfile::tempdir().unwrap();
        let volume = scratch.path().canonicalize().unwrap();
        let root = volume.join("data");
        let state = volume.join("state");
        fs::create_dir_all(root.join("old/proj/.git/objects")).unwrap();
        fs::write(root.join("old/proj/.git/objects/object"), b"original").unwrap();
        fs::create_dir(&state).unwrap();
        let holding = check_archive_paths(&root, &volume, "collection", "old").unwrap();
        assert_eq!(
            holding,
            volume.join("FolderSync Archived Originals/collection/old")
        );
        assert!(!holding.starts_with(&root));
        Self {
            _scratch: scratch,
            volume,
            root,
            holding,
            journal: ArchiveJournal::new(&state, &MappingId::new("fixture")),
        }
    }

    fn untouched(&self) {
        assert_eq!(
            fs::read(self.root.join("old/proj/.git/objects/object")).unwrap(),
            b"original"
        );
        assert!(!self.holding.exists());
        assert!(!self.root.join("old.zip").exists());
        assert!(!self.root.join("old.zip.foldersync-partial").exists());
    }
}

#[test]
fn verification_failure_keeps_original_and_cleans_partial() {
    let f = Fixture::new();
    let source = "old/photo.jpg";
    let bytes = b"unique stored ZIP entry";
    fs::write(f.root.join(source), bytes).unwrap();
    let files =
        [
            ArchiveFile::from_metadata(source.into(), &fs::metadata(f.root.join(source)).unwrap())
                .unwrap(),
        ];
    let result = f
        .journal
        .archive(&f.root, &f.holding, "old", 50, &|| false, |out| {
            write_zip(&f.root, &files, &mut *out, |_| Ok(())).unwrap();
            out.rewind().unwrap();
            let mut archive = Vec::new();
            out.read_to_end(&mut archive).unwrap();
            let offset = archive
                .windows(bytes.len())
                .position(|chunk| chunk == bytes)
                .unwrap();
            out.seek(SeekFrom::Start(offset as u64)).unwrap();
            out.write_all(&[bytes[0] ^ 1]).unwrap();
            verify_zip(out, &files, |_| Ok(()))
                .map_err(|error| io_error("check archive CRC", error))?;
            Ok(bytes.len() as u64)
        });
    assert!(result.unwrap_err().cause().contains("checksum"));
    f.untouched();
    assert_eq!(fs::read(f.root.join(source)).unwrap(), bytes);
    f.journal.recover(&f.root).unwrap();
    assert!(f.journal.originals().unwrap().is_empty());
}

#[test]
fn stop_before_creation_and_after_writing_keeps_original() {
    let f = Fixture::new();
    let result = f
        .journal
        .archive(&f.root, &f.holding, "old", 50, &|| true, |_| {
            panic!("already canceled before building")
        });
    assert!(result.is_err());
    f.untouched();

    let polls = Cell::new(0);
    let result = f.journal.archive(
        &f.root,
        &f.holding,
        "old",
        50,
        &|| {
            polls.set(polls.get() + 1);
            polls.get() == 4
        },
        |out| {
            out.write_all(b"verified archive").unwrap();
            Ok(8)
        },
    );
    assert!(result.is_err());
    assert_eq!(polls.get(), 4);
    f.untouched();

    let canceled = AtomicBool::new(false);
    let result = f.journal.archive(
        &f.root,
        &f.holding,
        "old",
        50,
        &|| canceled.load(Ordering::SeqCst),
        |out| {
            out.write_all(b"verified archive").unwrap();
            canceled.store(true, Ordering::SeqCst);
            Ok(8)
        },
    );
    assert!(result.is_err());
    f.untouched();
}

#[test]
fn occupied_paths_and_holding_symlinks_fail_closed() {
    let f = Fixture::new();
    let final_path = f.root.join("old.zip");
    fs::write(&final_path, b"owner data").unwrap();
    assert!(check_archive_paths(&f.root, &f.volume, "collection", "old").is_err());
    fs::remove_file(final_path).unwrap();
    fs::create_dir_all(&f.holding).unwrap();
    assert!(check_archive_paths(&f.root, &f.volume, "collection", "old").is_err());
    fs::remove_dir(&f.holding).unwrap();
    let parent = f.holding.parent().unwrap();
    fs::remove_dir(parent).unwrap();
    std::os::unix::fs::symlink(&f.root, parent).unwrap();
    assert!(check_archive_paths(&f.root, &f.volume, "collection", "old").is_err());
    fs::remove_file(parent).unwrap();
    f.untouched();
}

#[test]
fn space_shortage_and_invalid_holding_volume_fail_before_writing() {
    let f = Fixture::new();
    let err = f
        .journal
        .archive(&f.root, &f.holding, "old", u64::MAX, &|| false, |_| {
            panic!("archive build must wait for sufficient space")
        })
        .unwrap_err();
    assert!(err.cause().contains("bytes available"));
    assert!(err.cause().contains("estimated ZIP bytes"));
    assert!(check_archive_paths(&f.root, &f.root, "collection", "old").is_err());
    let root = Identity {
        device: 1,
        inode: 10,
    };
    let different_device = Identity {
        device: 2,
        inode: 10,
    };
    assert!(
        same_volume(root, different_device)
            .unwrap_err()
            .cause()
            .contains("same volume")
    );
    f.untouched();
}

#[test]
fn a_collision_after_review_leaves_foreign_files_untouched() {
    let f = Fixture::new();
    let final_path = f.root.join("old.zip");
    let err = f
        .journal
        .archive(&f.root, &f.holding, "old", 50, &|| false, |out| {
            out.write_all(b"our archive").unwrap();
            fs::write(&final_path, b"foreign ZIP").unwrap();
            Ok(8)
        })
        .unwrap_err();
    assert!(err.cause().contains("already exists"));
    assert_eq!(fs::read(&final_path).unwrap(), b"foreign ZIP");
    fs::remove_file(final_path).unwrap();
    f.untouched();
}

fn interrupted(f: &Fixture, moved: bool, published: bool) {
    let (root_path, root) = checked_root(&f.root).unwrap();
    let holding = checked_holding(&root_path, &root, &f.holding, "old", true)
        .unwrap()
        .unwrap();
    let names = Names::new("old").unwrap();
    let mut partial = create_partial(&root, &names.partial).unwrap();
    partial.write_all(b"verified archive bytes").unwrap();
    partial.sync_all().unwrap();
    let source = check_source(&root, "old").unwrap();
    f.journal
        .save(&Records::from([(
            "old".to_owned(),
            Record {
                original: ArchivedOriginal {
                    subfolder: "old".to_owned(),
                    path: f.holding.clone(),
                    bytes: 8,
                    present: false,
                },
                root: root_path,
                root_id: identity(&root).unwrap(),
                source,
                phase: Phase::Verified {
                    partial: Stamp::for_file(&partial).unwrap(),
                    holding_parent: identity(&holding).unwrap(),
                },
            },
        )]))
        .unwrap();
    if moved {
        rename_exclusive(&root, "old", &holding, "old").unwrap();
    }
    if published {
        rename_exclusive(&root, &names.partial, &root, &names.final_name).unwrap();
    }
}

#[test]
fn restart_resolves_each_rename_boundary_and_keeps_owned_bytes() {
    let before_move = Fixture::new();
    interrupted(&before_move, false, false);
    assert!(before_move.journal.has_pending().unwrap());
    before_move.journal.recover(&before_move.root).unwrap();
    before_move.untouched();
    assert!(!before_move.journal.has_pending().unwrap());

    for published in [false, true] {
        let f = Fixture::new();
        interrupted(&f, true, published);
        f.journal.recover(&f.root).unwrap();
        f.journal.recover(&f.root).unwrap();
        assert!(!f.journal.has_pending().unwrap());
        assert!(!f.root.join("old").exists());
        assert!(!f.root.join("old.zip.foldersync-partial").exists());
        assert_eq!(
            fs::read(f.root.join("old.zip")).unwrap(),
            b"verified archive bytes"
        );
        assert_eq!(
            fs::read(f.holding.join("proj/.git/objects/object")).unwrap(),
            b"original"
        );
        let records = f.journal.originals().unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].present);
        assert_eq!(records[0].bytes, 8);
        fs::remove_dir_all(&f.holding).unwrap();
        let records = f.journal.originals().unwrap();
        assert_eq!(records.len(), 1);
        assert!(!records[0].present);
    }
}

#[test]
fn restart_rolls_back_the_original_if_a_foreign_zip_blocks_publication() {
    let f = Fixture::new();
    interrupted(&f, true, false);
    let final_path = f.root.join("old.zip");
    fs::write(&final_path, b"foreign ZIP").unwrap();
    f.journal.recover(&f.root).unwrap();
    assert_eq!(fs::read(&final_path).unwrap(), b"foreign ZIP");
    fs::remove_file(final_path).unwrap();
    f.untouched();
}

#[test]
fn restart_refuses_a_replacement_mapping_root_and_preserves_holding() {
    let f = Fixture::new();
    interrupted(&f, true, false);
    let moved_root = f.volume.join("displaced");
    fs::rename(&f.root, &moved_root).unwrap();
    fs::create_dir(&f.root).unwrap();
    assert!(
        f.journal
            .recover(&f.root)
            .unwrap_err()
            .cause()
            .contains("different mapping")
    );
    assert!(f.journal.has_pending().unwrap());
    assert!(!f.root.join("old.zip").exists());
    assert!(moved_root.join("old.zip.foldersync-partial").is_file());
    assert_eq!(
        fs::read(f.holding.join("proj/.git/objects/object")).unwrap(),
        b"original"
    );
}

#[test]
fn deleting_mapping_metadata_preserves_the_completed_holding_tree() {
    let f = Fixture::new();
    interrupted(&f, true, false);
    assert!(f.journal.forget_completed().is_err());
    f.journal.recover(&f.root).unwrap();
    f.journal.forget_completed().unwrap();
    assert!(f.journal.originals().unwrap().is_empty());
    assert_eq!(
        fs::read(f.holding.join("proj/.git/objects/object")).unwrap(),
        b"original"
    );
}
