//! Flutter's profiling service extensions and the display refresh rate.

use serde_json::{Value, json};

use crate::rpc::RpcError;
use crate::service::VmService;

pub const PROFILE_WIDGET_BUILDS: &str = "ext.flutter.profileWidgetBuilds";
pub const PROFILE_USER_WIDGET_BUILDS: &str = "ext.flutter.profileUserWidgetBuilds";
pub const PROFILE_RENDER_OBJECT_LAYOUTS: &str = "ext.flutter.profileRenderObjectLayouts";
pub const PROFILE_RENDER_OBJECT_PAINTS: &str = "ext.flutter.profileRenderObjectPaints";

/// `enabled` comes back as the string `"true"`/`"false"`, though a bool is accepted too.
fn read_enabled(v: &Value) -> Result<bool, RpcError> {
    match v.get("enabled") {
        Some(Value::Bool(b)) => Ok(*b),
        Some(Value::String(s)) if s == "true" => Ok(true),
        Some(Value::String(s)) if s == "false" => Ok(false),
        other => Err(RpcError::Decode(format!(
            "expected `enabled` as a bool, got {other:?}"
        ))),
    }
}

impl VmService {
    /// Reads a bool service extension (call with only `isolateId`).
    pub async fn get_bool_extension(&self, method: &str, isolate: &str) -> Result<bool, RpcError> {
        read_enabled(&self.call_extension(method, isolate, json!({})).await?)
    }

    /// Sets a bool service extension; `enabled` is sent as a string. Returns the value read back.
    pub async fn set_bool_extension(
        &self,
        method: &str,
        isolate: &str,
        on: bool,
    ) -> Result<bool, RpcError> {
        let enabled = if on { "true" } else { "false" };
        read_enabled(
            &self
                .call_extension(method, isolate, json!({"enabled": enabled}))
                .await?,
        )
    }

    /// The display refresh rate in Hz, or `None` when there is no Flutter view.
    ///
    /// `_flutter.getDisplayRefreshRate` aborts the app when given a `viewId` it did not list, so
    /// the id comes verbatim from `_flutter.listViews` and nothing else is ever sent.
    pub async fn display_refresh_rate(&self, isolate: &str) -> Result<Option<f32>, RpcError> {
        let views = self
            .call_extension("_flutter.listViews", isolate, json!({}))
            .await?;
        let Some(id) = views
            .get("views")
            .and_then(Value::as_array)
            .and_then(|vs| {
                vs.iter()
                    .find(|v| v.get("type").and_then(Value::as_str) == Some("FlutterView"))
            })
            .and_then(|v| v.get("id"))
            .and_then(Value::as_str)
        else {
            return Ok(None);
        };
        let r = self
            .client
            .call("_flutter.getDisplayRefreshRate", json!({"viewId": id}))
            .await?;
        let fps = r
            .get("fps")
            .and_then(Value::as_f64)
            .ok_or_else(|| RpcError::Decode("getDisplayRefreshRate: no fps".into()))?;
        Ok(Some(if fps == 0.0 { 60.0 } else { fps as f32 }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{ConnectOptions, connect};
    use crate::testing::ScriptedServer;

    async fn svc(server: &ScriptedServer) -> VmService {
        let (client, _n) = connect(&server.ws_uri(), ConnectOptions::default())
            .await
            .unwrap();
        VmService::new(client)
    }

    #[tokio::test]
    async fn refresh_rate_passes_only_the_listed_view_id() {
        let server = ScriptedServer::start(|m, p| match m {
            "_flutter.listViews" => Ok(json!({"type":"FlutterViewList","views":[
                {"id":"_flutterView/0xdead","type":"Other"},
                {"id":"_flutterView/0xc50c95420","type":"FlutterView"},
                {"id":"_flutterView/0x2","type":"FlutterView"}]})),
            "_flutter.getDisplayRefreshRate" => {
                assert_eq!(p["viewId"], "_flutterView/0xc50c95420");
                Ok(json!({"type":"DisplayRefreshRate","fps":120.0}))
            }
            _ => Err((-32601, "nope".into())),
        });
        let s = svc(&server).await;
        assert_eq!(
            s.display_refresh_rate("isolates/1").await.unwrap(),
            Some(120.0)
        );
        let got = server.received();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "_flutter.listViews");
        assert_eq!(got[0].1["isolateId"], "isolates/1");
        assert_eq!(got[1].0, "_flutter.getDisplayRefreshRate");
        assert_eq!(got[1].1["viewId"], "_flutterView/0xc50c95420");
    }

    #[tokio::test]
    async fn refresh_rate_zero_is_sixty_and_no_view_sends_nothing_else() {
        let server = ScriptedServer::start(|m, _| match m {
            "_flutter.listViews" => Ok(json!({"type":"FlutterViewList","views":[]})),
            _ => Err((-32601, "nope".into())),
        });
        let s = svc(&server).await;
        assert_eq!(s.display_refresh_rate("isolates/1").await.unwrap(), None);
        assert!(
            server
                .received()
                .iter()
                .all(|(m, _)| m != "_flutter.getDisplayRefreshRate")
        );

        let server = ScriptedServer::start(|m, _| match m {
            "_flutter.listViews" => Ok(json!({"views":[{"id":"v","type":"FlutterView"}]})),
            _ => Ok(json!({"fps":0.0})),
        });
        let s = svc(&server).await;
        assert_eq!(s.display_refresh_rate("i").await.unwrap(), Some(60.0));
    }

    #[tokio::test]
    async fn bool_extension_sends_strings() {
        let server = ScriptedServer::start(|m, p| match p.get("enabled") {
            Some(e) => Ok(json!({"type":"_extensionType","method":m,"enabled": e})),
            None => Ok(json!({"type":"_extensionType","method":m,"enabled":false})),
        });
        let s = svc(&server).await;
        assert!(
            s.set_bool_extension(PROFILE_WIDGET_BUILDS, "isolates/1", true)
                .await
                .unwrap()
        );
        assert!(
            !s.set_bool_extension(PROFILE_WIDGET_BUILDS, "isolates/1", false)
                .await
                .unwrap()
        );
        assert!(
            !s.get_bool_extension(PROFILE_WIDGET_BUILDS, "isolates/1")
                .await
                .unwrap()
        );
        let got = server.received();
        assert_eq!(got[0].0, PROFILE_WIDGET_BUILDS);
        assert_eq!(got[0].1["enabled"], json!("true"));
        assert_eq!(got[0].1["isolateId"], "isolates/1");
        assert_eq!(got[1].1["enabled"], json!("false"));
        assert!(got[2].1.get("enabled").is_none());
    }

    #[test]
    fn enabled_accepts_bool_or_string_and_rejects_the_rest() {
        assert!(read_enabled(&json!({"enabled": true})).unwrap());
        assert!(!read_enabled(&json!({"enabled": "false"})).unwrap());
        assert!(read_enabled(&json!({"enabled": "yes"})).is_err());
        assert!(read_enabled(&json!({})).is_err());
    }
}
