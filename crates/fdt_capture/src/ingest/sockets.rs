//! Socket profile capture. Socket times are on the timeline clock.

use fdt_store::clock::RunClock;
use fdt_store::net::SocketUpdate;
use fdt_store::time::RunId;
use fdt_vm::dart_io::SocketStat;
use fdt_vm::rpc::RpcError;
use fdt_vm::service::VmService;

use super::{Ingest, Sink};

pub(crate) fn to_socket_update(
    s: &SocketStat,
    clock: &RunClock,
    run: RunId,
    isolate: &str,
) -> Option<SocketUpdate> {
    let time = |us: i64| clock.from_timeline_us(us).time;
    Some(SocketUpdate {
        run,
        isolate: isolate.to_string(),
        vm_id: s.id.clone(),
        address: s.address.clone(),
        port: u16::try_from(s.port).ok()?,
        kind: s.socket_type.clone(),
        start: time(s.start_time),
        end: s.end_time.map(time),
        read_bytes: s.read_bytes.max(0) as u64,
        write_bytes: s.write_bytes.max(0) as u64,
        last_read: s.last_read_time.map(time),
        last_write: s.last_write_time.map(time),
    })
}

/// Polls one isolate's sockets. Returns false when the connection is gone.
pub(crate) async fn poll(sink: &Sink, svc: &VmService, isolate: &str) -> bool {
    match svc.get_socket_profile(isolate).await {
        Ok(stats) => {
            let updates: Vec<_> = stats
                .iter()
                .filter_map(|s| to_socket_update(s, &sink.clock, sink.run, isolate))
                .collect();
            if !updates.is_empty() {
                sink.send(Ingest::Sockets(updates));
            }
            true
        }
        Err(RpcError::Closed) => false,
        Err(e) => {
            log::debug!("getSocketProfile failed: {e}");
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdt_store::time::SessionTime;
    use serde_json::json;

    #[test]
    fn socket_times_use_the_timeline_clock() {
        let clock = RunClock {
            origin_host_us: 0,
            skew_us: 10,
            timeline_to_wall_us: 1_000,
        };
        let s: SocketStat = serde_json::from_value(json!({"id": "5", "address": "127.0.0.1",
            "port": 9000, "socketType": "tcp", "startTime": 500, "endTime": 600,
            "readBytes": 7, "writeBytes": 8, "lastReadTime": 550}))
        .unwrap();
        let u = to_socket_update(&s, &clock, RunId(1), "isolates/1").unwrap();
        assert_eq!(u.start, SessionTime((500 + 1_000 + 10) * 1000));
        assert_eq!(u.end, Some(SessionTime((600 + 1_000 + 10) * 1000)));
        assert_eq!(u.last_read, Some(SessionTime((550 + 1_000 + 10) * 1000)));
        assert_eq!(u.last_write, None);
        assert_eq!((u.read_bytes, u.write_bytes, u.port), (7, 8, 9000));
    }
}
