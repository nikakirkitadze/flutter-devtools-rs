//! The connect sequence and the event loop of one connection.

use fdt_store::clock::{Calibration, calibrate, host_now_us};
use fdt_store::run::{AppInfo, Mode as StoreMode};
use fdt_vm::dart_io::HTTP_CALIBRATION_SINCE;
use fdt_vm::events::decode_notification;
use fdt_vm::rpc::{ConnectOptions, connect};
use fdt_vm::service::{Mode, VmService, detect_mode};
use fdt_vm::timeline::wanted_streams;
use fdt_vm::types::{Isolate, IsolateRef, Vm};
use fdt_vm::uri::VmServiceUri;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::capture::{SessionMsg, Target};
use crate::config::CaptureConfig;
use crate::ingest::http::{BodyFetcher, ENABLE_RPC, NetHub, PollerConfig};
use crate::ingest::perf::{PerfConfig, PerfHub};
use crate::ingest::{Ingest, Router};

const STREAMS: [&str; 6] = [
    "Logging",
    "Stdout",
    "Stderr",
    "Extension",
    "Isolate",
    "Service",
];

struct Fail {
    message: String,
    details: String,
}

impl Fail {
    fn rpc(e: fdt_vm::rpc::RpcError) -> Fail {
        Fail {
            message: "Connected, but the app didn't answer as expected.".to_string(),
            details: format!("{e} ({e:?})"),
        }
    }
}

/// The label shown while connecting; never contains the URL's token.
pub(crate) fn label_of(target: &Target) -> String {
    match target {
        Target::Uri(u) => u.redacted(),
        Target::Discovered(a) => a.name.clone(),
    }
}

fn main_isolate(vm: &Vm) -> Option<&IsolateRef> {
    let mut user = vm.isolates.iter().filter(|i| !i.is_system_isolate);
    vm.isolates
        .iter()
        .find(|i| i.name == "main" && !i.is_system_isolate)
        .or_else(|| user.next())
}

fn package_name(isolate: &Isolate) -> Option<String> {
    let uri = &isolate.root_lib.as_ref()?.uri;
    let rest = uri.strip_prefix("package:")?;
    let name = rest.split('/').next()?;
    (!name.is_empty()).then(|| name.to_string())
}

fn store_mode(m: Mode) -> StoreMode {
    match m {
        Mode::Debug => StoreMode::Debug,
        Mode::Profile => StoreMode::Profile,
    }
}

/// Pairs the device and host clocks. The extension call waits on the app's main isolate, which
/// can be paused or busy, so it gets `timeout` and then the fallback below. Also returns the
/// timeline clock at the moment of the calibration (0 when it could not be read).
async fn calibrate_clock(svc: &VmService, main: &str, timeout: Duration) -> (Calibration, i64) {
    let before = host_now_us();
    let timeline = svc.get_vm_timeline_micros().await;
    let profile = svc
        .get_http_profile_with_timeout(main, Some(HTTP_CALIBRATION_SINCE), timeout)
        .await;
    let after = host_now_us();
    match (timeline, profile) {
        (Ok(t), Ok(p)) => (calibrate(before, after, p.timestamp, t), t),
        (timeline, _) => {
            log::warn!("clock calibration failed; assuming the device clock matches the host");
            let cal = Calibration {
                skew_us: 0,
                timeline_to_wall_us: timeline.as_ref().map_or(0, |t| (before + after) / 2 - *t),
            };
            (cal, timeline.unwrap_or(0))
        }
    }
}

/// The display's refresh rate. Any failure, and any value that is not a positive number, is
/// "unknown": the frame budget falls back to 60 Hz.
async fn read_refresh_rate(svc: &VmService, main: &str, timeout: Duration) -> Option<f32> {
    match tokio::time::timeout(timeout, svc.display_refresh_rate(main)).await {
        Ok(Ok(Some(hz))) if hz.is_finite() && hz > 0.0 => Some(hz),
        Ok(Ok(other)) => {
            log::debug!("no usable refresh rate: {other:?}");
            None
        }
        Ok(Err(e)) => {
            log::debug!("reading the refresh rate failed: {e}");
            None
        }
        Err(_) => {
            log::debug!("reading the refresh rate timed out");
            None
        }
    }
}

