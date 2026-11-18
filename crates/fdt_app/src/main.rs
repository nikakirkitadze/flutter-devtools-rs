use std::process::ExitCode;

fn main() -> ExitCode {
    fdt_app::logging::init();
    let t0 = std::time::Instant::now();
    let main_entry_unix_ms = fdt_app::startup::unix_ms_now();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && args[0] == "--version" {
        println!("fdt {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args.is_empty() {
        return fdt_app::app::run();
    }
    if args.len() == 1 && args[0] == "--startup-trace" {
        return fdt_app::app::run_with_startup_trace(t0, main_entry_unix_ms);
    }
    eprintln!("usage: fdt [--version] [--startup-trace]");
    ExitCode::from(2)
}
