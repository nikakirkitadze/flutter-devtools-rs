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
    if args[0] == "--connect" {
        let Some(url) = args.get(1).filter(|_| args.len() == 2) else {
            eprintln!("usage: fdt --connect <vm-service-url>");
            return ExitCode::from(2);
        };
        return match fdt_vm::uri::VmServiceUri::parse(url) {
            Ok(uri) => fdt_app::app::run_connected(uri),
            Err(err) => {
                eprintln!("fdt: {}", err.message());
                ExitCode::from(2)
            }
        };
    }
    if args[0] == "bench-ui" {
        return fdt_app::bench_ui::main(&args[1..]);
    }
    eprintln!(
        "usage: fdt [--version] [--startup-trace] [--connect <vm-service-url>] [bench-ui <scenario>]"
    );
    ExitCode::from(2)
}
