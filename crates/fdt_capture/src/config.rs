use std::time::Duration;

use fdt_store::net::BodyLimits;

#[derive(Clone, Debug)]
pub struct CaptureConfig {
    pub publish_interval: Duration,
    pub http_poll: Duration,
    pub socket_poll: Duration,
    pub body_limits: BodyLimits,
    pub auto_fetch_limit: u64,
    pub free_app_http_memory: bool,
    /// One body fetch gives up after this long.
    pub body_fetch_timeout: Duration,
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
            body_limits: BodyLimits::default(),
            auto_fetch_limit: 4 << 20,
            free_app_http_memory: false,
            body_fetch_timeout: Duration::from_secs(10),
            stdout_flush: Duration::from_millis(100),
            string_fetch_timeout: Duration::from_secs(2),
            reconnect_uri_retry: Duration::from_secs(1),
            reconnect_uri_give_up: Duration::from_secs(30),
        }
    }
}
