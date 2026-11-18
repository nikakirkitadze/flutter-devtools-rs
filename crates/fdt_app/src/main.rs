use std::process::ExitCode;

fn main() -> ExitCode {
    let t0 = std::time::Instant::now();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && args[0] == "--version" {
        println!("fdt {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args.is_empty() {
        fdt_app::app::run();
        return ExitCode::SUCCESS;
    }
    if args.len() == 1 && args[0] == "--startup-trace" {
        fdt_app::app::run_with_startup_trace(t0);
        return ExitCode::SUCCESS;
    }
    eprintln!("usage: fdt [--version] [--startup-trace]");
    ExitCode::from(2)
}
