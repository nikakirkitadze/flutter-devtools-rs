//! The connect sequence and the event loop of one connection.

use fdt_store::clock::{Calibration, calibrate, host_now_us};
use fdt_store::run::{AppInfo, Mode as StoreMode};
use fdt_vm::dart_io::HTTP_CALIBRATION_SINCE;
use fdt_vm::events::decode_notification;
use fdt_vm::rpc::{ConnectOptions, connect};
use fdt_vm::service::{Mode, VmService, detect_mode};
use fdt_vm::types::{Isolate, IsolateRef, Vm};
use fdt_vm::uri::VmServiceUri;
use tokio::sync::{mpsc, oneshot};

use crate::capture::{SessionMsg, Target};
use crate::config::CaptureConfig;
use crate::ingest::{Ingest, Router, Sink};

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
            message: e.to_string(),
            details: format!("{e:?}"),
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

async fn calibrate_clock(svc: &VmService, main: &str) -> Calibration {
    let before = host_now_us();
    let timeline = svc.get_vm_timeline_micros().await;
    let profile = svc
        .get_http_profile(main, Some(HTTP_CALIBRATION_SINCE))
        .await;
    let after = host_now_us();
    match (timeline, profile) {
        (Ok(t), Ok(p)) => calibrate(before, after, p.timestamp, t),
        (timeline, _) => {
            log::warn!("clock calibration failed; assuming the device clock matches the host");
            Calibration {
                skew_us: 0,
                timeline_to_wall_us: timeline.map_or(0, |t| (before + after) / 2 - t),
            }
        }
    }
}

pub(crate) async fn run(
    target: Target,
    cfg: CaptureConfig,
    conn: u64,
    tx: mpsc::UnboundedSender<SessionMsg>,
) {
    match connect_and_ingest(target, &cfg, conn, &tx).await {
        Ok(reason) => log::info!("connection ended: {reason}"),
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
    let (client, mut notes) = connect(uri.ws_url(), ConnectOptions::default())
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
    let (isolate, listened, calibration) =
        tokio::join!(svc.get_isolate(&main), listen, calibrate_clock(&svc, &main));
    let isolate = isolate.map_err(Fail::rpc)?;
    listened.map_err(Fail::rpc)?;

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
            calibration,
            reply,
        },
    ));
    let (run, clock) = run_rx.await.map_err(|_| Fail {
        message: "The session closed.".into(),
        details: String::new(),
    })?;
    let sink = Sink::new(run, clock, conn, tx.clone());
    let mut router = Router::new(sink, svc, main, cfg.stdout_flush, cfg.string_fetch_timeout);
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
