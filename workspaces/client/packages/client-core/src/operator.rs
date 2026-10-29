//! Operator commands for the server registry (`layr-studio servers ...`).
//!
//! The running app owns the native profile lock. While it runs, the CLI sends
//! the request over a Unix socket in the profile folder and the app answers with
//! the same `RemoteClient` the window uses. Without the app, the CLI takes the
//! lock and runs the handler itself.
use crate::{
    server_keys::{FileVault, Vault},
    server_transport::RemoteClient,
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

/// Socket file name inside the profile folder. Mode 0600, so only this user connects.
pub const SOCKET_FILE: &str = "operator.sock";
/// The only methods the socket and the CLI accept.
const METHODS: [&str; 2] = ["servers.list", "servers.forget"];
/// Longest request line, in bytes.
const MAX_LINE: u64 = 64 * 1024;

/// Held for the whole life of a profile owner. Dropping it releases the lock.
pub struct ProfileLock(#[allow(dead_code)] std::fs::File);

/// Takes the profile lock. `None` means another process holds it.
#[allow(unsafe_code)]
pub fn try_lock_profile(directory: &Path) -> Result<Option<ProfileLock>, String> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("native-profile.lock"))
        .map_err(|e| e.to_string())?;
    // SAFETY: flock borrows the live file descriptor; ProfileLock retains the file.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(None);
    }
    Ok(Some(ProfileLock(file)))
}

/// Runs one accepted method on the client. A forget of an unknown id fails.
pub async fn handle(client: &RemoteClient, method: &str, params: &Value) -> Result<Value, String> {
    if !METHODS.contains(&method) {
        return Err(format!("The operator socket does not accept {method}."));
    }
    if method == "servers.forget" {
        let id = crate::server_keys::field(params, "serverId")?;
        let known = client.dispatch("servers.list", &json!({})).await?;
        if !known
            .as_array()
            .is_some_and(|v| v.iter().any(|s| s["id"] == id))
        {
            return Err(format!("Unknown server: {id}"));
        }
    }
    client.dispatch(method, params).await
}

/// Binds the socket with mode 0600 and answers one JSON line per connection.
pub fn serve(directory: &Path, client: Arc<RemoteClient>) -> Result<(), String> {
    let path = directory.join(SOCKET_FILE);
    // The caller holds the profile lock, so no live app owns an old socket file.
    drop(std::fs::remove_file(&path));
    let listener = UnixListener::bind(&path).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let client = client.clone();
            tokio::spawn(async move { drop(answer(stream, &client).await) });
        }
    });
    Ok(())
}

async fn answer(stream: UnixStream, client: &RemoteClient) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    BufReader::new(tokio::io::AsyncReadExt::take(read, MAX_LINE))
        .read_line(&mut line)
        .await?;
    let reply = match serde_json::from_str::<Value>(&line) {
        Ok(request) => handle(
            client,
            request["method"].as_str().unwrap_or(""),
            &request["params"],
        )
        .await
        .map_or_else(
            |error| json!({"error":error}),
            |result| json!({"result":result}),
        ),
        Err(_) => json!({"error":"Invalid operator request."}),
    };
    write.write_all(format!("{reply}\n").as_bytes()).await
}

