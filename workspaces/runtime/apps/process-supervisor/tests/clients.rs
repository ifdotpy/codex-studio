//! Run the existing Rust client contracts against this crate's built binary in
//! the default fast tier. Each test creates and cleans its own disposable state.
#![cfg(any(target_os = "linux", target_os = "macos"))]

#[path = "../../../packages/runtime-process/tests/support/mod.rs"]
mod process_support;

#[path = "../../../packages/runtime-process/tests/supervisor_v2.rs"]
mod protocol_v2;

#[allow(clippy::unwrap_used)]
mod admission {
    use super::process_support::*;
    use serde_json::{Value, json};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use studio_runtime_process::{CursorRequest, EventCursor, HealthRequest, SupervisorHandle};

    fn exchange(socket: &mut BufReader<UnixStream>, request: Value) -> Value {
        writeln!(socket.get_mut(), "{request}").unwrap();
        let mut response = String::new();
        socket.read_line(&mut response).unwrap();
        serde_json::from_str(&response).unwrap()
    }

    fn connect(state: &DisposableState) -> BufReader<UnixStream> {
        let socket = UnixStream::connect(safe_socket_path(state)).unwrap();
        socket.set_read_timeout(Some(HANG_GUARD)).unwrap();
        socket.set_write_timeout(Some(HANG_GUARD)).unwrap();
        BufReader::new(socket)
    }

    #[test]
    fn supervisor_admission_refuses_unidentified_and_ambiguous_connections() {
        let state = DisposableState::new();
        let _supervisor = SupervisorProcess::start(&state);
        let mut owner = connect_client(&state);
        for fields in [
            json!({}),
            json!({"backendId":""}),
            json!({"probe":true,"operator":true}),
            json!({"probe":true,"backendId":"bypass"}),
            json!({"backendId":"another-backend"}),
        ] {
            let mut handshake = json!({"protocol":2,"stateDir":state.0});
            handshake
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let response = exchange(&mut connect(&state), handshake.clone());
            assert!(
                response.get("error").is_some(),
                "admitted {handshake}: {response}"
            );
        }
        owner.health(HealthRequest::default()).unwrap();
        connect_client(&state)
            .health(HealthRequest::default())
            .unwrap();
    }

    #[test]
    fn supervisor_admission_limits_probe_and_operator_before_journal_dispatch() {
        let state = DisposableState::new();
        let mut supervisor = SupervisorProcess::start(&state);
        let mut owner = connect_client(&state);
        let opened = owner.open(open_request("owned", shell_command(
            "printf '{\"ready\":true}\\n'; while IFS= read -r line; do printf '%s\\n' \"$line\"; done"
        ), &state.0)).unwrap();
        supervisor.own_child(opened.pid);
        let event = poll_event(
            &mut owner,
            &SupervisorHandle("owned".into()),
            EventCursor(0),
            |_| true,
        );
        for (role, legacy_operator) in [("probe", false), ("operator", false), ("operator", true)] {
            let mut socket = connect(&state);
            let mut handshake = json!({"protocol":2,"stateDir":state.0});
            handshake[role] = json!(true);
            if legacy_operator {
                handshake["backendId"] = json!("another-backend");
            }
            assert_eq!(exchange(&mut socket, handshake)["ok"], true);
            assert!(
                exchange(&mut socket, json!({"action":"health"}))
                    .get("result")
                    .is_some()
            );
            let mut requests = vec![
                json!({"action":"write","handle":"owned","operationId":"forbidden","message":{"method":"forbidden"}}),
                json!({"action":"ack","handle":"owned","sequence":event.sequence.0}),
                json!({"action":"replay","handle":"owned","cursor":0}),
                json!({"action":"next","handle":"owned","cursor":0}),
                json!({"action":"nextBatch","handle":"owned","cursor":0,"generation":opened.generation.0,"limit":1}),
                json!({"action":"operationStatus","handle":"owned","operationId":"forbidden"}),
                json!({"action":"status","handle":"owned"}),
                json!({"action":"detach","handle":"owned"}),
                json!({"action":"retireCommand","handle":"owned","generation":opened.generation.0}),
                json!({"action":"open","handle":"owned","command":["/bin/true"],"env":{},"cwd":null}),
            ];
            if role == "probe" {
                requests.push(json!({"action":"finishFallback"}));
                requests.push(json!({"action":"adminCloseHandle","handle":"owned","expectedPid":opened.pid,"expectedStartTime":"","expectedSignature":""}));
            }
            for (index, mut request) in requests.into_iter().enumerate() {
                request["requestId"] = json!(index);
                let response = exchange(&mut socket, request.clone());
                assert_eq!(response["requestId"], index);
                assert!(
                    response["error"]
                        .as_str()
                        .is_some_and(|error| error.contains("capability")),
                    "{role} {request}: {response}"
                );
            }
            if role == "operator" {
                assert_eq!(
                    exchange(&mut socket, json!({"action":"finishFallback"}))["result"]["finished"],
                    true
                );
            }
        }
        let health = owner.health(HealthRequest::default()).unwrap();
        assert_eq!(health.handles[0].acknowledged, 0);
        assert_eq!(health.handles[0].pid, Some(opened.pid));
        let replay = owner
            .replay(CursorRequest {
                handle: SupervisorHandle("owned".into()),
                cursor: EventCursor(0),
                extra: Default::default(),
            })
            .unwrap();
        assert_eq!(replay.events[0].sequence, event.sequence);
    }
}
