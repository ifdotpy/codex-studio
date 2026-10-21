//! Bounded, credential-free native Codex compatibility checks for native-runtime.
//!
//! Python parity: `codex_native_binary.py:87-96` (isolated child environment),
//! `:117-175` (bounded command and version), `:272-303` (protocol schema),
//! `:382-453` (app-server smoke), and `:463-478` (Code Mode handshake).

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{Value, json};
use studio_runtime_services::{
    native_runtime::{
        CompatibilityEvidence, CompatibilityProbe, CompatibilityRefusal, CompatibilityResult,
        NativeRuntimeError,
    },
    provider_binaries::NativePlatform,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const SMOKE_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_FRAME_BYTES: usize = 1024 * 1024;

pub(crate) struct SystemCompatibilityProbe {
    runtime: tokio::runtime::Handle,
}

impl SystemCompatibilityProbe {
    pub(crate) fn new(runtime: tokio::runtime::Handle) -> Self {
        Self { runtime }
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "native compatibility probes run on labeled native-runtime blocking workers"
    )]
    fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.runtime.block_on(future)
    }
}

impl CompatibilityProbe for SystemCompatibilityProbe {
    fn version(&self, executable: &Path) -> Result<String, NativeRuntimeError> {
        let executable = executable.to_owned();
        let temp = ProbeHome::new()?;
        let output = self.block_on(run(
            &executable,
            &["--version"],
            temp.path(),
            COMMAND_TIMEOUT,
        ))?;
        studio_runtime_services::provider_binaries::codex_cli_version(&output)
            .map_err(|_| NativeRuntimeError::InvalidVersion)
    }

    fn check(
        &self,
        bundle: &Path,
        platform: NativePlatform,
    ) -> Result<CompatibilityResult, NativeRuntimeError> {
        let executable = bundle.join(platform.executable_name());
        let temp = ProbeHome::new()?;
        match self.block_on(check_bundle(&executable, bundle, temp.path())) {
            Ok(evidence) => Ok(CompatibilityResult::Approved(evidence)),
            Err(CompatibilityRefusal::ProtocolSchema) => Ok(CompatibilityResult::Refused(
                CompatibilityRefusal::ProtocolSchema,
            )),
            Err(CompatibilityRefusal::ProviderSmoke) => Ok(CompatibilityResult::Refused(
                CompatibilityRefusal::ProviderSmoke,
            )),
            Err(CompatibilityRefusal::CompanionCheck) => Ok(CompatibilityResult::Refused(
                CompatibilityRefusal::CompanionCheck,
            )),
            Err(CompatibilityRefusal::UnsupportedPlatform) => Ok(CompatibilityResult::Refused(
                CompatibilityRefusal::UnsupportedPlatform,
            )),
        }
    }
}

async fn check_bundle(
    executable: &Path,
    bundle: &Path,
    home: &Path,
) -> Result<CompatibilityEvidence, CompatibilityRefusal> {
    let names = if cfg!(windows) {
        ["codex-code-mode-host.exe"]
    } else {
        ["codex-code-mode-host"]
    };
    let mut companions = BTreeMap::new();
    for name in names {
        let path = bundle.join(name);
        let help = run(&path, &["--help"], home, COMMAND_TIMEOUT)
            .await
            .map_err(|_| CompatibilityRefusal::CompanionCheck)?;
        let help = String::from_utf8_lossy(&help);
        if !help.contains(&format!("Usage: {name}")) || !help.contains("--listen") {
            return Err(CompatibilityRefusal::CompanionCheck);
        }
        let hello = serde_json::to_vec(&json!({
            "type":"connection/hello",
            "supportedVersions":[1],
            "requiredCapabilities":[],
            "optionalCapabilities":[]
        }))
        .map_err(|_| CompatibilityRefusal::CompanionCheck)?;
        let mut frame = u32::try_from(hello.len())
            .map_err(|_| CompatibilityRefusal::CompanionCheck)?
            .to_le_bytes()
            .to_vec();
        frame.extend(hello);
        let reply = run_input(&path, &["--listen", "stdio"], home, &frame, COMMAND_TIMEOUT)
            .await
            .map_err(|_| CompatibilityRefusal::CompanionCheck)?;
        if reply.len() < 4
            || usize::try_from(u32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]])).ok()
                != Some(reply.len() - 4)
        {
            return Err(CompatibilityRefusal::CompanionCheck);
        }
        let value: Value = serde_json::from_slice(&reply[4..])
            .map_err(|_| CompatibilityRefusal::CompanionCheck)?;
        if value.get("type").and_then(Value::as_str) != Some("connection/ready")
            || value.get("selectedVersion") != Some(&json!(1))
            || !value.get("capabilities").is_some_and(Value::is_array)
        {
            return Err(CompatibilityRefusal::CompanionCheck);
        }
        companions.insert(name.to_owned(), json!({"smoke":true}));
    }

    let schema_dir = home.join("schemas");
    fs::create_dir(&schema_dir).map_err(|_| CompatibilityRefusal::ProtocolSchema)?;
    run(
        executable,
        &[
            "app-server",
            "generate-json-schema",
            "--experimental",
            "--out",
            schema_dir
                .to_str()
                .ok_or(CompatibilityRefusal::ProtocolSchema)?,
        ],
        home,
        COMMAND_TIMEOUT,
    )
    .await
    .map_err(|_| CompatibilityRefusal::ProtocolSchema)?;
    let schema = schema_check(&schema_dir)?;
    smoke(executable, home)
        .await
        .map_err(|_| CompatibilityRefusal::ProviderSmoke)?;
    Ok(CompatibilityEvidence {
        schema,
        smoke: json!(["initialize", "config/read", "thread/loaded/list"]),
        companions,
    })
}