/// Makes the VM record the `Dart` and `Embedder` timeline streams, keeping what else is recorded.
/// Failures are only logged: the timeline is a feature, not a requirement of connecting.
async fn fix_timeline_flags(svc: &VmService, timeout: Duration) {
    let flags = match tokio::time::timeout(timeout, svc.get_vm_timeline_flags()).await {
        Ok(Ok(f)) => f,
        Ok(Err(e)) => return log::debug!("getVMTimelineFlags failed: {e}"),
        Err(_) => return log::debug!("getVMTimelineFlags timed out"),
    };
    let Some(want) = wanted_streams(&flags) else {
        return;
    };
    let names: Vec<&str> = want.iter().map(String::as_str).collect();
    match tokio::time::timeout(timeout, svc.set_vm_timeline_flags(&names)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::warn!("setVMTimelineFlags failed: {e}"),
        Err(_) => log::warn!("setVMTimelineFlags timed out"),
    }
}

pub(crate) async fn run(
    target: Target,
    cfg: CaptureConfig,
    conn: u64,
    tx: mpsc::UnboundedSender<SessionMsg>,
    free_app_memory: Arc<AtomicBool>,
) {
    match connect_and_ingest(target, &cfg, conn, &tx, free_app_memory).await {
        Ok(reason) => {
            log::info!("connection ended: {reason}");
            let _ = tx.send(SessionMsg::Ingest(conn, Ingest::Closed));
        }
        Err(f) => {
            log::info!("connect failed: {}", f.message);
            let _ = tx.send(SessionMsg::Ingest(
                conn,
                Ingest::Failed {
                    message: f.message,
                    details: f.details,
                },
            ));
        }
    }
}

