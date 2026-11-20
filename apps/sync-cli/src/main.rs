use std::{env, process::ExitCode};

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    match (args.next(), args.next()) {
        (None, None) => print_version(),
        (Some(arg), None) if arg == "version" || arg == "--version" || arg == "-V" => {
            print_version();
        }
        (Some(arg), None) if arg == "--help" || arg == "-h" => {
            println!("Usage: sync-cli [version|--help|--version]");
        }
        _ => {
            eprintln!("Invalid arguments. Run sync-cli --help.");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}

fn print_version() {
    println!("FolderSync {}", env!("CARGO_PKG_VERSION"));
}