fn schema_check(directory: &Path) -> Result<Value, CompatibilityRefusal> {
    let read = |name: &str| -> Result<Value, CompatibilityRefusal> {
        let path = directory.join(name);
        let metadata = fs::metadata(&path).map_err(|_| CompatibilityRefusal::ProtocolSchema)?;
        if !metadata.is_file() || metadata.len() > 32 * 1024 * 1024 {
            return Err(CompatibilityRefusal::ProtocolSchema);
        }
        let bytes = fs::read(path).map_err(|_| CompatibilityRefusal::ProtocolSchema)?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| CompatibilityRefusal::ProtocolSchema)?;
        value
            .as_object()
            .ok_or(CompatibilityRefusal::ProtocolSchema)?;
        Ok(value)
    };
    let requests = read("ClientRequest.json")?;
    let methods = requests
        .get("oneOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|variant| {
            variant
                .pointer("/properties/method/enum")
                .and_then(Value::as_array)
        })
        .flatten()
        .filter_map(Value::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let required = [
        "initialize",
        "config/read",
        "thread/loaded/list",
        "turn/start",
        "thread/metadata/update",
        "thread/queue/list",
        "thread/backgroundTerminals/list",
        "thread/settings/update",
        "model/list",
    ];
    if required.iter().any(|method| !methods.contains(method)) {
        return Err(CompatibilityRefusal::ProtocolSchema);
    }
    let turn = read("v2/TurnStartParams.json")?;
    if turn.pointer("/properties/cyberAccessProgram").is_none()
        || !["standard", "daybreakBlue", "daybreakRed"]
            .iter()
            .all(|program| {
                turn.pointer("/definitions/CyberAccessProgram/enum")
                    .and_then(Value::as_array)
                    .is_some_and(|values| values.iter().any(|value| value == program))
            })
    {
        return Err(CompatibilityRefusal::ProtocolSchema);
    }
    let models = read("v2/ModelListResponse.json")?;
    if models
        .pointer("/definitions/Model/properties/availableAccessPrograms")
        .is_none()
    {
        return Err(CompatibilityRefusal::ProtocolSchema);
    }
    Ok(json!({"methods":required,"daybreak":true}))
}

async fn smoke(executable: &Path, home: &Path) -> Result<(), ()> {
    let mut command = Command::new(executable);
    command
        .args([
            "app-server",
            "--listen",
            "stdio://",
            "-c",
            "cli_auth_credentials_store=\"file\"",
        ])
        .current_dir(home)
        .env_clear()
        .envs(safe_environment(home))
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().map_err(|_| ())?;
    let mut input = child.stdin.take().ok_or(())?;
    let output = child.stdout.take().ok_or(())?;
    let mut reader = BufReader::new(output);
    let deadline = tokio::time::Instant::now() + SMOKE_TIMEOUT;
    let result = async {
        let initialize = request(
            1,
            "initialize",
            studio_runtime_providers::initialize_params(
                studio_runtime_providers::ClientName::UpdateProbe,
            ),
        );
        write_frame(&mut input, &initialize).await?;
        let initialized = response(&mut reader, 1, deadline).await?;
        if initialized
            .get("userAgent")
            .and_then(Value::as_str)
            .is_none()
        {
            return Err(());
        }
        write_frame(&mut input, &json!({"method":"initialized"})).await?;
        write_frame(
            &mut input,
            &request(2, "config/read", json!({"includeLayers":false})),
        )
        .await?;
        let config = response(&mut reader, 2, deadline).await?;
        if !config.get("config").is_some_and(Value::is_object) {
            return Err(());
        }
        write_frame(&mut input, &request(3, "thread/loaded/list", json!({}))).await?;
        let loaded = response(&mut reader, 3, deadline).await?;
        if loaded.get("data") != Some(&json!([])) {
            return Err(());
        }
        Ok(())
    }
    .await;
    drop(input);
    let _ignored = child.kill().await;
    let _ignored = child.wait().await;
    result
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"id":id,"method":method,"params":params})
}