/// Runs a method for the CLI: through the app socket when the app holds the
/// profile lock, otherwise directly under the lock.
pub async fn run(directory: &Path, method: &str, params: &Value) -> Result<Value, String> {
    if !METHODS.contains(&method) {
        return Err(format!("Unknown operator method: {method}"));
    }
    match try_lock_profile(directory)? {
        Some(_lock) => {
            let vault: Arc<dyn Vault> = Arc::new(FileVault::new(directory));
            handle(&RemoteClient::new(vault)?, method, params).await
        }
        None => {
            let stream = UnixStream::connect(directory.join(SOCKET_FILE))
                .await
                .map_err(|_| "The app is running but its operator socket is unavailable.")?;
            let (read, mut write) = stream.into_split();
            write
                .write_all(format!("{}\n", json!({"method":method,"params":params})).as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            let mut line = String::new();
            BufReader::new(read)
                .read_line(&mut line)
                .await
                .map_err(|e| e.to_string())?;
            let reply: Value =
                serde_json::from_str(&line).map_err(|_| "The app sent an invalid reply.")?;
            match reply["error"].as_str() {
                Some(error) => Err(error.to_owned()),
                None => Ok(reply["result"].clone()),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Memory(Mutex<Value>);
    impl Vault for Memory {
        fn read(&self) -> Result<Value, String> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn write(&self, state: &Value) -> Result<(), String> {
            *self.0.lock().unwrap() = state.clone();
            Ok(())
        }
    }
    fn seed() -> Value {
        json!({"drafts":{},"servers":{
            "alpha":{"origin":"https://alpha.example","label":"Alpha","secret":"x"},
            "beta":{"origin":"https://beta.example"}}})
    }
    fn dir() -> tempfile::TempDir {
        tempfile::tempdir_in("/tmp").unwrap()
    }
    fn file_profile(state: &Value) -> tempfile::TempDir {
        let dir = dir();
        std::fs::write(dir.path().join("server-keys.json"), state.to_string()).unwrap();
        dir
    }

    #[tokio::test]
    async fn operator_lists_and_forgets_a_server_in_a_profile() {
        let dir = file_profile(&seed());
        let list = run(dir.path(), "servers.list", &json!({})).await.unwrap();
        assert_eq!(list.as_array().unwrap().len(), 2);
        assert_eq!(list[0]["id"], "alpha");
        assert_eq!(list[0]["label"], "Alpha");
        assert_eq!(list[1]["label"], "beta");
        assert!(!list.to_string().contains("secret"));
        run(dir.path(), "servers.forget", &json!({"serverId":"alpha"}))
            .await
            .unwrap();
        let list = run(dir.path(), "servers.list", &json!({})).await.unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["id"], "beta");
    }

    #[tokio::test]
    async fn operator_forget_of_an_unknown_server_fails_and_keeps_the_file() {
        let dir = file_profile(&seed());
        let before = std::fs::read(dir.path().join("server-keys.json")).unwrap();
        let error = run(dir.path(), "servers.forget", &json!({"serverId":"nope"}))
            .await
            .unwrap_err();
        assert_eq!(error, "Unknown server: nope");
        assert_eq!(
            std::fs::read(dir.path().join("server-keys.json")).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn operator_socket_is_private_and_refuses_other_methods() {
        let dir = dir();
        let client = Arc::new(RemoteClient::new(Arc::new(Memory(Mutex::new(seed())))).unwrap());
        serve(dir.path(), client).unwrap();
        let mode = std::fs::metadata(dir.path().join(SOCKET_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _lock = try_lock_profile(dir.path()).unwrap().unwrap();
        for method in [
            "servers.pair",
            "servers.read",
            "api.request",
            "preferences.raw_all",
            "",
        ] {
            let stream = UnixStream::connect(dir.path().join(SOCKET_FILE))
                .await
                .unwrap();
            let (read, mut write) = stream.into_split();
            write
                .write_all(format!("{}\n", json!({"method":method,"params":{}})).as_bytes())
                .await
                .unwrap();
            let mut line = String::new();
            BufReader::new(read).read_line(&mut line).await.unwrap();
            let reply: Value = serde_json::from_str(&line).unwrap();
            assert!(
                reply["error"].as_str().unwrap().contains("does not accept"),
                "{method}"
            );
            assert!(reply["result"].is_null());
        }
    }

    #[tokio::test]
    async fn operator_uses_the_socket_and_never_writes_the_file_while_the_lock_is_held() {
        let profile = file_profile(&seed());
        let before = std::fs::read(profile.path().join("server-keys.json")).unwrap();
        let _held = try_lock_profile(profile.path()).unwrap().unwrap();
        // No socket yet: the CLI fails and leaves the file alone.
        let error = run(
            profile.path(),
            "servers.forget",
            &json!({"serverId":"alpha"}),
        )
        .await
        .unwrap_err();
        assert!(error.contains("operator socket"));
        assert_eq!(
            std::fs::read(profile.path().join("server-keys.json")).unwrap(),
            before
        );
        // The app's client (here in memory) receives the request.
        let memory = Arc::new(Memory(Mutex::new(seed())));
        serve(
            profile.path(),
            Arc::new(RemoteClient::new(memory.clone()).unwrap()),
        )
        .unwrap();
        run(
            profile.path(),
            "servers.forget",
            &json!({"serverId":"alpha"}),
        )
        .await
        .unwrap();
        assert!(memory.read().unwrap()["servers"].get("alpha").is_none());
        let list = run(profile.path(), "servers.list", &json!({}))
            .await
            .unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(
            std::fs::read(profile.path().join("server-keys.json")).unwrap(),
            before
        );
    }
}