async fn connect_and_ingest(
    target: Target,
    cfg: &CaptureConfig,
    conn: u64,
    tx: &mpsc::UnboundedSender<SessionMsg>,
    free_app_memory: Arc<AtomicBool>,
) -> Result<String, Fail> {
    let (uri, preset) = match &target {
        Target::Uri(u) => (u.clone(), None),
        Target::Discovered(a) => {
            let u = VmServiceUri::parse(&a.vm_uri).map_err(|e| Fail {
                message: e.message().to_string(),
                details: format!("{e:?}"),
            })?;
            (u, Some(a))
        }
    };
    log::info!("connecting to {}", uri.redacted());
    // Everything up to the start of the run is bounded: a server that is not a VM service, or an
    // isolate that is paused, must not leave the window waiting on the 30 s call default.
    let first = tokio::time::timeout(cfg.connect_deadline, async {
        let (client, notes) = connect(uri.ws_url(), ConnectOptions::default())
            .await
            .map_err(|e| Fail {
                message: e.message(),
                details: format!("{e:?}"),
            })?;
        let svc = VmService::new(client.clone());

        let vm = svc.get_vm().await.map_err(Fail::rpc)?;
        let main = main_isolate(&vm)
            .ok_or_else(|| Fail {
                message: "The app has no running isolate yet.".into(),
                details: format!("{vm:?}"),
            })?
            .id
            .clone();

        // Notifications that arrive while these run wait in the client's channel, in order.
        let listen = async {
            for s in STREAMS {
                if let Err(e) = svc.stream_listen(s).await {
                    if e == fdt_vm::rpc::RpcError::Closed {
                        return Err(e);
                    }
                    log::warn!("streamListen {s} failed: {e}");
                }
            }
            Ok(())
        };
        let (isolate, listened, (calibration, connect_timeline_us)) = tokio::join!(
            svc.get_isolate(&main),
            listen,
            calibrate_clock(&svc, &main, cfg.calibration_timeout)
        );
        let isolate = isolate.map_err(Fail::rpc)?;
        listened.map_err(Fail::rpc)?;
        // Other isolates already running (not only `main`) that expose the HTTP profile.
        let mut profiled = Vec::new();
        for other in vm
            .isolates
            .iter()
            .filter(|i| !i.is_system_isolate && i.id != main)
        {
            match svc.get_isolate(&other.id).await {
                Ok(i) if i.extension_rpcs.iter().any(|r| r == ENABLE_RPC) => {
                    profiled.push(other.id.clone());
                }
                Ok(_) => {}
                Err(fdt_vm::rpc::RpcError::Closed) => break,
                Err(e) => log::debug!("getIsolate {} failed: {e}", other.id),
            }
        }
        let refresh_hz = read_refresh_rate(&svc, &main, cfg.refresh_timeout).await;
        fix_timeline_flags(&svc, cfg.refresh_timeout).await;
        Ok::<_, Fail>((
            client,
            notes,
            svc,
            vm,
            main,
            isolate,
            calibration,
            profiled,
            refresh_hz,
            connect_timeline_us,
        ))
    })
    .await;
    let (
        client,
        mut notes,
        svc,
        vm,
        main,
        isolate,
        calibration,
        profiled,
        refresh_hz,
        connect_timeline_us,
    ) = match first {
        Ok(r) => r?,
        Err(_) => {
            return Err(Fail {
                message: format!(
                    "The app didn't answer within {} seconds. Is it paused in the debugger?",
                    cfg.connect_deadline.as_secs()
                ),
                details: "The connect sequence timed out.".into(),
            });
        }
    };

    let mode = store_mode(detect_mode(&isolate));
    let (name, device, workspace) = match preset {
        Some(a) => (a.name.clone(), Some(a.device.clone()), a.workspace.clone()),
        None => (
            package_name(&isolate).unwrap_or_else(|| "Dart app".to_string()),
            Some(vm.operating_system.clone()),
            None,
        ),
    };
    let app = AppInfo {
        name,
        device,
        mode,
        flutter_version: preset.and_then(|a| a.flutter_version.clone()),
        workspace,
        vm_uri: uri.ws_url().to_string(),
        pid: vm.pid,
    };

    let (reply, run_rx) = oneshot::channel();
    let _ = tx.send(SessionMsg::Ingest(
        conn,
        Ingest::StartRun {
            app: Box::new(app),
            vm_start_time: vm.start_time,
            calibration,
            refresh_hz,
            connect_timeline_us,
            reply,
        },
    ));
    let sink = run_rx.await.map_err(|_| Fail {
        message: "The session closed.".into(),
        details: String::new(),
    })?;
    let fetcher = Arc::new(BodyFetcher::new(
        svc.clone(),
        sink.clone(),
        cfg.body_fetch_timeout,
        cfg.auto_fetch_limit,
    ));
    sink.get().send(Ingest::NetReady(fetcher.clone()));
    let mut hub = NetHub::new(
        sink.clone(),
        svc.clone(),
        fetcher,
        PollerConfig {
            http_poll: cfg.http_poll,
            socket_poll: cfg.socket_poll,
            free_app_memory,
        },
    );
    if isolate.extension_rpcs.iter().any(|r| r == ENABLE_RPC) {
        hub.start(&main);
    }
    for id in &profiled {
        hub.start(id);
    }
    let perf = PerfHub::new(
        sink.clone(),
        svc.clone(),
        PerfConfig {
            poll: cfg.timeline_poll,
            call_timeout: cfg.timeline_call_timeout,
            cpu_poll: cfg.cpu_poll,
            cpu_call_timeout: cfg.cpu_call_timeout,
            start_us: connect_timeline_us,
        },
        main.clone(),
    );
    sink.get().send(Ingest::PerfReady(perf.control()));
    let mut router = Router::new(
        sink,
        svc,
        main,
        cfg.stdout_flush,
        cfg.string_fetch_timeout,
        hub,
        perf,
    );
    loop {
        let flush = router.next_flush();
        tokio::select! {
            n = notes.recv() => match n {
                Some(n) => {
                    if let Some(ev) = decode_notification(&n) {
                        router.on_event(ev);
                    }
                }
                None => break,
            },
            _ = async {
                match flush {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => router.flush_due(tokio::time::Instant::now()),
        }
    }
    router.flush_due(tokio::time::Instant::now() + std::time::Duration::from_secs(3600));
    let close = client.closed().await;
    Ok(format!("closed ({:?}) {}", close.code, close.reason))
}
