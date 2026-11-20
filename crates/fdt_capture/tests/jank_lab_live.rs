//! Runs the real capture against a running `fixtures/jank_lab` (debug or profile).
//!
//! Ignored unless `FDT_JANK_LAB=1`; `scripts/jank-lab-it.sh` starts the app and sets
//! `FDT_JANK_LAB_VM_URI`. The actions are driven through a second VM service connection.
//!
//! Printed timings, all from `Capture::connect()` unless noted:
//! - `attach_ms`: until the status is first `Connected`.
//! - `attach_to_data_ms`: until the first live log record (`emit_levels`, called as soon as the
//!   status is `Connected`) is in a published snapshot.
//! - `first_log_ms`: from the `emit_levels` call until its record is in a snapshot.
//!
//! Hot restart is required in debug (the `hotRestart` service must be registered within 10 s)
//! and skipped, with a message, in profile.

mod common;

use std::time::{Duration, Instant};

use fdt_analysis::explain::{Cause, CpuState, Explanation, explain};
use fdt_analysis::rebuilds::frame_rebuilds;
use fdt_capture::{Capture, CaptureConfig, CaptureRuntime, ConnStatus, Target};
use fdt_store::Snapshot;
use fdt_store::frames::{FrameStatus, frame_status};
use fdt_store::logs::{LogLevel, LogSource};
use fdt_store::net::{BodyState, StatusClass};
use fdt_vm::discovery::{DiscoveredApp, Discovery, DiscoveryConfig};
use fdt_vm::events::{StreamEvent, decode_notification};
use fdt_vm::flutter::PROFILE_USER_WIDGET_BUILDS;
use fdt_vm::rpc::{ConnectOptions, RpcError, connect};
use fdt_vm::service::VmService;
use serde_json::json;
use tokio::time::sleep;

const WAIT: Duration = Duration::from_secs(10);

fn host_port(uri: &str) -> String {
    url::Url::parse(uri)
        .ok()
        .and_then(|u| Some(format!("{}:{}", u.host_str()?, u.port()?)))
        .unwrap_or_default()
}

async fn main_isolate(vm: &VmService) -> String {
    let info = vm.get_vm().await.expect("getVM");
    info.isolates
        .iter()
        .find(|i| i.name == "main" && !i.is_system_isolate)
        .expect("main isolate")
        .id
        .clone()
}

/// Runs an automation action, retrying while its screen has not mounted yet.
async fn act(vm: &VmService, isolate: &str, action: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match vm
            .call_extension("ext.jank_lab.run", isolate, json!({"action": action}))
            .await
        {
            Ok(_) => return,
            Err(RpcError::Server { .. }) if Instant::now() < deadline => {
                // `list` schedules a frame, which a background window may not otherwise draw,
                // so the screen the previous navigation pushed gets built.
                let _ = vm
                    .call_extension("ext.jank_lab.run", isolate, json!({"action": "list"}))
                    .await;
                sleep(Duration::from_millis(100)).await
            }
            Err(e) => {
                let list = vm
                    .call_extension("ext.jank_lab.run", isolate, json!({"action": "list"}))
                    .await;
                panic!("action {action}: {e}; available: {list:?}")
            }
        }
    }
}

/// Frames from `from` on that have both phases, slowest first among those over twice the budget.
fn janky_frames(s: &Snapshot, from: usize) -> Vec<usize> {
    let mut v: Vec<usize> = (from..s.frames.len())
        .filter(|&i| {
            let f = s.frames.get(i).unwrap();
            f.ui.is_some()
                && f.raster.is_some()
                && frame_status(f.time_us(), s.frame_budget_us(f.run)) == FrameStatus::Janky
        })
        .collect();
    v.sort_by_key(|&i| std::cmp::Reverse(s.frames.get(i).unwrap().time_us()));
    v
}

/// The janky frames whose build phase is at least half their UI time. The single slowest frame
/// can be a one-off stall (e.g. the VM serving the CPU-samples fetch), which no build code
/// explains, so the checks look at all of these and require most to be explained.
fn build_janky(s: &Snapshot, from: usize) -> Vec<usize> {
    janky_frames(s, from)
        .into_iter()
        .filter(|&i| {
            let ui = s.frames.get(i).unwrap().ui.unwrap();
            u64::from(ui.build_us) * 2 >= u64::from(ui.ui_us)
        })
        .collect()
}

