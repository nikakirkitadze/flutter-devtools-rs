use std::path::PathBuf;

use fdt_vm::rpc::{ConnectOptions, RpcError, connect};
use fdt_vm::testing::ReplayServer;
use serde_json::json;

fn transcript() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/recordings/jank_lab-debug.jsonl")
}

#[tokio::test]
async fn replay_answers_get_vm_from_the_recording() {
    let server = ReplayServer::start(&transcript());
    let (client, _notes) = connect(&server.ws_uri(), ConnectOptions::default())
        .await
        .unwrap();
    let vm = client.call("getVM", json!({})).await.unwrap();
    assert_eq!(vm["operatingSystem"], "macos");
    assert!(
        vm["isolates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["name"] == "main")
    );
    // The same answer is repeated after the recorded ones run out.
    let again = client
        .call("getVM", json!({"isolateId": "ignored"}))
        .await
        .unwrap();
    assert_eq!(again["pid"], vm["pid"]);
    assert_eq!(server.received().len(), 2);
}

#[tokio::test]
async fn stream_listen_replays_that_streams_events_in_order() {
    let server = ReplayServer::start(&transcript());
    let (client, mut notes) = connect(&server.ws_uri(), ConnectOptions::default())
        .await
        .unwrap();
    client
        .call("streamListen", json!({"streamId": "Logging"}))
        .await
        .unwrap();
    let mut seqs = Vec::new();
    for _ in 0..5 {
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), notes.recv())
            .await
            .expect("notification in time")
            .expect("open");
        assert_eq!(n.method, "streamNotify");
        assert_eq!(n.params["streamId"], "Logging");
        seqs.push(
            n.params["event"]["logRecord"]["sequenceNumber"]
                .as_i64()
                .unwrap(),
        );
    }
    let mut sorted = seqs.clone();
    sorted.sort();
    assert_eq!(seqs, sorted);
    // Nothing from other streams.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), notes.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn get_object_matches_by_object_id() {
    let server = ReplayServer::start(&transcript());
    let (client, mut notes) = connect(&server.ws_uri(), ConnectOptions::default())
        .await
        .unwrap();
    client
        .call("streamListen", json!({"streamId": "Logging"}))
        .await
        .unwrap();
    let mut object_id = None;
    for _ in 0..5 {
        let n = notes.recv().await.unwrap();
        let msg = &n.params["event"]["logRecord"]["message"];
        if msg["valueAsStringIsTruncated"] == json!(true) {
            object_id = msg["id"].as_str().map(str::to_string);
        }
    }
    let object_id = object_id.expect("a truncated message was recorded");
    let obj = client
        .call(
            "getObject",
            json!({"isolateId": "x", "objectId": object_id}),
        )
        .await
        .unwrap();
    let expected = (1..=30)
        .map(|i| format!("line {i} of 30"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(expected.len(), 410);
    assert_eq!(obj["valueAsString"], expected);

    let miss = client
        .call("getObject", json!({"objectId": "objects/0/0"}))
        .await;
    assert!(matches!(miss, Err(RpcError::Server { code: -32602, .. })));
}

#[tokio::test]
async fn unknown_method_is_not_found() {
    let server = ReplayServer::start(&transcript());
    let (client, _notes) = connect(&server.ws_uri(), ConnectOptions::default())
        .await
        .unwrap();
    match client.call("noSuchMethod", json!({})).await {
        Err(RpcError::Server { code, message, .. }) => {
            assert_eq!(code, -32601);
            assert_eq!(message, "Method not found");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn keyed_methods_match_by_their_key() {
    let server = ReplayServer::start(&transcript());
    let (client, _notes) = connect(&server.ws_uri(), ConnectOptions::default())
        .await
        .unwrap();
    let ok = client
        .call(
            "ext.jank_lab.run",
            json!({"isolateId": "i", "action": "goto_logs"}),
        )
        .await
        .unwrap();
    assert_eq!(ok["ok"], true);
    // An action that was never recorded is an invalid-params error, as on the real app.
    let miss = client
        .call("ext.jank_lab.run", json!({"action": "never_recorded"}))
        .await;
    assert!(matches!(miss, Err(RpcError::Server { code: -32602, .. })));
}

#[tokio::test]
async fn replay_exhausts_to_empty_timeline() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    let lines = [
        json!({"t_us":1,"dir":"send","msg":{"jsonrpc":"2.0","id":"1","method":"getVMTimeline","params":{}}}),
        json!({"t_us":2,"dir":"recv","msg":{"jsonrpc":"2.0","id":"1","result":{"type":"Timeline","traceEvents":[{"ph":"B","name":"x","ts":5}],"timeOriginMicros":5,"timeExtentMicros":9}}}),
        json!({"t_us":3,"dir":"send","msg":{"jsonrpc":"2.0","id":"2","method":"getPerfettoCpuSamples","params":{}}}),
        json!({"t_us":4,"dir":"recv","msg":{"jsonrpc":"2.0","id":"2","result":{"type":"PerfettoCpuSamples","sampleCount":3,"samples":"AAAA","timeOriginMicros":1,"timeExtentMicros":2}}}),
        json!({"t_us":5,"dir":"send","msg":{"jsonrpc":"2.0","id":"3","method":"getVM","params":{}}}),
        json!({"t_us":6,"dir":"recv","msg":{"jsonrpc":"2.0","id":"3","result":{"type":"VM","pid":7}}}),
    ];
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
    drop(f);
    let server = ReplayServer::start(&path);
    let (client, _notes) = connect(&server.ws_uri(), ConnectOptions::default())
        .await
        .unwrap();
    let first = client.call("getVMTimeline", json!({})).await.unwrap();
    assert_eq!(first["traceEvents"].as_array().unwrap().len(), 1);
    let again = client.call("getVMTimeline", json!({})).await.unwrap();
    assert_eq!(
        again,
        json!({"type":"Timeline","traceEvents":[],"timeOriginMicros":0,"timeExtentMicros":0})
    );
    let cpu = client
        .call("getPerfettoCpuSamples", json!({}))
        .await
        .unwrap();
    assert_eq!(cpu["sampleCount"], 3);
    let cpu = client
        .call("getPerfettoCpuSamples", json!({}))
        .await
        .unwrap();
    assert_eq!(cpu["sampleCount"], 0);
    assert_eq!(cpu["samples"], "");
    assert_eq!(cpu["type"], "PerfettoCpuSamples");
    // Other methods still repeat the last answer.
    for _ in 0..2 {
        assert_eq!(client.call("getVM", json!({})).await.unwrap()["pid"], 7);
    }
}
