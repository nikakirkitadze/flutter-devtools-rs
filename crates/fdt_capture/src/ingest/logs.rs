//! `Logging` stream events. One worker task handles them in order; a slow `getObject` delays
//! only later log records, never stdout or extension events.

use std::time::Duration;

use fdt_store::logs::{LogInput, LogSource, level_from_dart};
use fdt_vm::service::VmService;
use fdt_vm::types::InstanceRef;
use tokio::sync::mpsc;

use super::{Ingest, Sink};

pub(crate) struct LoggingEvent {
    pub(crate) isolate: String,
    pub(crate) time_ms: i64,
    pub(crate) level: i64,
    pub(crate) logger: InstanceRef,
    pub(crate) message: InstanceRef,
    pub(crate) error: InstanceRef,
    pub(crate) stack_trace: InstanceRef,
}

pub(crate) fn spawn_worker(
    sink: Sink,
    svc: VmService,
    fetch_timeout: Duration,
) -> mpsc::UnboundedSender<LoggingEvent> {
    let (tx, mut rx) = mpsc::unbounded_channel::<LoggingEvent>();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let input = build(&sink, &svc, fetch_timeout, ev).await;
            sink.send(Ingest::Log(input));
        }
    });
    tx
}

/// The text of a string instance, fetched in full when the event carried a truncated copy.
async fn full_text(
    svc: &VmService,
    isolate: &str,
    inst: &InstanceRef,
    fetch_timeout: Duration,
) -> String {
    let text = inst.value_as_string.clone().unwrap_or_default();
    let (true, Some(id)) = (inst.value_as_string_is_truncated, inst.id.as_deref()) else {
        return text;
    };
    match tokio::time::timeout(fetch_timeout, svc.get_object_string(isolate, id)).await {
        Ok(Ok(full)) => full,
        other => {
            log::debug!("could not fetch a truncated log string: {other:?}");
            format!("{text}…")
        }
    }
}

/// Like [`full_text`], but `None` for a null instance and the class name when no text came.
async fn optional_text(
    svc: &VmService,
    isolate: &str,
    inst: &InstanceRef,
    fetch_timeout: Duration,
) -> Option<String> {
    if inst.kind == "Null" {
        return None;
    }
    if inst.value_as_string.is_some() {
        return Some(full_text(svc, isolate, inst, fetch_timeout).await);
    }
    inst.class_name.clone()
}

async fn build(sink: &Sink, svc: &VmService, t: Duration, ev: LoggingEvent) -> LogInput {
    let (logger, mut text, error, stack) = tokio::join!(
        full_text(svc, &ev.isolate, &ev.logger, t),
        full_text(svc, &ev.isolate, &ev.message, t),
        optional_text(svc, &ev.isolate, &ev.error, t),
        optional_text(svc, &ev.isolate, &ev.stack_trace, t),
    );
    if let Some(e) = error {
        text.push_str("\nError: ");
        text.push_str(&e);
    }
    if let Some(s) = stack {
        text.push_str("\nStack trace:\n");
        text.push_str(&s);
    }
    let st = sink.clock.from_epoch_ms(ev.time_ms);
    LogInput {
        run: sink.run,
        time: st.time,
        clamped: st.clamped,
        level: level_from_dart(ev.level),
        source: LogSource::Log,
        logger: if ev.logger.kind == "Null" {
            String::new()
        } else {
            logger
        },
        text,
        error: None,
    }
}