/// The janky frames dominated by raster.
fn raster_janky(s: &Snapshot, from: usize) -> Vec<usize> {
    janky_frames(s, from)
        .into_iter()
        .filter(|&i| {
            let f = s.frames.get(i).unwrap();
            f.raster_us > s.frame_budget_us(f.run) && f.raster_us > f.build_us
        })
        .collect()
}

fn with_phases(s: &Snapshot, from: usize) -> usize {
    (from..s.frames.len())
        .filter(|&i| {
            let f = s.frames.get(i).unwrap();
            f.ui.is_some() && f.raster.is_some()
        })
        .count()
}

/// Observes for `window`, fetching CPU samples each round, then explains every frame `pick`
/// returns from `from` on. Requires at least 5 judged frames (with `covered_only`, only those with covering CPU samples) and at least 80 % of them accepted
/// by `ok`. Prints the fraction and the slowest accepted frame's sentence.
#[allow(clippy::too_many_arguments)]
async fn attributed(
    cap: &Capture,
    vm: &VmService,
    iso: &str,
    from: usize,
    what: &str,
    window: Duration,
    pick: impl Fn(&Snapshot, usize) -> Vec<usize>,
    covered_only: bool,
    ok: impl Fn(&Explanation) -> bool,
) -> String {
    let end = Instant::now() + window;
    while Instant::now() < end {
        // A background window may stop drawing; `list` schedules a frame.
        let _ = vm
            .call_extension("ext.jank_lab.run", iso, json!({"action": "list"}))
            .await;
        cap.fetch_cpu_samples();
        sleep(Duration::from_millis(700)).await;
    }
    cap.fetch_cpu_samples();
    sleep(Duration::from_millis(1000)).await;
    let snap = cap.snapshots().borrow().clone();
    let candidates = pick(&snap, from);
    let (mut judged, mut good) = (0, 0);
    let mut sentence = None;
    let mut misses: std::collections::BTreeMap<String, usize> = Default::default();
    for &seq in &candidates {
        let e = explain(&snap, seq);
        // Frames without covering CPU samples cannot be attributed from samples.
        if covered_only && e.cpu != CpuState::Covered {
            continue;
        }
        judged += 1;
        if ok(&e) {
            good += 1;
            sentence.get_or_insert(e.sentence);
        } else {
            let kind = match &e.cause {
                Cause::HotFunction { function, .. } => format!("HotFunction({function})"),
                c => format!("{c:?}").chars().take(40).collect(),
            };
            *misses.entry(format!("{kind} cpu={:?}", e.cpu)).or_default() += 1;
        }
    }
    println!(
        "{what}: covered {judged} of {} candidates, attributed {good} of {judged} ({:.0}%); misses {misses:?}; {}",
        candidates.len(),
        100.0 * good as f64 / judged.max(1) as f64,
        sentence.as_deref().unwrap_or("-")
    );
    assert!(judged >= 5, "{what}: only {judged} judged frames");
    assert!(
        good * 5 >= judged * 4,
        "{what}: {good}/{judged} attributed; misses {misses:?}"
    );
    format!("{good}/{judged}")
}

fn frames_len(cap: &Capture) -> usize {
    cap.snapshots().borrow().frames.len()
}

