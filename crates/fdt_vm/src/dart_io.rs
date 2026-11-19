//! The `dart:io` HTTP and socket profiling extensions.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::rpc::RpcError;
use crate::service::{VmService, decode};

/// An `updatedSince` far in the future: the reply has no requests but carries `timestamp`
/// (epoch microseconds), which pairs with `getVMTimelineMicros` to calibrate the two clocks.
pub const HTTP_CALIBRATION_SINCE: i64 = 9_000_000_000_000_000;

pub type Headers = BTreeMap<String, Vec<String>>;

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionInfo {
    pub local_port: Option<u32>,
    pub remote_address: Option<String>,
    pub remote_port: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpEvent {
    pub timestamp: i64,
    pub event: String,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRequestData {
    pub headers: Option<Headers>,
    pub connection_info: Option<ConnectionInfo>,
    pub content_length: Option<i64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpResponseData {
    #[serde(default)]
    pub start_time: i64,
    pub end_time: Option<i64>,
    pub status_code: Option<u16>,
    pub reason_phrase: Option<String>,
    pub headers: Option<Headers>,
    pub content_length: Option<i64>,
    pub error: Option<String>,
    pub connection_info: Option<ConnectionInfo>,
}

/// One request. Times are epoch microseconds; `id` is a decimal string that can be negative.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpProfileEntry {
    pub id: String,
    #[serde(default)]
    pub isolate_id: String,
    pub method: String,
    pub uri: String,
    pub start_time: i64,
    pub end_time: Option<i64>,
    #[serde(default)]
    pub events: Vec<HttpEvent>,
    pub request: Option<HttpRequestData>,
    pub response: Option<HttpResponseData>,
}

impl HttpProfileEntry {
    /// Complete once the response finished, or either side reported an error.
    pub fn is_complete(&self) -> bool {
        self.response.as_ref().is_some_and(|r| r.end_time.is_some())
            || self.request.as_ref().is_some_and(|r| r.error.is_some())
            || self.response.as_ref().is_some_and(|r| r.error.is_some())
    }

    pub fn is_websocket_upgrade(&self) -> bool {
        let upgrade_header = self
            .request
            .as_ref()
            .and_then(|r| r.headers.as_ref())
            .is_some_and(|h| {
                h.iter().any(|(k, v)| {
                    k.eq_ignore_ascii_case("upgrade")
                        && v.iter().any(|x| x.eq_ignore_ascii_case("websocket"))
                })
            });
        upgrade_header && self.response.as_ref().and_then(|r| r.status_code) == Some(101)
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct HttpProfile {
    pub timestamp: i64,
    #[serde(default)]
    pub requests: Vec<HttpProfileEntry>,
}

/// An entry plus its bodies, from `getHttpProfileRequest`.
#[derive(Clone, Debug, PartialEq)]
pub struct HttpProfileRequestFull {
    pub entry: HttpProfileEntry,
    pub request_body: Option<Vec<u8>>,
    pub response_body: Option<Vec<u8>>,
}

fn take_bytes(
    obj: &mut serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<Vec<u8>>, String> {
    match obj.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .into_iter()
            .map(|v| {
                v.as_u64()
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or_else(|| format!("{key}: not a byte"))
            })
            .collect::<Result<Vec<u8>, _>>()
            .map(Some),
        Some(_) => Err(format!("{key}: not an array")),
    }
}

impl HttpProfileRequestFull {
    /// Bodies are JSON arrays of integers; they are moved out of the tree without cloning.
    pub fn from_value(v: Value) -> Result<Self, RpcError> {
        let Value::Object(mut obj) = v else {
            return Err(RpcError::Decode(
                "getHttpProfileRequest: not an object".into(),
            ));
        };
        let request_body = take_bytes(&mut obj, "requestBody").map_err(RpcError::Decode)?;
        let response_body = take_bytes(&mut obj, "responseBody").map_err(RpcError::Decode)?;
        Ok(HttpProfileRequestFull {
            entry: decode(Value::Object(obj))?,
            request_body,
            response_body,
        })
    }
}

/// Times are on the timeline clock.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocketStat {
    pub id: String,
    pub address: String,
    pub port: u32,
    pub socket_type: String,
    pub start_time: i64,
    pub end_time: Option<i64>,
    #[serde(default)]
    pub read_bytes: i64,
    #[serde(default)]
    pub write_bytes: i64,
    pub last_read_time: Option<i64>,
    pub last_write_time: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct SocketProfile {
    #[serde(default)]
    pub sockets: Vec<SocketStat>,
}

impl VmService {
    /// Turns HTTP profiling on or off (off by default, and off again after a hot restart).
    /// Returns the state the VM reports.
    pub async fn http_enable(&self, isolate: &str, enabled: bool) -> Result<bool, RpcError> {
        let v = self
            .call_extension(
                "ext.dart.io.httpEnableTimelineLogging",
                isolate,
                json!({"enabled": enabled}),
            )
            .await?;
        Ok(v["enabled"].as_bool().unwrap_or(enabled))
    }

    pub async fn get_http_profile(
        &self,
        isolate: &str,
        updated_since: Option<i64>,
    ) -> Result<HttpProfile, RpcError> {
        let params = match updated_since {
            Some(t) => json!({"updatedSince": t}),
            None => json!({}),
        };
        decode(
            self.call_extension("ext.dart.io.getHttpProfile", isolate, params)
                .await?,
        )
    }

    pub async fn get_http_profile_request(
        &self,
        isolate: &str,
        id: &str,
    ) -> Result<HttpProfileRequestFull, RpcError> {
        HttpProfileRequestFull::from_value(
            self.call_extension(
                "ext.dart.io.getHttpProfileRequest",
                isolate,
                json!({"id": id}),
            )
            .await?,
        )
    }

    pub async fn clear_http_profile(&self, isolate: &str) -> Result<(), RpcError> {
        self.call_extension("ext.dart.io.clearHttpProfile", isolate, json!({}))
            .await
            .map(|_| ())
    }

    pub async fn socket_profiling_enable(
        &self,
        isolate: &str,
        enabled: bool,
    ) -> Result<(), RpcError> {
        self.call_extension(
            "ext.dart.io.socketProfilingEnabled",
            isolate,
            json!({"enabled": enabled}),
        )
        .await
        .map(|_| ())
    }

    pub async fn get_socket_profile(&self, isolate: &str) -> Result<Vec<SocketStat>, RpcError> {
        let p: SocketProfile = decode(
            self.call_extension("ext.dart.io.getSocketProfile", isolate, json!({}))
                .await?,
        )?;
        Ok(p.sockets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn take(v: Value) -> Result<Option<Vec<u8>>, String> {
        let mut m = serde_json::Map::new();
        m.insert("b".into(), v);
        take_bytes(&mut m, "b")
    }

    #[test]
    fn byte_arrays_decode() {
        assert_eq!(take(json!([0, 1, 255])), Ok(Some(vec![0, 1, 255])));
        assert_eq!(take(Value::Null), Ok(None));
    }

    #[test]
    fn byte_arrays_reject_out_of_range_and_non_arrays() {
        assert!(take(json!([1, 256])).is_err());
        assert!(take(json!([1, -1])).is_err());
        assert!(take(json!("abc")).is_err());
    }
}
