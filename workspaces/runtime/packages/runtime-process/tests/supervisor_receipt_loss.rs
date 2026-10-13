#![cfg(unix)]
#![allow(clippy::unwrap_used)]
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
};
use studio_runtime_process::{
    EventCursor, EventKind, OperationId, OperationStatusRequest, SupervisorHandle, WriteRequest,
};
mod support;
use support::*;

#[test]
fn lost_write_response_never_repeats_child_input() {
    let state = DisposableState::new();
    let python = python_executable();
    let mut supervisor = SupervisorProcess::start(&state, &python);
    let handle = SupervisorHandle("receipt-loss".into());
    let count = state.0.join("writes");
    let script = r#"
import json, sys
print(json.dumps({'ready':True}),flush=True)
for line in sys.stdin:
    message=json.loads(line)
    with open(sys.argv[1],'a') as f: f.write(message['method']+'\n')
    print(json.dumps({'id':message['id'],'result':message['method']}),flush=True)
    if message['method']=='stop': break
"#;
    let mut client = connect_client(&state);
    let opened = client
        .open(open_request(
            &handle.0,
            vec![
                python,
                "-u".into(),
                "-c".into(),
                script.into(),
                count.to_string_lossy().into_owned(),
            ],
            &state.0,
        ))
        .unwrap();
    supervisor.own_child(opened.pid);
    let ready = poll_event(&mut client, &handle, EventCursor(0), |e| {
        e.payload.get("ready") == Some(&Value::Bool(true))
    });
    let mut socket = UnixStream::connect(state.socket()).unwrap();
    let identity = state.identity();
    writeln!(
        socket,
        "{}",
        json!({"protocol":1,"stateDir":state.0,"backendId":identity.backend_id.0})
    )
    .unwrap();
    let mut reply = String::new();
    BufReader::new(&socket).read_line(&mut reply).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&reply).unwrap(),
        json!({"ok":true})
    );
    writeln!(socket,"{}",json!({"requestId":99,"action":"write","handle":handle.0,"operationId":"monitor:lost","nativeId":17,"message":{"method":"echo","params":{"text":"one"}}})).unwrap();
    drop(socket); // The action response cannot reach this backend connection.
    let first = poll_event(&mut client, &handle, ready.sequence, |e| {
        e.payload["result"] == "echo"
    });
    let receipt = client
        .operation_status(OperationStatusRequest {
            handle: handle.clone(),
            operation_id: OperationId("monitor:lost".into()),
            extra: BTreeMap::new(),
        })
        .unwrap();
    assert!(receipt.accepted);
    let duplicate = client
        .write(WriteRequest {
            handle: handle.clone(),
            operation_id: OperationId("monitor:lost".into()),
            native_id: Some(999),
            message: json!({"method":"echo","params":{"text":"one"}}),
            extra: BTreeMap::new(),
        })
        .unwrap();
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.remote_id, receipt.native_id);
    client
        .write(WriteRequest {
            handle: handle.clone(),
            operation_id: OperationId("monitor:stop".into()),
            native_id: Some(18),
            message: json!({"method":"stop"}),
            extra: BTreeMap::new(),
        })
        .unwrap();
    poll_event(&mut client, &handle, first.sequence, |e| {
        e.kind == EventKind::Exit
    });
    assert_eq!(fs::read_to_string(count).unwrap(), "echo\nstop\n");
}