/// Profile-mode checks of phases, explanations, rebuilds and the tracing restore.
async fn perf_checks(cap: &Capture, vm: &VmService, iso: &str) {
    // Jank: CPU samples name the heavy build function.
    act(vm, iso, "home").await;
    let from = frames_len(cap);
    let t = Instant::now();
    act(vm, iso, "goto_jank").await;
    sleep(Duration::from_secs(3)).await;
    cap.fetch_cpu_samples();
    let snap = cap.snapshots().borrow().clone();
    println!(
        "jank: after 3 s frames={} with_phases={} timeline_slices={}",
        snap.frames.len() - from,
        with_phases(&snap, from),
        snap.timeline.tracks().len()
    );
    let snap = common::wait_for(cap, Duration::from_secs(20), "20 frames with phases", |s| {
        with_phases(s, from) >= 20
    })
    .await;
    println!(
        "jank: frames={} with_phases={} wait_ms={}",
        snap.frames.len() - from,
        with_phases(&snap, from),
        t.elapsed().as_millis()
    );
    attributed(
        cap,
        vm,
        iso,
        from,
        "jank (cpu)",
        Duration::from_secs(3),
        build_janky,
        true,
        |e| matches!(&e.cause, Cause::HotFunction { function, .. } if function.contains("_HeavyBuildPanelState.build")),
    )
    .await;

    // Jank with tracing: the widget is named.
    cap.set_trace_widget_builds(true);
    sleep(Duration::from_secs(2)).await;
    let from2 = frames_len(cap);
    attributed(
        cap,
        vm,
        iso,
        from2,
        "jank (tracing)",
        Duration::from_secs(3),
        build_janky,
        false,
        |e| matches!(&e.cause, Cause::WidgetBuild { widget, .. } if &**widget == "HeavyBuildPanel"),
    )
    .await;

    // Raster.
    cap.set_trace_widget_builds(false);
    {
        let snap = cap.snapshots().borrow().clone();
        let mut r: Vec<u32> = (from2..snap.frames.len())
            .map(|i| snap.frames.get(i).unwrap().raster_us)
            .collect();
        r.sort();
        println!(
            "raster baseline: frames={} median_raster_us={}",
            r.len(),
            r.get(r.len() / 2).copied().unwrap_or(0)
        );
    }
    act(vm, iso, "raster_on").await;
    sleep(Duration::from_secs(3)).await;
    let from3 = frames_len(cap);
    attributed(
        cap,
        vm,
        iso,
        from3,
        "raster",
        Duration::from_secs(3),
        raster_janky,
        false,
        |e| matches!(e.cause, Cause::Raster { save_layers } if save_layers >= 10),
    )
    .await;

    // Rebuilds.
    act(vm, iso, "raster_off").await;
    act(vm, iso, "home").await;
    act(vm, iso, "goto_rebuilds").await;
    cap.set_trace_widget_builds(true);
    sleep(Duration::from_secs(2)).await;
    let from4 = frames_len(cap);
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut seen = String::new();
    loop {
        // The user-widget names come from CPU samples, so keep fetching them.
        cap.fetch_cpu_samples();
        sleep(Duration::from_millis(700)).await;
        let snap = cap.snapshots().borrow().clone();
        let hit = (from4..snap.frames.len()).find_map(|i| {
            let r = frame_rebuilds(&snap, i)?;
            let root = r.roots.iter().find(|r| &*r.name == "CartScreen")?;
            let grid = matches!(&root.largest_child, Some((n, _)) if &**n == "ProductGrid");
            let tiles = r.widgets.iter().find(|w| &*w.name == "ProductTile")?.builds;
            seen = format!("frame {i}: roots={:?} tiles={tiles}", r.roots);
            (grid && tiles == 64).then_some((i, e_sentence(&snap, i)))
        });
        if let Some((i, sentence)) = hit {
            let root = &frame_rebuilds(&snap, i).unwrap().roots[0];
            println!(
                "rebuilds: frame {i}: {sentence} root={} widgets={} largest_child={:?}",
                root.name, root.widgets, root.largest_child
            );
            break;
        }
        if Instant::now() >= deadline {
            dump_tree(&snap, snap.frames.len() - 2);
        }
        assert!(Instant::now() < deadline, "no rebuilds frame; last: {seen}");
    }

    // Restore: tracing is on, and a user disconnect turns it back off.
    assert!(
        vm.get_bool_extension(PROFILE_USER_WIDGET_BUILDS, iso)
            .await
            .unwrap(),
        "tracing should be on before the disconnect"
    );
    cap.disconnect();
    let t = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if !vm
            .get_bool_extension(PROFILE_USER_WIDGET_BUILDS, iso)
            .await
            .unwrap()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "tracing still on after disconnect"
        );
        sleep(Duration::from_millis(100)).await;
    }
    println!(
        "restore: tracing off {} ms after disconnect",
        t.elapsed().as_millis()
    );
}

