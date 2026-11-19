use std::process::Command;

#[test]
fn version_flag_prints_name_and_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_fdt"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("fdt {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unknown_argument_prints_usage_and_exits_2() {
    let out = Command::new(env!("CARGO_BIN_EXE_fdt"))
        .arg("--bogus")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.starts_with("usage: fdt"));
    assert!(stderr.contains("--startup-trace"));
}

#[test]
fn bench_ui_without_a_scenario_prints_usage_and_exits_2() {
    let out = Command::new(env!("CARGO_BIN_EXE_fdt"))
        .arg("bench-ui")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.starts_with("usage: fdt bench-ui <scenario>"));
    assert!(stderr.contains("logs-scroll"));
    assert!(out.stdout.is_empty());
}

#[test]
fn bench_ui_with_an_unknown_scenario_or_bad_frames_exits_2() {
    for args in [
        vec!["bench-ui", "bogus"],
        vec!["bench-ui", "logs-scroll", "--frames"],
        vec!["bench-ui", "logs-scroll", "--frames", "x"],
        vec!["bench-ui", "logs-scroll", "--frames", "0"],
        vec!["bench-ui", "logs-scroll", "--nope"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_fdt"))
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
    }
}
