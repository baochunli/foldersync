//! Runs the Rust application directly on the machine's native backend.

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
use nsh_platform_linux as platform;
#[cfg(target_os = "macos")]
use nsh_platform_macos as platform;
#[cfg(target_os = "windows")]
use nsh_platform_windows as platform;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn main() -> impl std::process::Termination {
    nsh_app_sync_ui::handle_credential_helper_invocation();
    env_logger::init();
    platform::run_app(
        platform::AppSource::Rust(platform::Root::of::<nsh_app_sync_ui::App>()),
        platform::AppOptions {
            title: "FolderSync".to_owned(),
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            app_id: "com.example.syncui".to_owned(),
            size: (1080, 740),
            #[cfg(target_os = "macos")]
            activation_policy: platform::ActivationPolicy::Accessory,
            #[cfg(target_os = "macos")]
            keep_alive_on_close: true,
            ..Default::default()
        },
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!(
    "Run this binary on macOS, Windows or Linux; use the TSX entry with nsh run for iOS and Android."
);
