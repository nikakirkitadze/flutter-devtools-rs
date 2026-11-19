//! The `dart:io` HTTP and socket profiling extensions.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::value::RawValue;
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
    /// Set instead of the body when it was longer than the decoder's cap: its real length.
    pub request_too_large: Option<u64>,
    pub response_too_large: Option<u64>,
}

/// One body as read from the reply text.
enum Side {
    Bytes(Vec<u8>),
    Over(u64),
}

/// Reads a JSON array of integers into bytes, keeping at most `cap` of them. Past the cap it
/// only counts, so the real length is known without the body ever being held.
struct CappedBytes {
    cap: usize,
}

impl<'de> serde::de::DeserializeSeed<'de> for CappedBytes {
    type Value = Option<Side>;
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for CappedBytes {
    type Value = Option<Side>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an array of bytes or null")
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        use serde::de::Error;
        let mut bytes = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(self.cap).min(1 << 20));
        let mut total = 0u64;
        while let Some(n) = seq
            .next_element::<u8>()
            .map_err(|_| A::Error::custom("not a byte"))?
        {
            total += 1;
            if total > self.cap as u64 {
                // Over the cap: drop what was kept and only count the rest.
                bytes = Vec::new();
            } else {
                bytes.push(n);
            }
        }
        Ok(Some(if total > self.cap as u64 {
            Side::Over(total)
        } else {
            Side::Bytes(bytes)
        }))
    }
}

/// Only the two body fields of a reply, each read under the cap; everything else is skipped.
struct Bodies {
    request: Option<Side>,
    response: Option<Side>,
}

struct BodiesSeed {
    cap: usize,
}