fn e_sentence(snap: &Snapshot, i: usize) -> String {
    explain(snap, i).sentence
}

/// Debug mode: frames only need their phases.
async fn debug_checks(cap: &Capture, vm: &VmService, iso: &str) {
    act(vm, iso, "home").await;
    let from = frames_len(cap);
    act(vm, iso, "goto_jank").await;
    let snap = common::wait_for(cap, Duration::from_secs(20), "frames with phases", |s| {
        with_phases(s, from) >= 1
    })
    .await;
    println!(
        "debug: frames={} with_phases={}",
        snap.frames.len() - from,
        with_phases(&snap, from)
    );
}

#[test]
#[ignore = "needs a running jank_lab; run scripts/jank-lab-it.sh"]
fn live_capture_of_jank_lab() {
    assert_eq!(
        std::env::var("FDT_JANK_LAB").as_deref(),
        Ok("1"),
        "set FDT_JANK_LAB=1 (scripts/jank-lab-it.sh does)"
    );
    let vm_uri = std::env::var("FDT_JANK_LAB_VM_URI").expect("FDT_JANK_LAB_VM_URI");
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(run(&rt, &vm_uri));
}

async fn run(rt: &CaptureRuntime, vm_uri: &str) {
    // Discovery over the real registry directory; other apps on this machine are ignored.
    let (_discovery, mut apps) = Discovery::spawn(&rt.handle(), DiscoveryConfig::default());
    let want = host_port(vm_uri);
    let found = |list: &[DiscoveredApp]| {
        let named: Vec<&DiscoveredApp> = list.iter().filter(|a| a.name == "jank_lab").collect();
        named
            .iter()
            .find(|a| host_port(&a.vm_uri) == want)
            .or(named.first())
            .map(|a| (*a).clone())
    };
    let app = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(a) = found(&apps.borrow_and_update()) {
                return a;
            }
            apps.changed().await.expect("discovery ended");
        }
    })
    .await
    .expect("discovery did not list jank_lab within 5 s");
    println!(
        "discovered {} mode={:?} uri={}",
        app.name, app.mode, app.vm_uri
    );

    // The driving connection, opened first so it is not part of the attach timings.
    let url = url::Url::parse(vm_uri).expect("vm uri");
    let (client, mut notes) = connect(&url, ConnectOptions::default())
        .await
        .expect("driver connect");
    let vm = VmService::new(client);
    let mut iso = main_isolate(&vm).await;
    vm.stream_listen("Service").await.ok();

    let cap = Capture::start(rt.handle(), CaptureConfig::default(), None);
    let t0 = Instant::now();
    cap.connect(Target::Discovered(app));
    let mut status = cap.status();
    tokio::time::timeout(
        Duration::from_secs(20),
        status.wait_for(|s| matches!(s, ConnStatus::Connected { .. })),
    )
    .await
    .unwrap_or_else(|_| panic!("never connected: {:?}", *cap.status().borrow()))
    .unwrap();
    let attach_ms = t0.elapsed().as_millis();
    let is_debug = match &*cap.status().borrow() {
        ConnStatus::Connected { app, .. } => app.mode == fdt_store::run::Mode::Debug,
        _ => unreachable!(),
    };
    println!("attach_ms={attach_ms} debug={is_debug}");

    // Logs. `emit_levels` is called as soon as the status is Connected, so the first live
    // record also gives `attach_to_data_ms`.
    act(&vm, &iso, "goto_logs").await;
    let t_logs = Instant::now();
    act(&vm, &iso, "emit_levels").await;
    common::wait_for(&cap, WAIT, "emit_levels records", |s| {
        (0..s.logs.len()).any(|i| s.logs.row(i).source == LogSource::Log)
    })
    .await;
    println!("attach_to_data_ms={}", t0.elapsed().as_millis());
    common::wait_for(&cap, WAIT, "emit_levels records", |s| {
        (0..s.logs.len()).any(|i| {
            let r = s.logs.row(i);
            r.source == LogSource::Log && r.logger == "jank_lab.net" && r.level == LogLevel::Debug
        })
    })
    .await;
    println!("first_log_ms={}", t_logs.elapsed().as_millis());
    act(&vm, &iso, "print_burst").await;
    act(&vm, &iso, "multi_line").await;
    act(&vm, &iso, "report_error").await;

    let snap = common::wait_for(&cap, WAIT, "all log records", |s| {
        let mut burst = 0;
        let mut thirty = false;
        let mut err = false;
        for i in 0..s.logs.len() {
            let r = s.logs.row(i);
            if r.source == LogSource::Stdout && r.text.starts_with("burst line ") {
                burst += 1;
            }
            thirty |= r.line_count == 30 && r.text.ends_with("line 30 of 30");
            err |= r.source == LogSource::FlutterError;
        }
        burst >= 1000 && thirty && (err || !is_debug)
    })
    .await;
    let rows: Vec<_> = (0..snap.logs.len()).map(|i| snap.logs.row(i)).collect();
    for (logger, level) in [
        ("jank_lab.net", LogLevel::Debug),
        ("jank_lab.net", LogLevel::Warning),
        ("jank_lab.ui", LogLevel::Info),
        ("jank_lab.ui", LogLevel::Error),
    ] {
        assert!(
            rows.iter()
                .any(|r| r.source == LogSource::Log && r.logger == logger && r.level == level),
            "missing {logger} {level:?}"
        );
    }
    let burst: Vec<&str> = rows
        .iter()
        .filter(|r| r.source == LogSource::Stdout && r.text.starts_with("burst line "))
        .map(|r| r.text)
        .collect();
    assert_eq!(burst.len(), 1000, "burst lines");
    let thirty = rows
        .iter()
        .find(|r| r.line_count == 30 && r.text.ends_with("line 30 of 30"))
        .expect("30-line record");
    assert_eq!(thirty.text.lines().count(), 30, "record arrived in full");
    if is_debug {
        assert!(rows.iter().any(|r| r.source == LogSource::FlutterError));
    }
    drop(rows);

    // Network.
    act(&vm, &iso, "pop").await;
    // Let the pop finish before pushing, as a person would.
    sleep(Duration::from_millis(500)).await;
    act(&vm, &iso, "goto_network").await;
    act(&vm, &iso, "load_products").await;
    act(&vm, &iso, "place_order").await;
    act(&vm, &iso, "refused").await;
    act(&vm, &iso, "open_socket").await;
    let snap = common::wait_for(&cap, WAIT, "network requests", |s| {
        let done = |tail: &str| {
            (0..s.net.len()).map(|i| s.net.request(i)).any(|r| {
                r.url.ends_with(tail)
                    && r.end.is_some()
                    && !matches!(r.response_body, BodyState::Pending)
                    && !matches!(r.request_body, BodyState::Pending)
            })
        };
        done("/v2/products?page=3")
            && done("/v2/orders")
            && done("http://127.0.0.1:1/")
            && (0..s.net.len()).any(|i| {
                let r = s.net.request(i);
                r.websocket && s.net.socket_for(r.id).is_some()
            })
    })
    .await;
    let find = |tail: &str| {
        (0..snap.net.len())
            .map(|i| &**snap.net.request(i))
            .find(|r| r.url.ends_with(tail))
            .unwrap_or_else(|| panic!("no request ending in {tail}"))
    };
    let products = find("/v2/products?page=3");
    assert_eq!(
        (products.method.as_str(), products.status),
        ("GET", Some(200))
    );
    match &products.response_body {
        BodyState::Captured(b) => assert!(
            serde_json::from_slice::<serde_json::Value>(&b.bytes).is_ok(),
            "products body is JSON"
        ),
        other => panic!("products body: {other:?}"),
    }
    let orders = find("/v2/orders");
    assert_eq!((orders.method.as_str(), orders.status), ("POST", Some(500)));
    match &orders.request_body {
        BodyState::Captured(b) => assert_eq!(&b.bytes[..], br#"{"cart":1}"#),
        other => panic!("order request body: {other:?}"),
    }
    let refused = find("http://127.0.0.1:1/");
    assert_eq!(refused.status_class(), StatusClass::Failed);
    assert!(refused.is_error(), "refused shows under the Errors filter");
    let ws = find("/ws");
    assert!(ws.websocket && snap.net.socket_for(ws.id).is_some());

    // Hot restart: the capture must keep recording in the same run.
    // `flutter run` always registers `hotRestart` in debug; only profile may lack it.
    let hot_restart = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(n) = notes.recv().await {
            if let Some(StreamEvent::ServiceRegistered { method, .. }) = decode_notification(&n)
                && method.ends_with("hotRestart")
            {
                return Some(method);
            }
        }
        None
    })
    .await
    .ok()
    .flatten();
    assert!(
        hot_restart.is_some() || !is_debug,
        "no hotRestart service registered in a debug app"
    );
    match hot_restart {
        Some(method) => {
            let before = snap.net.len();
            let run_before = match &*cap.status().borrow() {
                ConnStatus::Connected { run, .. } => *run,
                other => panic!("not connected: {other:?}"),
            };
            vm.call_service(&method, &iso).await.expect("hot restart");
            // The isolate id changes; wait for the new main isolate.
            let deadline = Instant::now() + WAIT;
            let old = iso.clone();
            loop {
                if let Ok(info) = vm.get_vm().await
                    && let Some(i) = info
                        .isolates
                        .iter()
                        .find(|i| i.name == "main" && !i.is_system_isolate && i.id != old)
                {
                    iso = i.id.clone();
                    break;
                }
                assert!(Instant::now() < deadline, "no new main isolate");
                sleep(Duration::from_millis(100)).await;
            }
            // The restarted app may not have a navigator yet, which ignores the push silently.
            let deadline = Instant::now() + WAIT;
            loop {
                act(&vm, &iso, "goto_network").await;
                sleep(Duration::from_millis(300)).await;
                let list = vm
                    .call_extension("ext.jank_lab.run", &iso, json!({"action": "list"}))
                    .await
                    .expect("list");
                if list.to_string().contains("load_products") {
                    break;
                }
                assert!(Instant::now() < deadline, "network screen never mounted");
            }
            act(&vm, &iso, "load_products").await;
            let snap = common::wait_for(&cap, WAIT, "request after hot restart", |s| {
                s.net.len() > before
                    && (before..s.net.len()).any(|i| s.net.request(i).url.contains("/v2/products"))
            })
            .await;
            assert!(snap.net.len() > before);
            let run_after = match &*cap.status().borrow() {
                ConnStatus::Connected { run, .. } => *run,
                other => panic!("not connected after restart: {other:?}"),
            };
            assert_eq!(run_before, run_after, "same run after hot restart");
            println!("hot restart: ok");
        }
        None => println!("hot restart: skipped (no hotRestart service registered; profile mode)"),
    }
    if is_debug {
        debug_checks(&cap, &vm, &iso).await;
    } else {
        perf_checks(&cap, &vm, &iso).await;
    }
    drop(cap);
}

/// Prints the first slices of each lane inside the frame's UI window (diagnostics).
fn dump_tree(snap: &Snapshot, seq: usize) {
    use fdt_store::time::{SessionTime, TimeRange};
    let f = snap.frames.get(seq).unwrap();
    let ui = f.ui.unwrap();
    let w = TimeRange::new(
        ui.begin,
        SessionTime(ui.begin.0 + u64::from(ui.ui_us) * 1000),
    );
    let t = snap
        .timeline
        .tracks()
        .iter()
        .find(|t| t.run == f.run && t.order == 0)
        .unwrap();
    for (d, lane) in snap.timeline.lanes(t.id).iter().enumerate().take(14) {
        let names: Vec<String> = lane
            .range_indices(w)
            .map(|i| snap.timeline.name(lane.get(i).name).to_string())
            .collect();
        let mut head: Vec<&str> = names.iter().map(|s| s.as_str()).take(8).collect();
        head.dedup();
        println!("TREE d{d} n={} {head:?}", names.len());
    }
}
