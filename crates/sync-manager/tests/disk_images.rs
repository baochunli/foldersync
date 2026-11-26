#![cfg(target_os = "macos")]

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    io::{self, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use chrono::{TimeDelta, Utc};
use rclone_adapter::{
    EngineIdentity, ProcessError, ProcessRunner, ProcessSpec, RealProcessRunner, RunHandle,
    SpawnOptions, qualify_pinned_binary,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sync_core::{
    ACCESS_MARKER_FILENAME, Execution, LocalBinding, Readiness, RunOutcome, VolumeIdentity,
    VolumeKind,
};
use sync_manager::{
    AddMappingCommand, Clock, FinderMappingState, FinderStatusSnapshot, FixedClock,
    InitializeCommand, ManagerConfig, MountedVolume, OsVolumeProbe, RealBisyncLauncher,
    RecoveryCopySide, RunRecord, SyncManager, VolumeProbe, disk_space_for_path,
};

const DEADLINE: Duration = Duration::from_secs(120);
const DEREGISTRATION_POLL: Duration = Duration::from_millis(250);
const VOLUME_NAME: &str = "FolderSync Test Disk";
const MAPPING: &str = "disk-image";
const ACCOUNT: &str = "fixture";
const UNPLUG: &str = "simulated unplug (disk image forced detach)";

fn hdiutil(args: &[&str], path: Option<&Path>) -> io::Result<Output> {
    let mut command = Command::new("/usr/bin/hdiutil");
    command.args(args).env("LC_ALL", "C");
    if let Some(path) = path {
        command.arg(path);
    }
    command.output()
}

fn checked(output: Output) -> io::Result<String> {
    if output.status.success() {
        String::from_utf8(output.stdout).map_err(io::Error::other)
    } else {
        Err(io::Error::other(format!(
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn owned_info(info: &str, image: &Path) -> Option<String> {
    info.split("================================================")
        .find(|section| {
            section.lines().any(|line| {
                line.split_once(':').is_some_and(|(key, value)| {
                    key.trim() == "image-path" && Path::new(value.trim()) == image
                })
            })
        })
        .map(str::to_owned)
}

fn device_in(info: &str, device: &str) -> bool {
    info.lines()
        .any(|line| line.split_whitespace().next() == Some(device))
}

fn detach_and_wait(
    image: &Path,
    device: &str,
    force: bool,
    timeout: Duration,
    mut read_info: impl FnMut() -> io::Result<String>,
    mut detach: impl FnMut(&str, bool) -> io::Result<()>,
) -> io::Result<()> {
    if !owned_info(&read_info()?, image).is_some_and(|section| device_in(&section, device)) {
        return Err(io::Error::other(format!(
            "refused to detach {device}: hdiutil info did not confirm image ownership"
        )));
    }
    detach(device, force)?;
    let Some(section) = wait_for_deregistration(image, timeout, &mut read_info)? else {
        return Ok(());
    };

    // A device number can be reused while the original image's path is still
    // listed. Only use the node if the last poll still associates it with us.
    let retry_error = if device_in(&section, device) {
        eprintln!("Image still registered after {timeout:?}. Retrying normal detach of {device}.");
        let error = detach(device, false).err();
        if wait_for_deregistration(image, timeout, &mut read_info)?.is_none() {
            return Ok(());
        }
        error
    } else {
        None
    };
    let mut message = format!("image still registered: {}", image.display());
    if let Some(error) = retry_error {
        message.push_str(&format!(". Normal detach retry failed: {error}"));
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, message))
}

// Return the still-registered image section only if the deadline expires.
fn wait_for_deregistration(
    image: &Path,
    timeout: Duration,
    read_info: &mut impl FnMut() -> io::Result<String>,
) -> io::Result<Option<String>> {
    let deadline = Instant::now() + timeout;
    loop {
        let section = owned_info(&read_info()?, image);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if section.is_none() || remaining.is_zero() {
            return Ok(section);
        }
        thread::sleep(DEREGISTRATION_POLL.min(remaining));
    }
}

#[cfg(test)]
mod deregistration_tests {
    use super::*;

    const IMAGE: &str = "/tmp/owned/original.dmg";
    const DEVICE: &str = "/dev/disk12";
    const OWNED: &str = "image-path : /tmp/owned/original.dmg\n/dev/disk12";
    const PATH_ONLY: &str = "image-path : /tmp/owned/original.dmg\n/dev/disk120";
    const UNRELATED: &str = "image-path : /tmp/other/another.dmg\n/dev/disk12";

    #[test]
    fn waits_for_the_exact_image_to_disappear() {
        let mut info = [OWNED, OWNED, PATH_ONLY, UNRELATED].into_iter();
        let mut detaches = Vec::new();
        detach_and_wait(
            Path::new(IMAGE),
            DEVICE,
            true,
            Duration::from_secs(5),
            || Ok(info.next().expect("unexpected info query").to_owned()),
            |device, force| {
                detaches.push((device.to_owned(), force));
                Ok(())
            },
        )
        .unwrap();
        assert!(info.next().is_none());
        assert_eq!(detaches, [(DEVICE.to_owned(), true)]);
    }

    #[test]
    fn retries_normal_detach_once_after_timeout() {
        let mut info = [OWNED, OWNED, OWNED].into_iter();
        let mut detaches = Vec::new();
        let error = detach_and_wait(
            Path::new(IMAGE),
            DEVICE,
            true,
            Duration::ZERO,
            || Ok(info.next().expect("unexpected info query").to_owned()),
            |device, force| {
                detaches.push((device.to_owned(), force));
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(info.next().is_none());
        assert_eq!(
            detaches,
            [(DEVICE.to_owned(), true), (DEVICE.to_owned(), false)]
        );
    }

    #[test]
    fn refuses_retry_after_the_recorded_node_moves_to_another_image() {
        let stale =
            format!("{PATH_ONLY}\n================================================\n{UNRELATED}");
        let mut info = [OWNED, stale.as_str()].into_iter();
        let mut detaches = Vec::new();
        let error = detach_and_wait(
            Path::new(IMAGE),
            DEVICE,
            false,
            Duration::ZERO,
            || Ok(info.next().expect("unexpected info query").to_owned()),
            |device, force| {
                detaches.push((device.to_owned(), force));
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(detaches, [(DEVICE.to_owned(), false)]);
    }
}

struct Image {
    path: PathBuf,
    device: Option<String>,
    mount: Option<PathBuf>,
    detach_requested: bool,
}

struct Images {
    root: PathBuf,
    images: Vec<Image>,
    probe: Arc<ImageProbe>,
}

// Some macOS releases omit DeviceInternal on synthesized APFS image volumes.
// Bridge only that missing field, for a tracked image still backing the path.
const MISSING_DEVICE_INTERNAL: &str = "Disk connection type is unavailable";

#[derive(Default)]
struct ImageProbe(Mutex<BTreeMap<PathBuf, (u64, MountedVolume)>>);

impl VolumeProbe for ImageProbe {
    fn probe(&self, path: &Path) -> io::Result<MountedVolume> {
        OsVolumeProbe.probe(path).or_else(|error| {
            if error.to_string() == MISSING_DEVICE_INTERNAL {
                let device = fs::metadata(path)?.dev();
                if let Some((_, volume)) = self
                    .0
                    .lock()
                    .unwrap()
                    .values()
                    .find(|(id, volume)| *id == device && path.starts_with(&volume.mount_path))
                {
                    return Ok(volume.clone());
                }
            }
            Err(error)
        })
    }

    fn find(&self, binding: &LocalBinding) -> io::Result<Option<MountedVolume>> {
        OsVolumeProbe.find(binding)
    }
}

impl Images {
    fn new() -> Self {
        let path = checked(Command::new("/usr/bin/mktemp").arg("-d").output().unwrap()).unwrap();
        Self {
            root: fs::canonicalize(path.trim()).unwrap(),
            images: Vec::new(),
            probe: Arc::default(),
        }
    }

    fn create(&mut self, name: &str) -> usize {
        let path = self.root.join(format!("{name}.dmg"));
        self.images.push(Image {
            path,
            device: None,
            mount: None,
            detach_requested: false,
        });
        let index = self.images.len() - 1;
        checked(
            hdiutil(
                &[
                    "create",
                    "-fs",
                    "APFS",
                    "-size",
                    "512m",
                    "-volname",
                    VOLUME_NAME,
                ],
                Some(&self.images[index].path),
            )
            .unwrap(),
        )
        .expect("create disposable APFS image");
        index
    }

    fn attach(&mut self, index: usize, mount: &Path) -> io::Result<String> {
        let image = &mut self.images[index];
        assert!(image.device.is_none() && mount.starts_with(&self.root) && mount != self.root);
        match fs::create_dir(mount) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                assert!(
                    mount.symlink_metadata()?.is_dir()
                        && fs::metadata(mount)?.dev() == fs::metadata(&self.root)?.dev()
                        && fs::read_dir(mount)?.next().is_none()
                );
            }
            Err(err) => return Err(err),
        }
        image.mount = Some(mount.to_owned());
        image.detach_requested = false;
        let output = Command::new("/usr/bin/hdiutil")
            .args(["attach", "-nobrowse", "-mountpoint"])
            .arg(mount)
            .arg(&image.path)
            .env("LC_ALL", "C")
            .output()?;
        // Keep the physical image node from attach output before any fallible checks.
        let stdout = String::from_utf8_lossy(&output.stdout);
        image.device = stdout
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .find(|field| {
                field.strip_prefix("/dev/disk").is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
                })
            })
            .map(str::to_owned);
        let stdout = checked(output)?;
        if image.device.is_none() {
            return Err(io::Error::other(
                "attach did not report a whole image device",
            ));
        }
        stdout
            .lines()
            .find_map(|line| {
                let mut fields = line.split('\t');
                let device = fields.next()?.trim();
                let mounted = fields.nth(1)?.trim();
                (Path::new(mounted) == mount).then(|| device.to_owned())
            })
            .ok_or_else(|| io::Error::other("attach did not report the requested mountpoint"))
    }

    fn detach(&mut self, index: usize, force: bool) -> io::Result<()> {
        let image = &mut self.images[index];
        let device = image.device.as_deref().expect("recorded attach device");
        detach_and_wait(
            &image.path,
            device,
            force,
            DEADLINE,
            || checked(hdiutil(&["info"], None)?),
            |device, force| {
                let mut args = vec!["detach"];
                if force {
                    args.push("-force");
                }
                args.push(device);
                let result = checked(hdiutil(&args, None)?)?;
                image.detach_requested = true;
                eprintln!(
                    "{}: {}",
                    if force { UNPLUG } else { "controlled detach" },
                    result.trim()
                );
                Ok(())
            },
        )?;
        image.device = None;
        if let Some(mount) = &image.mount {
            self.probe.0.lock().unwrap().remove(mount);
        }
        Ok(())
    }

    fn cleanup(&mut self) -> io::Result<()> {
        let mut detach_error = None;
        for index in (0..self.images.len()).rev() {
            if self.images[index].device.is_some()
                && !self.images[index].detach_requested
                && let Err(error) = self.detach(index, false)
            {
                detach_error = Some(error);
            }
        }
        let info = checked(hdiutil(&["info"], None)?)?;
        let root_device = fs::metadata(&self.root)?.dev();
        for image in &self.images {
            if owned_info(&info, &image.path).is_some() {
                return Err(io::Error::other(format!(
                    "image still registered: {}",
                    image.path.display()
                )));
            }
            if let Some(mount) = &image.mount {
                match fs::metadata(mount) {
                    Ok(meta) if meta.dev() == root_device => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    _ => {
                        return Err(io::Error::other(format!(
                            "mountpoint still active or inaccessible: {}",
                            mount.display()
                        )));
                    }
                }
            }
            eprintln!("hdiutil info: no entry for {}", image.path.display());
        }
        if let Some(error) = detach_error {
            return Err(error);
        }
        fs::remove_dir_all(&self.root)
    }

    fn classify(&self, index: usize, mount: &Path, volume_device: &str) -> MountedVolume {
        let info = checked(hdiutil(&["info"], None).unwrap()).unwrap();
        assert!(
            owned_info(&info, &self.images[index].path)
                .is_some_and(|section| device_in(&section, volume_device))
        );
        let details = checked(
            Command::new("/usr/sbin/diskutil")
                .args(["info", volume_device])
                .env("LC_ALL", "C")
                .output()
                .unwrap(),
        )
        .unwrap();
        let field = |key: &str| {
            details
                .lines()
                .find_map(|line| {
                    let (label, value) = line.trim().split_once(':')?;
                    (label == key).then_some(value.trim())
                })
                .expect("owned image diskutil field")
        };
        let name = field("Volume Name");
        let uuid = uuid::Uuid::parse_str(field("Volume UUID"))
            .unwrap()
            .to_string();
        let filesystem = field("File System Personality");
        assert_eq!(name, VOLUME_NAME);
        assert_eq!(filesystem, "APFS");
        eprintln!(
            "diskutil: APFS name={name:?} uuid={uuid} protocol={:?} location={:?} removable={:?}",
            field("Protocol"),
            field("Device Location"),
            field("Removable Media"),
        );
        match OsVolumeProbe.probe(mount) {
            Ok(volume) => {
                assert_eq!(volume.mount_path, mount);
                assert_eq!(volume.filesystem, filesystem);
                assert_eq!(volume.display_name, name);
                assert_eq!(volume.identity.stable_id, uuid);
                eprintln!(
                    "DiskArbitration: VolumeName={name:?} VolumeUUID={uuid} DeviceInternal={} VolumeKind={:?}",
                    volume.identity.kind == VolumeKind::Internal,
                    volume.identity.kind
                );
                if volume.identity.kind != VolumeKind::External {
                    eprintln!(
                        "NOT VERIFIED for real disks: the image did not classify as External"
                    );
                }
                volume
            }
            Err(error) if error.to_string() == MISSING_DEVICE_INTERNAL => {
                eprintln!(
                    "DiskArbitration: VolumeUUID valid, DeviceInternal missing; VolumeKind unavailable ({error}). \
                     NOT VERIFIED for real disks. Using test-only classification from the owned image."
                );
                assert_eq!(field("Protocol"), "Disk Image");
                let kind = match field("Device Location") {
                    "External" => VolumeKind::External,
                    "Internal" => VolumeKind::Internal,
                    _ => panic!("cannot classify the owned disk image"),
                };
                let volume = MountedVolume {
                    identity: VolumeIdentity::new(kind, uuid),
                    mount_path: mount.to_owned(),
                    display_name: name.to_owned(),
                    filesystem: filesystem.to_owned(),
                    space: disk_space_for_path(mount).unwrap(),
                };
                self.probe.0.lock().unwrap().insert(
                    mount.to_owned(),
                    (fs::metadata(mount).unwrap().dev(), volume.clone()),
                );
                volume
            }
            Err(error) => panic!("DiskArbitration volume probe: {error}"),
        }
    }

    fn attach_and_classify(&mut self, index: usize, mount: &Path) -> MountedVolume {
        let device = self
            .attach(index, mount)
            .expect("attach disposable APFS image");
        self.classify(index, mount, &device)
    }
}

impl Drop for Images {
    fn drop(&mut self) {
        if self.root.exists()
            && let Err(error) = self.cleanup()
        {
            eprintln!(
                "Image cleanup failed. Preserved {}: {error}",
                self.root.display()
            );
            if !thread::panicking() {
                panic!("could not verify safe disk-image cleanup");
            }
        }
    }
}

#[derive(Default, Serialize)]
struct Truth {
    files: BTreeMap<String, String>,
    local_recovery: BTreeMap<String, String>,
    remote_recovery: BTreeMap<String, String>,
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hashes(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, result: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            let path = entry.path();
            if kind.is_dir() {
                walk(root, &path, result);
            } else {
                assert!(kind.is_file(), "unexpected filesystem entry: {path:?}");
                let rel = path.strip_prefix(root).unwrap().to_str().unwrap();
                result.insert(rel.to_owned(), sha256(&fs::read(&path).unwrap()));
            }
        }
    }

    let mut result = BTreeMap::new();
    walk(root, root, &mut result);
    result
}

// Recovery roots contain a separate directory for each run. Compare every
// preserved file, including copies made by the interrupted run and its retry.
fn recovery_hashes(root: &Path) -> BTreeMap<String, String> {
    let mut copies = BTreeMap::new();
    for (path, hash) in hashes(root) {
        let (_, rest) = path.split_once('/').expect("scoped recovery path");
        let (run, relative) = rest.split_once('/').expect("run-scoped recovery path");
        assert!(run.starts_with("run-"));
        assert!(
            copies.insert(relative.to_owned(), hash).is_none(),
            "duplicate copy of {relative}"
        );
    }
    copies
}

struct Fixture {
    scratch: PathBuf,
    local: PathBuf,
    remote: PathBuf,
    local_recovery: PathBuf,
    remote_recovery: PathBuf,
    truth: Truth,
}

impl Fixture {
    fn new(scratch: &Path, mount: &Path, remote_recovery_mount: &Path) -> Self {
        let fixture = Self {
            scratch: scratch.to_owned(),
            local: mount.join("data"),
            remote: scratch.join("remote"),
            local_recovery: mount.join("recovery"),
            remote_recovery: remote_recovery_mount.join("remote-recovery"),
            truth: Truth::default(),
        };
        for path in [
            &fixture.local,
            &fixture.remote,
            &fixture.local_recovery,
            &fixture.remote_recovery,
        ] {
            fs::create_dir(path).unwrap();
        }
        fixture
    }

    fn seed(&mut self, local: bool, name: &str, bytes: &[u8]) {
        let directory = if local { &self.local } else { &self.remote };
        fs::write(directory.join(name), bytes).unwrap();
        self.truth.files.insert(name.to_owned(), sha256(bytes));
    }

    fn setup(&mut self, config: ManagerConfig, probe: Arc<ImageProbe>) -> SyncManager {
        let manager = SyncManager::open_with_volume_probe(config, probe.clone()).unwrap();
        manager
            .register_local_fixture_account(ACCOUNT, "Local fixture")
            .unwrap();
        // With the bridge, construct the local fixture command on the host
        // filesystem and then bind the actual image below.
        let mut command = AddMappingCommand::local_fixture(
            MAPPING,
            ACCOUNT,
            "APFS qualification",
            if OsVolumeProbe.probe(&self.local).is_ok() {
                &self.local
            } else {
                &self.scratch
            },
            &self.remote,
            &self.local_recovery,
            &self.remote_recovery,
        )
        .unwrap();
        let volume = probe.probe(&self.local).unwrap();
        command.local_binding = LocalBinding {
            relative_dir: self
                .local
                .strip_prefix(&volume.mount_path)
                .unwrap()
                .to_owned(),
            mount_path: volume.mount_path,
            volume: volume.identity,
        };
        manager.add_mapping(command).unwrap();
        manager
            .initialize(InitializeCommand {
                mapping_id: MAPPING.into(),
                expected_direction: None,
            })
            .unwrap();
        assert_success(&manager);
        let marker = fs::read(self.local.join(ACCESS_MARKER_FILENAME)).unwrap();
        self.truth
            .files
            .insert(ACCESS_MARKER_FILENAME.to_owned(), sha256(&marker));
        self.verify("initialized");
        // Establish a normal-run baseline with both saved recovery listings.
        run(&manager);
        manager
    }

    fn verify(&self, checkpoint: &str) {
        let truth_path = self.scratch.join("truth.json");
        fs::write(&truth_path, serde_json::to_vec(&self.truth).unwrap()).unwrap();
        eprintln!(
            "{checkpoint} SHA-256 truth: {}",
            fs::read_to_string(truth_path).unwrap()
        );
        assert_eq!(hashes(&self.local), self.truth.files, "{checkpoint}: image");
        assert_eq!(
            hashes(&self.remote),
            self.truth.files,
            "{checkpoint}: fake remote"
        );
        assert_eq!(
            recovery_hashes(&self.local_recovery),
            self.truth.local_recovery,
            "{checkpoint}: image recovery"
        );
        assert_eq!(
            recovery_hashes(&self.remote_recovery),
            self.truth.remote_recovery,
            "{checkpoint}: fake-remote recovery"
        );
    }

    fn follow_up(&mut self, manager: &SyncManager) {
        self.seed(true, "cycle-one.txt", b"created on the image in cycle one");
        run(manager);
        self.verify("follow-up 1");
        self.seed(
            false,
            "cycle-two.txt",
            b"created on the fake remote in cycle two",
        );
        run(manager);
        self.verify("follow-up 2");
    }
}

fn pinned_engine() -> Option<EngineIdentity> {
    match qualify_pinned_binary().expect("qualify the verified pinned rclone") {
        Some(engine) => {
            eprintln!(
                "rclone {} {} sha256={}",
                engine.version(),
                engine.platform(),
                engine.sha256()
            );
            Some(engine)
        }
        None => {
            eprintln!("SKIP: pinned rclone is absent; run scripts/fetch-rclone.sh");
            None
        }
    }
}

fn first_attach(images: &mut Images, index: usize, mount: &Path) -> Option<MountedVolume> {
    let device = match images.attach(index, mount) {
        Ok(device) => device,
        Err(err)
            if std::env::var_os("FOLDERSYNC_DISK_IMAGES_CI").as_deref() == Some("1".as_ref())
                && [
                    "Operation not permitted",
                    "Operation not supported",
                    "Permission denied",
                    "Device not configured",
                    "Device not supported",
                    "Function not implemented",
                    "No such device",
                ]
                .iter()
                .any(|message| err.to_string().contains(message)) =>
        {
            eprintln!("SKIP: hdiutil attach unavailable on this CI runner: {err}");
            return None;
        }
        Err(err) => panic!("hdiutil attach failed: {err}"),
    };
    Some(images.classify(index, mount, &device))
}

fn assert_success(manager: &SyncManager) {
    assert_success_with_review(manager, &[]);
}

fn assert_success_with_review(manager: &SyncManager, batch_paths: &[String]) -> bool {
    let snapshot = manager.wait_until_idle(DEADLINE).unwrap();
    let mapping = &snapshot.mappings[0];
    let needs_review = mapping.last_run_outcome == Some(RunOutcome::SuccessWithConflicts)
        && !batch_paths.is_empty();
    if needs_review {
        assert!(!mapping.unreviewed_conflict_paths.is_empty());
        for path in &mapping.unreviewed_conflict_paths {
            assert!(batch_paths.contains(path), "unexpected conflict: {path}");
        }
        eprintln!(
            "reviewed recovery reported potential conflicts on {:?}",
            mapping.unreviewed_conflict_paths
        );
    } else {
        if mapping.last_run_outcome != Some(RunOutcome::Success)
            && let Some(id) = &mapping.last_run_id
        {
            for line in manager.read_run_log(id, 50).unwrap() {
                eprintln!("{line}");
            }
        }
        assert_eq!(
            mapping.last_run_outcome,
            Some(RunOutcome::Success),
            "{mapping:#?}"
        );
    }
    assert_eq!(mapping.readiness, Readiness::Ready, "{mapping:#?}");
    assert_eq!(mapping.execution, Execution::Idle, "{mapping:#?}");
    needs_review
}

fn run(manager: &SyncManager) {
    manager.run_now(MAPPING).unwrap();
    assert_success(manager);
}

fn wait_for(description: &str, condition: impl Fn() -> bool) {
    let started = Instant::now();
    while !condition() {
        assert!(
            started.elapsed() < DEADLINE,
            "timed out waiting for {description}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn presence_case(wrong_disk: bool) {
    let Some(_engine) = pinned_engine() else {
        return;
    };
    let mut images = Images::new();
    let mount = images.root.join(VOLUME_NAME);
    let original = images.create("original");
    let Some(bound) = first_attach(&mut images, original, &mount) else {
        return;
    };
    let replacement = wrong_disk.then(|| images.create("replacement"));
    let mut fixture = Fixture::new(&images.root, &mount, &images.root);
    fixture.seed(true, "baseline.txt", b"original volume only");
    let clock = Arc::new(FixedClock::new(Utc::now()));
    let mut config = ManagerConfig::with_state_dir(images.root.join("state"));
    config.clock = clock.clone();
    let manager = fixture.setup(config, images.probe.clone());
    let id = MAPPING.into();
    let before = manager.list_runs(&id).unwrap().len();

    images.detach(original, false).unwrap();
    let wrong_root = mount.join("decoy");
    let mut wrong_hashes = BTreeMap::new();
    if let Some(replacement) = replacement {
        let wrong = images.attach_and_classify(replacement, &mount);
        assert_ne!(wrong.identity.stable_id, bound.identity.stable_id);
        // Give the wrong disk both required directories and a matching access marker.
        fs::create_dir(&fixture.local).unwrap();
        fs::create_dir(&fixture.local_recovery).unwrap();
        fs::create_dir(&wrong_root).unwrap();
        fs::write(wrong_root.join("sentinel.txt"), b"must remain untouched").unwrap();
        fs::copy(
            fixture.remote.join(ACCESS_MARKER_FILENAME),
            fixture.local.join(ACCESS_MARKER_FILENAME),
        )
        .unwrap();
        wrong_hashes = hashes(&mount);
    }
    let expected = if wrong_disk {
        format!("A different disk named {VOLUME_NAME} is connected")
    } else {
        format!("Waiting for {VOLUME_NAME}")
    };
    wait_for(&expected, || {
        manager.snapshot().mappings[0].status_text == expected
    });
    let snapshot = manager.snapshot();
    assert_eq!(snapshot.mappings[0].readiness, Readiness::VolumeUnavailable);
    assert_eq!(snapshot.mappings[0].execution, Execution::Idle);
    assert!(snapshot.mappings[0].scheduling_enabled && !snapshot.scheduling_paused);
    eprintln!("held mapping: {expected}");
    let finder: FinderStatusSnapshot =
        serde_json::from_slice(&fs::read(manager.finder_status_dir().join("status.json")).unwrap())
            .unwrap();
    assert_eq!(finder.mappings[0].state, FinderMappingState::DiskMissing);
    eprintln!("Finder status: disk_missing");

    fixture.seed(
        false,
        "while-held.txt",
        b"remote change while the original disk is absent",
    );
    let error = manager.run_now(MAPPING).unwrap_err();
    assert!(error.cause().contains(&expected), "{error}");
    // Put interval scheduling past due while the mapping is held, then give
    // the real one-second scheduler more than one tick to attempt a launch.
    clock.set(clock.now_utc() + TimeDelta::hours(25));
    thread::sleep(Duration::from_secs(3));
    assert_eq!(manager.list_runs(&id).unwrap().len(), before);
    assert_eq!(hashes(&fixture.remote), fixture.truth.files);
    if let Some(replacement) = replacement {
        assert_eq!(
            hashes(&mount),
            wrong_hashes,
            "replacement disk was modified"
        );
        images.detach(replacement, false).unwrap();
    } else {
        assert!(!fixture.local.exists(), "missing local root was created");
    }
    let reattached = images.attach_and_classify(original, &mount);
    assert_eq!(reattached.identity, bound.identity);
    wait_for("scheduled sync after the original UUID returns", || {
        manager.list_runs(&id).unwrap().len() > before
    });
    assert_success(&manager);
    fixture.verify("scheduled sync after reconnect");
    fixture.follow_up(&manager);
    manager.shutdown().unwrap();
}

#[test]
#[ignore = "requires attaching disposable APFS disk images"]
fn wrong_disk_with_same_volume_name() {
    presence_case(true);
}

#[test]
#[ignore = "requires attaching disposable APFS disk images"]
fn missing_disk() {
    presence_case(false);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnplugPhase {
    Upload,
    Download,
    Delete,
}

const BATCH: usize = 4;

#[derive(Default)]
struct ThrottledRunner {
    slow_next_run: AtomicBool,
}

impl ProcessRunner for ThrottledRunner {
    fn spawn(&self, spec: &ProcessSpec, options: SpawnOptions) -> Result<RunHandle, ProcessError> {
        if !self.slow_next_run.swap(false, Ordering::SeqCst) {
            return RealProcessRunner.spawn(spec, options);
        }
        // Stretch only the fault run. Keep the production process wrapper and
        // pinned digest check, including its normal graceful-stop behavior.
        let mut spec = spec.clone();
        spec.args.extend(
            [
                "--disable",
                "Copy",
                "--bwlimit",
                "768k",
                "--transfers",
                "1",
                "--checkers",
                "1",
                "--retries-sleep",
                "6s",
            ]
            .map(Into::into),
        );
        RealProcessRunner.spawn(&spec, options)
    }
}

fn unplug_case(phase: UnplugPhase) {
    let Some(engine) = pinned_engine() else {
        return;
    };
    eprintln!("{UNPLUG}: {phase:?}");
    let mut images = Images::new();
    let mount = images.root.join(VOLUME_NAME);
    let original = images.create("original");
    let Some(bound) = first_attach(&mut images, original, &mount) else {
        return;
    };
    let mut remote_recovery_mount = images.root.clone();
    if phase == UnplugPhase::Delete {
        // Backups on a different filesystem require a real copy before rclone
        // removes each original. The throttle makes partial deletion observable.
        let recovery_image = images.create("remote-recovery");
        remote_recovery_mount = images.root.join("recovery-mount");
        images.attach_and_classify(recovery_image, &remote_recovery_mount);
        assert_ne!(
            fs::metadata(&remote_recovery_mount).unwrap().dev(),
            fs::metadata(&images.root).unwrap().dev()
        );
    }
    let mut fixture = Fixture::new(&images.root, &mount, &remote_recovery_mount);
    let names: Vec<_> = (0..BATCH).map(|i| format!("batch-{i}.bin")).collect();
    // Stay within the ordinary 10% deletion limit, including if rclone backed
    // up a target before the interruption and its replacement is still absent.
    for i in 0..40 {
        fixture.seed(
            true,
            &format!("retained-{i}.txt"),
            format!("retained-{i}").as_bytes(),
        );
    }
    for (i, name) in names.iter().enumerate() {
        let size = if phase == UnplugPhase::Delete {
            8 * 1024 * 1024
        } else {
            128
        };
        fixture.seed(true, name, &vec![i as u8; size]);
    }
    let runner = Arc::new(ThrottledRunner::default());
    let mut config = ManagerConfig::with_state_dir(images.root.join("state"));
    config.clock = Arc::new(FixedClock::new(Utc::now()));
    config.launcher = Arc::new(RealBisyncLauncher::with_process_runner(
        Some(engine.path().to_owned()),
        runner.clone(),
    ));
    let manager = fixture.setup(config, images.probe.clone());
    let mapping_id = MAPPING.into();
    let before = manager.snapshot().mappings[0].last_run_id.clone();
    let generation = manager.snapshot().mappings[0]
        .active_generation_id
        .clone()
        .unwrap();
    let workdir = manager
        .get_generation(&generation)
        .unwrap()
        .unwrap()
        .workdir_path;

    for (i, name) in names.iter().enumerate() {
        let original = fixture.truth.files[name].clone();
        if phase == UnplugPhase::Download {
            fixture.truth.local_recovery.insert(name.clone(), original);
        } else {
            fixture.truth.remote_recovery.insert(name.clone(), original);
        }
        if phase == UnplugPhase::Delete {
            fs::remove_file(fixture.local.join(name)).unwrap();
            fixture.truth.files.remove(name);
        } else {
            fixture.seed(
                phase == UnplugPhase::Upload,
                name,
                &vec![0x80 + i as u8; 8 * 1024 * 1024],
            );
        }
    }

    runner.slow_next_run.store(true, Ordering::SeqCst);
    manager.run_now(MAPPING).unwrap();
    let wait = Instant::now();
    let active = loop {
        assert!(
            wait.elapsed() < DEADLINE,
            "timed out waiting for partial {phase:?}"
        );
        if let Some(id) = &manager.snapshot().mappings[0].last_run_id
            && Some(id) != before.as_ref()
        {
            panic!(
                "{phase:?} completed before injection:\n{}",
                manager.read_run_log(id, 100).unwrap().join("\n")
            );
        }
        if let Some(run) = manager
            .list_runs(&mapping_id)
            .unwrap()
            .into_iter()
            .find(|run| run.status == "running" && run.child_pid.is_some())
        {
            let completed = phase_progress(&manager, &run, phase, &names);
            if (1..BATCH).contains(&completed) {
                eprintln!(
                    "{UNPLUG}: {phase:?} run={} pid={:?} progress={completed}/{BATCH} at {}",
                    run.id,
                    run.child_pid,
                    Utc::now().to_rfc3339(),
                );
                break run;
            }
        }
        thread::sleep(Duration::from_millis(50));
    };
    images.detach(original, true).unwrap();

    let snapshot = manager.wait_until_idle(DEADLINE).unwrap();
    let finished = manager.get_run(&active.id).unwrap().unwrap();
    let log = fs::read_to_string(&finished.log_path).unwrap();
    eprintln!(
        "{UNPLUG}: {phase:?} run={} status={} exit={:?}/{:?}\n{log}",
        finished.id, finished.status, finished.exit_code, finished.exit_signal
    );
    let state = &snapshot.mappings[0];
    assert_eq!(state.last_run_id.as_ref(), Some(&active.id));
    assert_eq!(state.last_run_outcome, Some(RunOutcome::Interrupted));
    assert_eq!(state.execution, Execution::Interrupted);
    assert_eq!(state.readiness, Readiness::RecoveryRequired);
    assert_eq!(finished.workdir_path, workdir);
    assert!(finished.workdir_path.is_dir() && finished.run_dir.join("outcome.json").is_file());
    assert!(finished.finished_at.is_some());
    assert!(
        log.contains("Signal received: interrupt")
            && log.contains("Attempting to gracefully shutdown."),
        "{phase:?}: pinned rclone did not receive the manager's graceful stop"
    );
    assert_eq!(
        finished.exit_signal, None,
        "rclone was terminated by a signal"
    );

    if phase == UnplugPhase::Download {
        preserve_unmounted_writes(&images.root, &mount, &fixture.remote, &names);
    }
    let reattached = images.attach_and_classify(original, &mount);
    assert_eq!(reattached.identity, bound.identity);
    let report = manager.inspect_recovery(MAPPING).unwrap();
    assert!(
        report.identities_verified,
        "{}",
        report.identity_check_detail
    );
    assert!(!report.workdir_files.is_empty());
    assert_eq!(report.open_cases.len(), 1);
    let case = &report.open_cases[0].case;
    assert_eq!(case.reason_kind, "volume_disconnected");
    assert_eq!(case.run_id.as_ref(), Some(&active.id));
    assert_eq!(case.workdir_path.as_ref(), Some(&workdir));
    let count = manager.list_runs(&mapping_id).unwrap().len();
    assert!(
        manager.run_now(MAPPING).is_err(),
        "reconnect cleared the recovery hold"
    );
    assert_eq!(manager.list_runs(&mapping_id).unwrap().len(), count);
    eprintln!(
        "{UNPLUG}: persisted case={} workdir entries={:?}",
        case.id, report.workdir_files
    );

    let inspection_dir = images.root.join("inspected");
    fs::create_dir(&inspection_dir).unwrap();
    for (i, copy) in manager
        .list_recovery_copies(&active.id)
        .unwrap()
        .iter()
        .enumerate()
    {
        let truth = match copy.side {
            RecoveryCopySide::Local => &fixture.truth.local_recovery,
            RecoveryCopySide::Remote => &fixture.truth.remote_recovery,
        };
        let directory = inspection_dir.join(i.to_string());
        fs::create_dir(&directory).unwrap();
        let destination = manager
            .copy_recovery_file_to_inspection(
                &active.id,
                copy.side,
                &copy.relative_path,
                &directory,
            )
            .unwrap();
        assert_eq!(
            sha256(&fs::read(destination).unwrap()),
            truth[&copy.relative_path]
        );
    }
    if phase != UnplugPhase::Delete {
        let (source, target) = if phase == UnplugPhase::Upload {
            (&fixture.local, &fixture.remote)
        } else {
            (&fixture.remote, &fixture.local)
        };
        preserve_partials(source, target, &inspection_dir, &names);
    }
    manager.complete_recovery(MAPPING, true).unwrap();
    let needs_review = assert_success_with_review(&manager, &names);
    fixture.verify("reviewed recovery");
    if needs_review {
        manager.acknowledge_conflicts(MAPPING).unwrap();
    }
    fixture.follow_up(&manager);
    manager.shutdown().unwrap();
}

// A detached destination can strand a temporary copy. Model an explicit
// recovery review: retain only verified fragments of the seeded source files
// outside both endpoints so the next run cannot propagate them as user data.
fn preserve_partials(source: &Path, target: &Path, inspection: &Path, names: &[String]) {
    for entry in fs::read_dir(target).unwrap() {
        let entry = entry.unwrap();
        let filename = entry.file_name();
        let Some(name) = partial_source(&filename, names) else {
            continue;
        };
        assert!(entry.file_type().unwrap().is_file());
        let path = entry.path();
        let fragment = fs::read(&path).unwrap();
        assert!(fs::read(source.join(name)).unwrap().starts_with(&fragment));
        let preserved = inspection.join(&filename);
        let mut file = fs::File::create_new(&preserved).unwrap();
        file.write_all(&fragment).unwrap();
        file.sync_all().unwrap();
        assert_eq!(fs::read(preserved).unwrap(), fragment);
        fs::remove_file(path).unwrap();
        eprintln!(
            "{UNPLUG}: reviewed and preserved {filename:?} outside the endpoints ({} bytes, SHA-256 {})",
            fragment.len(),
            sha256(&fragment)
        );
    }
}

fn partial_source<'a>(filename: &'a OsStr, names: &[String]) -> Option<&'a str> {
    let (name, tag) = filename
        .to_str()?
        .strip_suffix(".partial")?
        .rsplit_once('.')?;
    (tag.len() == 8
        && tag.bytes().all(|byte| byte.is_ascii_hexdigit())
        && names.iter().any(|expected| expected == name))
    .then_some(name)
}

// rclone can recreate a writable mount path during graceful shutdown.
// Verify and archive any such files before the next attach can cover them.
fn preserve_unmounted_writes(scratch: &Path, mount: &Path, source: &Path, names: &[String]) {
    let metadata = match fs::symlink_metadata(mount) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => panic!("inspect unmounted fixture mountpoint: {error}"),
    };
    assert!(metadata.is_dir());
    assert_eq!(metadata.dev(), fs::metadata(scratch).unwrap().dev());
    if fs::read_dir(mount).unwrap().next().is_none() {
        return;
    }
    let observed = hashes(mount);
    for (relative, digest) in &observed {
        let filename = relative.strip_prefix("data/").unwrap();
        if names.iter().any(|name| name == filename) {
            assert_eq!(*digest, sha256(&fs::read(source.join(filename)).unwrap()));
        } else {
            let name = partial_source(OsStr::new(filename), names)
                .expect("only the owned batch can write at the unmounted path");
            assert!(
                fs::read(source.join(name))
                    .unwrap()
                    .starts_with(&fs::read(mount.join(relative)).unwrap())
            );
        }
    }
    let preserved = scratch.join("unmounted-writes");
    fs::rename(mount, &preserved).unwrap();
    assert_eq!(hashes(&preserved), observed);
    eprintln!(
        "{UNPLUG}: Download recreated the writable mount path; preserved host-side writes with SHA-256 {observed:?}"
    );
}

fn phase_progress(
    manager: &SyncManager,
    run: &RunRecord,
    phase: UnplugPhase,
    names: &[String],
) -> usize {
    manager
        .read_run_log(&run.id, 256)
        .unwrap()
        .iter()
        .filter(|line| {
            let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
                return false;
            };
            let message = record["msg"].as_str().unwrap_or_default();
            let object = record["object"].as_str().unwrap_or_default();
            names.iter().any(|name| name == object)
                && match phase {
                    UnplugPhase::Upload | UnplugPhase::Download => message.starts_with("Copied ("),
                    UnplugPhase::Delete => message == "Moved into backup dir",
                }
        })
        .count()
}

#[test]
#[ignore = "requires attaching disposable APFS disk images"]
fn simulated_unplug_during_upload() {
    unplug_case(UnplugPhase::Upload);
}

#[test]
#[ignore = "requires attaching disposable APFS disk images"]
fn simulated_unplug_during_download() {
    unplug_case(UnplugPhase::Download);
}

#[test]
#[ignore = "requires attaching disposable APFS disk images"]
fn simulated_unplug_during_deletion() {
    unplug_case(UnplugPhase::Delete);
}
