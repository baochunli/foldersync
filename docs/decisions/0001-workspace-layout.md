# 0001. One Cargo workspace

The pinned NSH 0.6.1 CLI generated a standalone Rust app. The app built
standalone, then compiled as a root Cargo workspace member after its nested
`[workspace]` was removed. Its generated `nsh.json` still points at the Rust
crate in the same directory. NSH keeps generated runners and declarations
under the app's ignored `.nsh/` directory.

The root `Cargo.lock` pins the app and library dependencies. The CLI and
three library crates have independent build targets and no NSH dependencies,
so the Linux job can lint and test them without a desktop runtime.
