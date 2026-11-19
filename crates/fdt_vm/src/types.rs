//! The subset of VM service objects `fdt` reads. Unknown fields are ignored; fields the VM
//! sends only sometimes are `Option`.

use serde::Deserialize;

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Vm {
    pub pid: Option<u32>,
    /// Epoch milliseconds.
    pub start_time: Option<i64>,
    pub operating_system: String,
    #[serde(rename = "targetCPU")]
    pub target_cpu: Option<String>,
    #[serde(default)]
    pub isolates: Vec<IsolateRef>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IsolateRef {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub is_system_isolate: bool,
    pub isolate_group_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibRef {
    pub uri: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Isolate {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "extensionRPCs")]
    pub extension_rpcs: Vec<String>,
    #[serde(default, rename = "rootLib")]
    pub root_lib: Option<LibRef>,
}

#[derive(Deserialize)]
struct ClassRef {
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawInstanceRef {
    id: Option<String>,
    #[serde(default)]
    kind: String,
    value_as_string: Option<String>,
    #[serde(default)]
    value_as_string_is_truncated: bool,
    length: Option<u64>,
    class: Option<ClassRef>,
}

/// A reference to a Dart instance, as it appears in `Logging` events and `getObject` results.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(from = "RawInstanceRef")]
pub struct InstanceRef {
    pub id: Option<String>,
    pub kind: String,
    pub value_as_string: Option<String>,
    /// The VM leaves the flag out when nothing was cut.
    pub value_as_string_is_truncated: bool,
    pub length: Option<u64>,
    pub class_name: Option<String>,
}

impl From<RawInstanceRef> for InstanceRef {
    fn from(r: RawInstanceRef) -> Self {
        InstanceRef {
            id: r.id,
            kind: r.kind,
            value_as_string: r.value_as_string,
            value_as_string_is_truncated: r.value_as_string_is_truncated,
            length: r.length,
            class_name: r.class.and_then(|c| c.name),
        }
    }
}

impl Default for InstanceRef {
    /// A `Null` instance.
    fn default() -> Self {
        InstanceRef {
            id: None,
            kind: "Null".into(),
            value_as_string: None,
            value_as_string_is_truncated: false,
            length: None,
            class_name: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlutterVersion {
    pub flutter_version: Option<String>,
    pub framework_version: Option<String>,
    pub dart_sdk_version: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Flag {
    pub name: String,
    pub value_as_string: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct FlagList {
    #[serde(default)]
    pub flags: Vec<Flag>,
}