impl<'de> serde::de::DeserializeSeed<'de> for BodiesSeed {
    type Value = Bodies;
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Bodies, D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de> serde::de::Visitor<'de> for BodiesSeed {
    type Value = Bodies;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an object")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Bodies, A::Error> {
        let mut out = Bodies {
            request: None,
            response: None,
        };
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            match &*key {
                "requestBody" => {
                    out.request = map.next_value_seed(CappedBytes { cap: self.cap })?
                }
                "responseBody" => {
                    out.response = map.next_value_seed(CappedBytes { cap: self.cap })?
                }
                _ => {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
        }
        Ok(out)
    }
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
            request_too_large: None,
            response_too_large: None,
        })
    }

    /// Decodes a reply from its JSON text without building a `Value` tree. Each body longer
    /// than `max_body_bytes` is not kept: its length is recorded in `*_too_large` instead.
    pub fn from_raw(raw: &RawValue, max_body_bytes: usize) -> Result<Self, RpcError> {
        use serde::de::DeserializeSeed;
        let decode_err = |e: serde_json::Error| RpcError::Decode(e.to_string());
        let entry: HttpProfileEntry = serde_json::from_str(raw.get()).map_err(decode_err)?;
        let mut de = serde_json::Deserializer::from_str(raw.get());
        let bodies = BodiesSeed {
            cap: max_body_bytes,
        }
        .deserialize(&mut de)
        .map_err(decode_err)?;
        let split = |side: Option<Side>| match side {
            Some(Side::Bytes(b)) => (Some(b), None),
            Some(Side::Over(n)) => (None, Some(n)),
            None => (None, None),
        };
        let (request_body, request_too_large) = split(bodies.request);
        let (response_body, response_too_large) = split(bodies.response);
        Ok(HttpProfileRequestFull {
            entry,
            request_body,
            response_body,
            request_too_large,
            response_too_large,
        })
    }

    /// The request body, or [`RpcError::BodyTooLarge`] when it was over the cap.
    pub fn take_request_body(&mut self) -> Result<Option<Vec<u8>>, RpcError> {
        match self.request_too_large {
            Some(len) => Err(RpcError::BodyTooLarge { len }),
            None => Ok(self.request_body.take()),
        }
    }

    /// The response body, or [`RpcError::BodyTooLarge`] when it was over the cap.
    pub fn take_response_body(&mut self) -> Result<Option<Vec<u8>>, RpcError> {
        match self.response_too_large {
            Some(len) => Err(RpcError::BodyTooLarge { len }),
            None => Ok(self.response_body.take()),
        }
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
        self.get_http_profile_with_timeout(isolate, updated_since, self.client().call_timeout())
            .await
    }

    /// Like [`get_http_profile`](Self::get_http_profile), giving up after `timeout`.
    pub async fn get_http_profile_with_timeout(
        &self,
        isolate: &str,
        updated_since: Option<i64>,
        timeout: std::time::Duration,
    ) -> Result<HttpProfile, RpcError> {
        let params = match updated_since {
            Some(t) => json!({"updatedSince": t}),
            None => json!({}),
        };
        let mut params = match params {
            Value::Object(m) => m,
            _ => Default::default(),
        };
        params.insert("isolateId".into(), json!(isolate));
        decode(
            self.client()
                .call_with_timeout("ext.dart.io.getHttpProfile", Value::Object(params), timeout)
                .await?,
        )
    }

    /// Fetches one request with its bodies. A body longer than `max_body_bytes` is reported in
    /// the `*_too_large` fields, not returned.
    pub async fn get_http_profile_request(
        &self,
        isolate: &str,
        id: &str,
        max_body_bytes: usize,
    ) -> Result<HttpProfileRequestFull, RpcError> {
        let raw = self
            .client()
            .call_raw_with_timeout(
                "ext.dart.io.getHttpProfileRequest",
                json!({"isolateId": isolate, "id": id}),
                self.client().call_timeout(),
            )
            .await?;
        HttpProfileRequestFull::from_raw(&raw, max_body_bytes)
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

    fn reply(req: &[u8], resp: &[u8]) -> Box<RawValue> {
        let v = json!({"id": "7", "isolateId": "isolates/1", "method": "GET",
            "uri": "http://x/", "startTime": 1,
            "requestBody": req, "responseBody": resp});
        RawValue::from_string(v.to_string()).unwrap()
    }

    #[test]
    fn raw_replies_decode_both_bodies() {
        let raw = reply(&[1, 2], &[3, 4, 5]);
        let full = HttpProfileRequestFull::from_raw(&raw, 100).unwrap();
        assert_eq!(full.entry.id, "7");
        assert_eq!(full.request_body, Some(vec![1, 2]));
        assert_eq!(full.response_body, Some(vec![3, 4, 5]));
        assert_eq!(full.request_too_large, None);
    }

    #[test]
    fn a_body_over_the_cap_is_an_error_with_its_real_length() {
        let raw = reply(&[1, 2], &[0; 50]);
        let mut full = HttpProfileRequestFull::from_raw(&raw, 10).unwrap();
        assert_eq!(full.response_body, None);
        assert_eq!(full.response_too_large, Some(50));
        assert_eq!(
            full.take_response_body(),
            Err(RpcError::BodyTooLarge { len: 50 })
        );
        // The other side is unaffected.
        assert_eq!(full.take_request_body(), Ok(Some(vec![1, 2])));
    }

    #[test]
    fn a_five_megabyte_body_decodes_from_text() {
        let body: Vec<u8> = (0..5 << 20).map(|i| (i % 251) as u8).collect();
        let raw = reply(&[], &body);
        let full = HttpProfileRequestFull::from_raw(&raw, 8 << 20).unwrap();
        assert_eq!(full.response_body.as_deref(), Some(&body[..]));
    }

    #[test]
    fn raw_replies_reject_bad_bytes() {
        let raw = RawValue::from_string(
            r#"{"id":"1","method":"GET","uri":"u","startTime":1,"responseBody":[1,300]}"#.into(),
        )
        .unwrap();
        assert!(HttpProfileRequestFull::from_raw(&raw, 100).is_err());
    }
}
