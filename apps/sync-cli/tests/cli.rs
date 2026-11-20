use std::process::Command;

#[test]
fn prints_version() {
    for args in [&[][..], &["version"][..], &["--version"][..]] {
        let output = Command::new(env!("CARGO_BIN_EXE_sync-cli"))
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            concat!("FolderSync ", env!("CARGO_PKG_VERSION"), "\n")
        );
    }
}

#[test]
fn prints_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_sync-cli"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("Usage: sync-cli"));
    assert!(help.contains("version"));
}

#[test]
fn rejects_unknown_arguments() {
    let output = Command::new(env!("CARGO_BIN_EXE_sync-cli"))
        .arg("unknown")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr).unwrap().contains("--help"));
}
