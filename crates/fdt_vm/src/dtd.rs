//! A client for the Dart Tooling Daemon (DTD) that each `flutter run` starts.

use serde_json::json;

use crate::rpc::{ConnectError, ConnectOptions, RpcClient, RpcError, connect};

/// One VM service a DTD knows about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmServiceEntry {
    pub uri: String,
    pub name: Option<String>,
    pub exposed_uri: Option<String>,
}

/// Connection to one DTD. Cheap to clone; the socket closes when the last clone drops.
#[derive(Clone, Debug)]
pub struct DtdClient {
    client: RpcClient,
}

impl DtdClient {
    pub async fn connect(ws: &url::Url) -> Result<DtdClient, ConnectError> {
        // DTD notifications are not used, so the receiver is dropped.
        let (client, _notes) = connect(ws, ConnectOptions::default()).await?;
        Ok(DtdClient { client })
    }

    /// `ConnectedApp.getVmServices` needs no secret.
    pub async fn get_vm_services(&self) -> Result<Vec<VmServiceEntry>, RpcError> {
        let v = self
            .client
            .call("ConnectedApp.getVmServices", json!({}))
            .await?;
        let list = v
            .get("vmServices")
            .and_then(|l| l.as_array())
            .ok_or_else(|| RpcError::Decode("getVmServices: no vmServices".into()))?;
        let s =
            |e: &serde_json::Value, k: &str| e.get(k).and_then(|x| x.as_str()).map(str::to_string);
        Ok(list
            .iter()
            .filter_map(|e| {
                Some(VmServiceEntry {
                    uri: s(e, "uri")?,
                    name: s(e, "name"),
                    exposed_uri: s(e, "exposedUri"),
                })
            })
            .collect())
    }

    /// Resolves when the connection closes.
    pub async fn closed(&self) {
        self.client.closed().await;
    }

    pub fn is_closed(&self) -> bool {
        self.client.is_closed()
    }
}

/// The parts of a DTD app name like `Kind: Flutter - Device: macOS - Package: jank_lab`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppLabel {
    pub kind: Option<String>,
    pub device: Option<String>,
    pub package: Option<String>,
}

/// Tolerates missing or reordered parts. A part starts at `Kind: `, `Device: ` or `Package: `
/// at the beginning or after ` - `, and runs to the next part, so values may contain ` - `.
pub fn parse_app_label(s: &str) -> AppLabel {
    const KEYS: [&str; 3] = ["Kind: ", "Device: ", "Package: "];
    let mut marks: Vec<(usize, usize, &str)> = Vec::new(); // (start of marker, value start, key)
    for key in KEYS {
        let mut from = 0;
        while let Some(i) = s[from..].find(key) {
            let at = from + i;
            if at == 0 || s[..at].ends_with(" - ") {
                marks.push((at, at + key.len(), key));
                break;
            }
            from = at + key.len();
        }
    }
    marks.sort();
    let mut label = AppLabel::default();
    for (n, (_, value_start, key)) in marks.iter().enumerate() {
        let end = marks
            .get(n + 1)
            .map(|(next, _, _)| s[..*next].strip_suffix(" - ").map_or(*next, str::len))
            .unwrap_or(s.len());
        let v = s[*value_start..end.max(*value_start)].trim();
        let v = (!v.is_empty()).then(|| v.to_string());
        match *key {
            "Kind: " => label.kind = v,
            "Device: " => label.device = v,
            _ => label.package = v,
        }
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_app_label_cases() {
        let l = parse_app_label("Kind: Flutter - Device: macOS - Package: jank_lab");
        assert_eq!(l.kind.as_deref(), Some("Flutter"));
        assert_eq!(l.device.as_deref(), Some("macOS"));
        assert_eq!(l.package.as_deref(), Some("jank_lab"));
        let l = parse_app_label("Package: a_b - Kind: Dart");
        assert_eq!(l.package.as_deref(), Some("a_b"));
        assert_eq!(l.kind.as_deref(), Some("Dart"));
        assert_eq!(l.device, None);
        assert_eq!(parse_app_label(""), AppLabel::default());
        assert_eq!(parse_app_label("garbage"), AppLabel::default());
        // A device name may itself contain " - " or colons.
        let l = parse_app_label("Kind: Flutter - Device: Pixel 7 - API 34 - Package: p");
        assert_eq!(l.device.as_deref(), Some("Pixel 7 - API 34"));
        assert_eq!(l.package.as_deref(), Some("p"));
    }
}
