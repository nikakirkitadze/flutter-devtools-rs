use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && args[0] == "--version" {
        println!("fdt {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args.is_empty() {
        fdt_app::app::run();
        return ExitCode::SUCCESS;
    }
    eprintln!("usage: fdt [--version]");
    ExitCode::from(2)
}