async fn write_frame(input: &mut tokio::process::ChildStdin, value: &Value) -> Result<(), ()> {
    let mut bytes = serde_json::to_vec(value).map_err(|_| ())?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(());
    }
    bytes.push(b'\n');
    input.write_all(&bytes).await.map_err(|_| ())?;
    input.flush().await.map_err(|_| ())
}

async fn response(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    id: u64,
    deadline: tokio::time::Instant,
) -> Result<Value, ()> {
    loop {
        let line = tokio::time::timeout_at(deadline, bounded_line(reader, MAX_FRAME_BYTES))
            .await
            .map_err(|_| ())??;
        let value: Value = serde_json::from_slice(&line).map_err(|_| ())?;
        if value.get("method").is_some() {
            if value.get("id").is_some() {
                return Err(());
            }
            continue;
        }
        if value.get("id") != Some(&json!(id)) || value.get("error").is_some() {
            return Err(());
        }
        return value.get("result").cloned().ok_or(());
    }
}

async fn bounded_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    limit: usize,
) -> Result<Vec<u8>, ()> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(|_| ())?;
        if available.is_empty() {
            return Err(());
        }
        let length = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(length) > limit {
            return Err(());
        }
        let done = available.get(length.saturating_sub(1)) == Some(&b'\n');
        line.extend_from_slice(&available[..length]);
        reader.consume(length);
        if done {
            line.pop();
            return Ok(line);
        }
    }
}

async fn run(
    program: &Path,
    args: &[&str],
    home: &Path,
    timeout: Duration,
) -> Result<Vec<u8>, NativeRuntimeError> {
    run_input(program, args, home, &[], timeout).await
}

async fn run_input(
    program: &Path,
    args: &[&str],
    home: &Path,
    input: &[u8],
    timeout: Duration,
) -> Result<Vec<u8>, NativeRuntimeError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(home)
        .env_clear()
        .envs(safe_environment(home))
        .kill_on_drop(true)
        .stdin(if input.is_empty() {
            std::process::Stdio::null()
        } else {
            std::process::Stdio::piped()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("native probe stdout is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("native probe stderr is unavailable"))?;
    let mut stdout_task = tokio::spawn(read_bounded(stdout, MAX_OUTPUT_BYTES));
    let mut stderr_task = tokio::spawn(read_bounded(stderr, MAX_OUTPUT_BYTES));
    let result = tokio::time::timeout(timeout, async {
        if !input.is_empty() {
            let input_pipe = stdin
                .as_mut()
                .ok_or_else(|| io::Error::other("native probe stdin is unavailable"))?;
            input_pipe.write_all(input).await?;
        }
        drop(stdin.take());
        let status = child.wait().await?;
        let stdout = (&mut stdout_task)
            .await
            .map_err(|_| io::Error::other("native probe stdout reader failed"))??;
        let stderr = (&mut stderr_task)
            .await
            .map_err(|_| io::Error::other("native probe stderr reader failed"))??;
        Ok::<_, io::Error>((status, stdout, stderr))
    })
    .await;
    let (status, stdout, stderr) = match result {
        Ok(result) => result?,
        Err(_) => {
            let _ignored = child.kill().await;
            let _ignored = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "native compatibility command timed out",
            )
            .into());
        }
    };
    if stdout.len().saturating_add(stderr.len()) > MAX_OUTPUT_BYTES || !status.success() {
        return Err(io::Error::other("native compatibility command failed").into());
    }
    Ok(stdout)
}

async fn read_bounded<R: AsyncRead + Unpin>(reader: R, limit: usize) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_to_end(&mut output)
        .await?;
    if output.len() > limit {
        return Err(io::Error::other(
            "native compatibility output exceeded limit",
        ));
    }
    Ok(output)
}

fn safe_environment(home: &Path) -> BTreeMap<&'static str, String> {
    let mut environment = BTreeMap::new();
    for name in [
        "PATH",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "LANG",
        "LC_ALL",
        "TMPDIR",
    ] {
        if let Some(value) = std::env::var_os(name) {
            environment.insert(name, value.to_string_lossy().into_owned());
        }
    }
    let home = home.to_string_lossy().into_owned();
    for name in [
        "HOME",
        "USERPROFILE",
        "TEMP",
        "TMP",
        "CODEX_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "TMPDIR",
    ] {
        environment.insert(name, home.clone());
    }
    environment
}

struct ProbeHome(PathBuf);

