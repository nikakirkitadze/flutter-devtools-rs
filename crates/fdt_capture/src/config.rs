use std::time::Duration;

use fdt_store::net::BodyLimits;

#[derive(Clone, Debug)]
pub struct CaptureConfig {
    pub publish_interval: Duration,
    pub http_poll: Duration,
    pub socket_poll: Duration,
    /// How often `getVMTimeline` is polled while not in stream mode.
    pub timeline_poll: Duration,
    /// One `getVMTimeline` call gives up after this long.
    pub timeline_call_timeout: Duration,
    /// The refresh-rate read and the timeline-flags calls of the connect sequence wait this long.
    pub refresh_timeout: Duration,
    pub body_limits: BodyLimits,
    pub auto_fetch_limit: u64,
    pub free_app_http_memory: bool,
    /// One body fetch gives up after this long.
    pub body_fetch_timeout: Duration,
    /// The whole connect sequence, up to the start of the run, gives up after this long.
    pub connect_deadline: Duration,
    /// The clock calibration's call into the app waits this long before the fallback.
    pub calibration_timeout: Duration,
    pub stdout_flush: Duration,
    pub string_fetch_timeout: Duration,
    pub reconnect_uri_retry: Duration,
    pub reconnect_uri_give_up: Duration,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        CaptureConfig {
            publish_interval: Duration::from_millis(16),
            http_poll: Duration::from_millis(500),
            socket_poll: Duration::from_secs(2),
            timeline_poll: Duration::from_millis(500),
            timeline_call_timeout: Duration::from_secs(5),
            refresh_timeout: Duration::from_secs(1),
            body_limits: BodyLimits::default(),
            auto_fetch_limit: 4 << 20,
            free_app_http_memory: false,
            body_fetch_timeout: Duration::from_secs(10),
            connect_deadline: Duration::from_secs(5),
            calibration_timeout: Duration::from_millis(1500),
            stdout_flush: Duration::from_millis(100),
            string_fetch_timeout: Duration::from_secs(2),
            reconnect_uri_retry: Duration::from_secs(1),
            reconnect_uri_give_up: Duration::from_secs(30),
        }
    }
}
