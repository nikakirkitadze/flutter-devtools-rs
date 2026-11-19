//! Typed calls over an [`RpcClient`].

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::rpc::{RpcClient, RpcError};
use crate::types::{FlagList, FlutterVersion, InstanceRef, Isolate, Version, Vm};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Debug,
    Profile,
}

/// Debug exactly when `ext.flutter.reassemble` is registered on the isolate. `getVM._features`
/// says `release` in both modes, so it is not used.
pub fn detect_mode(isolate: &Isolate) -> Mode {
    if isolate
        .extension_rpcs
        .iter()
        .any(|r| r == "ext.flutter.reassemble")
    {
        Mode::Debug
    } else {
        Mode::Profile
    }
}

pub(crate) fn decode<T: DeserializeOwned>(v: Value) -> Result<T, RpcError> {
    serde_json::from_value(v).map_err(|e| RpcError::Decode(e.to_string()))
}

/// Typed access to one VM service connection. Cheap to clone.
#[derive(Clone, Debug)]
pub struct VmService {
    pub(crate) client: RpcClient,
}

impl VmService {
    pub fn new(client: RpcClient) -> VmService {
        VmService { client }
    }

    pub fn client(&self) -> &RpcClient {
        &self.client
    }

    pub async fn get_version(&self) -> Result<Version, RpcError> {
        decode(self.client.call("getVersion", json!({})).await?)
    }

    pub async fn get_vm(&self) -> Result<Vm, RpcError> {
        decode(self.client.call("getVM", json!({})).await?)
    }

    pub async fn get_isolate(&self, id: &str) -> Result<Isolate, RpcError> {
        decode(
            self.client
                .call("getIsolate", json!({"isolateId": id}))
                .await?,
        )
    }

    /// Subscribes to a stream. Error 103 ("Stream already subscribed") counts as success.
    pub async fn stream_listen(&self, stream: &str) -> Result<(), RpcError> {
        match self
            .client
            .call("streamListen", json!({"streamId": stream}))
            .await
        {
            Ok(_) | Err(RpcError::Server { code: 103, .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// The VM timeline clock in microseconds (monotonic).
    pub async fn get_vm_timeline_micros(&self) -> Result<i64, RpcError> {
        let v = self.client.call("getVMTimelineMicros", json!({})).await?;
        v["timestamp"]
            .as_i64()
            .ok_or_else(|| RpcError::Decode("getVMTimelineMicros: no timestamp".into()))
    }

    /// The full `valueAsString` of a String instance (events carry at most 128 characters).
    pub async fn get_object_string(
        &self,
        isolate: &str,
        object_id: &str,
    ) -> Result<String, RpcError> {
        let v = self
            .client
            .call(
                "getObject",
                json!({"isolateId": isolate, "objectId": object_id}),
            )
            .await?;
        let r: InstanceRef = decode(v)?;
        r.value_as_string
            .ok_or_else(|| RpcError::Decode("getObject: no valueAsString".into()))
    }

    pub async fn get_flag_list(&self) -> Result<FlagList, RpcError> {
        decode(self.client.call("getFlagList", json!({})).await?)
    }

    /// Calls a method with only `isolateId` (e.g. the registered `s1.flutterVersion`).
    pub async fn call_service(&self, method: &str, isolate: &str) -> Result<Value, RpcError> {
        self.call_extension(method, isolate, json!({})).await
    }

    /// `method` is the name the `ServiceRegistered` event gave; never assume its prefix.
    pub async fn flutter_version(
        &self,
        method: &str,
        isolate: &str,
    ) -> Result<FlutterVersion, RpcError> {
        decode(self.call_service(method, isolate).await?)
    }

    /// Calls a service extension; `isolateId` is added to `params`.
    pub async fn call_extension(
        &self,
        method: &str,
        isolate: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        let mut params = match params {
            Value::Object(m) => m,
            _ => Default::default(),
        };
        params.insert("isolateId".into(), json!(isolate));
        self.client.call(method, Value::Object(params)).await
    }
}