impl ProbeHome {
    fn new() -> Result<Self, NativeRuntimeError> {
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
        let path = std::env::temp_dir().join(format!(
            "studio-native-probe-{}",
            data_encoding::HEXLOWER.encode(&nonce)
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ProbeHome {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::SystemCompatibilityProbe;
    use serde_json::json;
    use std::{fs, os::unix::fs::PermissionsExt};
    use studio_runtime_services::{
        native_runtime::{CompatibilityProbe, CompatibilityResult},
        provider_binaries::NativePlatform,
    };

    fn write_executable(path: &std::path::Path, source: &str) {
        fs::write(path, source).expect("fixture executable");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("make fixture executable");
    }

    #[test]
    fn version_probe_runs_candidate_with_isolated_home() {
        let directory = tempfile::tempdir().expect("temporary fixture directory");
        let executable = directory.path().join("codex");
        write_executable(
            &executable,
            "#!/bin/sh\ntest \"$HOME\" = \"$CODEX_HOME\" || exit 7\ntest -z \"${OPENAI_API_KEY:-}\" || exit 8\nprintf 'codex-cli 1.2.3\\n'\n",
        );
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("probe runtime");
        let probe = SystemCompatibilityProbe::new(runtime.handle().clone());
        let version = runtime.block_on(async move {
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-server/native_probe",
                move || probe.version(&executable),
            )
            .await
            .expect("version probe task")
        });
        assert_eq!(version.expect("valid version"), "1.2.3");
    }

    #[test]
    fn compatibility_probe_checks_companion_schema_and_isolated_provider() {
        let directory = tempfile::tempdir().expect("temporary fixture directory");
        let bundle = directory.path().to_owned();
        let codex = bundle.join("codex");
        write_executable(
            &codex,
            r##"#!/usr/bin/env python3
import json, os, pathlib, sys
assert 'OPENAI_API_KEY' not in os.environ
assert 'CODEX_API_KEY' not in os.environ
assert os.environ['HOME'] == os.environ['CODEX_HOME']
if '--version' in sys.argv:
    print('codex-cli 1.2.3')
elif 'generate-json-schema' in sys.argv:
    root = pathlib.Path(sys.argv[sys.argv.index('--out') + 1])
    schemas = {
        'ClientRequest.json': {'oneOf': [
            {'properties': {'method': {'enum': [method]}}} for method in [
                'initialize', 'config/read', 'thread/loaded/list', 'turn/start',
                'thread/metadata/update', 'thread/queue/list',
                'thread/backgroundTerminals/list', 'thread/settings/update', 'model/list']]},
        'v2/TurnStartParams.json': {'properties': {'cyberAccessProgram': {}},
            'definitions': {'CyberAccessProgram': {'enum': ['standard', 'daybreakBlue', 'daybreakRed']}}},
        'v2/ModelListResponse.json': {'definitions': {'Model': {'properties': {'availableAccessPrograms': {}}}}},
    }
    for name, value in schemas.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value))
else:
    for line in sys.stdin:
        frame = json.loads(line)
        if frame.get('method') == 'initialized':
            continue
        result = {'initialize': {'userAgent': 'fixture'},
                  'config/read': {'config': {}}, 'thread/loaded/list': {'data': []}}[frame['method']]
        print(json.dumps({'id': frame['id'], 'result': result}), flush=True)
"##,
        );
        let companion = bundle.join("codex-code-mode-host");
        write_executable(
            &companion,
            r##"#!/usr/bin/env python3
import json, struct, sys
if '--help' in sys.argv:
    print('Usage: codex-code-mode-host --listen ADDRESS')
else:
    stream = sys.stdin.buffer
    size = struct.unpack('<I', stream.read(4))[0]
    hello = json.loads(stream.read(size))
    assert hello['type'] == 'connection/hello'
    reply = json.dumps({'type': 'connection/ready', 'selectedVersion': 1, 'capabilities': []}).encode()
    sys.stdout.buffer.write(struct.pack('<I', len(reply)) + reply)
"##,
        );
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("probe runtime");
        let probe = SystemCompatibilityProbe::new(runtime.handle().clone());
        let result = runtime.block_on(async move {
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-server/native_probe",
                move || probe.check(&bundle, NativePlatform::LinuxX64),
            )
            .await
            .expect("compatibility probe task")
        });
        let result = result.expect("compatibility result");
        let CompatibilityResult::Approved(evidence) = result else {
            panic!("compatible fixture should be approved, got {result:?}");
        };
        assert_eq!(
            evidence.smoke,
            json!(["initialize", "config/read", "thread/loaded/list"])
        );
        assert_eq!(evidence.schema["daybreak"], json!(true));
        assert_eq!(
            evidence.companions["codex-code-mode-host"]["smoke"],
            json!(true)
        );
    }
}
