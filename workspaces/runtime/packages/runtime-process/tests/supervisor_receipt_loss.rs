#![cfg(unix)]
#![allow(clippy::unwrap_used)]
use crate::process_support as support;
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
use support::*;

#[test]
fn lost_write_response_never_repeats_child_input() {
    let state = DisposableState::new();
    let mut supervisor = SupervisorProcess::start(&state);
    let handle = SupervisorHandle("receipt-loss".into());
    let count = state.0.join("writes");
    let script = r#"
printf '{"ready":true}\n'
while IFS= read -r line; do
 id=$(rpc_id "$line")
 method=$(printf '%s\n' "$line" | sed -E 's/.*"method":"([^"]*)".*/\1/')
 printf '%s\n' "$method" >> "$1"
 printf '{"id":%s,"result":"%s"}\n' "$id" "$method"
 [ "$method" != stop ] || break
done
"#;
    let mut command = shell_command(script);
    command.extend(["receipt-loss".into(), count.to_string_lossy().into_owned()]);
    let mut client = connect_client(&state);
    let opened = client
        .open(open_request(&handle.0, command, &state.0))
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
        json!({"protocol":studio_runtime_process::SUPERVISOR_PROTOCOL_V2,"stateDir":state.0,"backendId":identity.backend_id.0})
    )
    .unwrap();
    let mut reply = String::new();
    BufReader::new(&socket).read_line(&mut reply).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&reply).unwrap(),
        json!({"ok":true,"protocol":studio_runtime_process::SUPERVISOR_PROTOCOL_V2})
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
            native_id: Some(999.into()),
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
            native_id: Some(18.into()),
            message: json!({"method":"stop"}),
            extra: BTreeMap::new(),
        })
        .unwrap();
    poll_event(&mut client, &handle, first.sequence, |e| {
        e.kind == EventKind::Exit
    });
    assert_eq!(fs::read_to_string(count).unwrap(), "echo\nstop\n");
}
