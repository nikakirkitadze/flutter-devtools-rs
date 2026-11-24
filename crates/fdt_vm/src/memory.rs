//! Memory calls: usage, process RSS, the allocation profile and heap-snapshot requests.

use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::rpc::RpcError;
use crate::service::{VmService, decode};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryUsage {
    #[serde(default)]
    pub heap_usage: u64,
    #[serde(default)]
    pub heap_capacity: u64,
    #[serde(default)]
    pub external_usage: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassLocation {
    pub script_uri: String,
    pub line: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassStats {
    pub class_id: String,
    pub name: String,
    /// `None` when the VM sent a Null `@Instance` instead of a library (profile mode).
    pub library_uri: Option<String>,
    pub instances: u64,
    pub bytes: u64,
    pub location: Option<ClassLocation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocationProfile {
    pub memory: MemoryUsage,
    /// Only classes with live instances.
    pub members: Vec<ClassStats>,
}

#[derive(Deserialize)]
struct RawProfile {
    #[serde(default, rename = "memoryUsage")]
    memory_usage: MemoryUsage,
    #[serde(default)]
    members: Vec<RawMember>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMember {
    #[serde(default)]
    instances_current: u64,
    #[serde(default)]
    bytes_current: u64,
    class: Option<RawClass>,
}

#[derive(Deserialize)]
struct RawClass {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    library: Option<UriHolder>,
    location: Option<RawLocation>,
}

/// A library has a `uri`; a Null `@Instance` has none.
#[derive(Deserialize)]
struct UriHolder {
    uri: Option<String>,
}

#[derive(Deserialize)]
struct RawLocation {
    script: Option<UriHolder>,
    line: Option<u32>,
}

fn decode_profile(raw: &str) -> Result<AllocationProfile, RpcError> {
    let p: RawProfile = serde_json::from_str(raw)
        .map_err(|e| RpcError::Decode(format!("allocation profile: {e}")))?;
    let members = p
        .members
        .into_iter()
        .filter(|m| m.instances_current > 0)
        .filter_map(|m| {
            let c = m.class?;
            Some(ClassStats {
                class_id: c.id,
                name: c.name,
                library_uri: c.library.and_then(|l| l.uri),
                instances: m.instances_current,
                bytes: m.bytes_current,
                location: c.location.and_then(|l| {
                    Some(ClassLocation {
                        script_uri: l.script?.uri?,
                        line: l.line,
                    })
                }),
            })
        })
        .collect();
    Ok(AllocationProfile {
        memory: p.memory_usage,
        members,
    })
}

impl VmService {
    /// Heap usage of an isolate group (0.2 ms on the app; safe to poll).
    pub async fn get_isolate_group_memory_usage(
        &self,
        group: &str,
    ) -> Result<MemoryUsage, RpcError> {
        decode(
            self.client
                .call(
                    "getIsolateGroupMemoryUsage",
                    json!({"isolateGroupId": group}),
                )
                .await?,
        )
    }

    /// The process's resident set size in bytes.
    pub async fn get_process_rss(&self) -> Result<u64, RpcError> {
        let v = self.client.call("getProcessMemoryUsage", json!({})).await?;
        v["root"]["size"]
            .as_u64()
            .ok_or_else(|| RpcError::Decode("getProcessMemoryUsage: no root.size".into()))
    }

    /// Per-class live counts and bytes. With `gc`, the VM collects first, and every client sees
    /// that GC. Never sends `reset`.
    pub async fn get_allocation_profile(
        &self,
        isolate: &str,
        gc: bool,
        timeout: Duration,
    ) -> Result<AllocationProfile, RpcError> {
        let mut params = json!({"isolateId": isolate});
        if gc {
            // The VM accepts only the JSON value `true`.
            params["gc"] = json!(true);
        }
        let raw = self
            .client
            .call_raw_with_timeout("getAllocationProfile", params, timeout)
            .await?;
        tokio::task::spawn_blocking(move || decode_profile(raw.get()))
            .await
            .map_err(|e| RpcError::Decode(format!("allocation profile: {e}")))?
    }

    /// Asks the VM to write a heap snapshot. The chunks arrive as binary frames (see
    /// [`RpcClient::heap_chunks`](crate::rpc::RpcClient::heap_chunks)) on the `HeapSnapshot`
    /// stream, which the caller subscribes to first; the reply comes after the last one.
    pub async fn request_heap_snapshot(
        &self,
        isolate: &str,
        timeout: Duration,
    ) -> Result<(), RpcError> {
        self.client
            .call_with_timeout(
                "requestHeapSnapshot",
                json!({"isolateId": isolate}),
                timeout,
            )
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap_snapshot::encode_frame;
    use crate::rpc::{ConnectOptions, connect};
    use crate::testing::ScriptedServer;
    use serde_json::Value;

    const T: Duration = Duration::from_secs(5);

    async fn service(s: &ScriptedServer) -> VmService {
        let (c, _rx) = connect(&s.ws_uri(), ConnectOptions::default())
            .await
            .unwrap();
        VmService::new(c)
    }

    #[tokio::test]
    async fn memory_usage_and_rss_decode() {
        let s = ScriptedServer::start(|method, params| match method {
            "getIsolateGroupMemoryUsage" => {
                assert_eq!(params["isolateGroupId"], "isolateGroups/1");
                Ok(json!({"type":"MemoryUsage","heapUsage":11184544,
                          "heapCapacity":12075008,"externalUsage":2624}))
            }
            "getProcessMemoryUsage" => Ok(json!({"type":"ProcessMemoryUsage","root":{
                "name":"Process 1","description":"Resident set size","size":115376128,
                "children":[{"name":"Dart VM","size":1}]}})),
            _ => Err((-32601, "nope".into())),
        });
        let vm = service(&s).await;
        let m = vm
            .get_isolate_group_memory_usage("isolateGroups/1")
            .await
            .unwrap();
        assert_eq!(
            m,
            MemoryUsage {
                heap_usage: 11184544,
                heap_capacity: 12075008,
                external_usage: 2624
            }
        );
        assert_eq!(vm.get_process_rss().await.unwrap(), 115376128);
    }

    fn profile_json() -> Value {
        json!({"type":"AllocationProfile",
        "dateLastAccumulatorReset":"1795272000000","dateLastServiceGC":"1795272100000",
        "memoryUsage":{"type":"MemoryUsage","heapUsage":100,"heapCapacity":200,"externalUsage":3},
        "members":[
          {"type":"ClassHeapStats","instancesCurrent":1,"bytesCurrent":64,
           "instancesAccumulated":1,"accumulatedSize":64,
           "class":{"type":"@Class","id":"classes/2304","name":"_LeakyScreenState",
                    "library":{"type":"@Library","uri":"package:jank_lab/scenarios/leaks.dart"},
                    "location":{"type":"SourceLocation",
                                "script":{"type":"@Script","uri":"package:jank_lab/scenarios/leaks.dart"},
                                "tokenPos":10,"line":42}}},
          {"type":"ClassHeapStats","instancesCurrent":5,"bytesCurrent":80,
           "class":{"type":"@Class","id":"classes/7","name":"_Smi",
                    "library":{"type":"@Instance","kind":"Null","id":"objects/null"}}},
          {"type":"ClassHeapStats","instancesCurrent":0,"bytesCurrent":0,
           "class":{"type":"@Class","id":"classes/9","name":"Gone"}}
        ]})
    }

    #[test]
    fn allocation_profile_decodes() {
        let p = decode_profile(&profile_json().to_string()).unwrap();
        assert_eq!(p.memory.heap_capacity, 200);
        assert_eq!(p.members.len(), 2, "zero-instance members are dropped");
        let a = &p.members[0];
        assert_eq!(a.class_id, "classes/2304");
        assert_eq!(a.name, "_LeakyScreenState");
        assert_eq!(
            a.library_uri.as_deref(),
            Some("package:jank_lab/scenarios/leaks.dart")
        );
        assert_eq!((a.instances, a.bytes), (1, 64));
        assert_eq!(
            a.location,
            Some(ClassLocation {
                script_uri: "package:jank_lab/scenarios/leaks.dart".into(),
                line: Some(42)
            })
        );
        let b = &p.members[1];
        assert_eq!(b.library_uri, None, "a Null @Instance library");
        assert_eq!(b.location, None);
    }

    #[tokio::test]
    async fn allocation_profile_params() {
        let s = ScriptedServer::start(|_, _| Ok(profile_json()));
        let vm = service(&s).await;
        vm.get_allocation_profile("isolates/1", false, T)
            .await
            .unwrap();
        vm.get_allocation_profile("isolates/1", true, T)
            .await
            .unwrap();
        let got = s.received();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].1, json!({"isolateId": "isolates/1"}));
        assert_eq!(got[1].1, json!({"isolateId": "isolates/1", "gc": true}));
        assert!(got.iter().all(|(_, p)| p.get("reset").is_none()));
    }

    #[tokio::test]
    async fn heap_snapshot_success_arrives_after_its_chunks() {
        let s = ScriptedServer::start(|_, _| Ok(json!({"type": "Success"})));
        s.script_binary(
            "requestHeapSnapshot",
            vec![
                encode_frame("isolates/1", 1, false, b"aaa"),
                encode_frame("isolates/1", 2, false, b"bbb"),
                encode_frame("isolates/1", 3, true, b"cc"),
            ],
        );
        let (c, _notes) = connect(&s.ws_uri(), ConnectOptions::default())
            .await
            .unwrap();
        let mut rx = c.heap_chunks();
        VmService::new(c)
            .request_heap_snapshot("isolates/1", T)
            .await
            .unwrap();
        // The reply came after the frames, so all three are already queued.
        let mut got = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            got.push(chunk);
        }
        assert_eq!(got.len(), 3);
        assert!(got[2].last);
    }
}
