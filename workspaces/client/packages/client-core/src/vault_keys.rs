//! Paper keys go only to the person's Terminal, never a captured Studio stream.
use serde_json::Value;
use std::{io::Write, path::Path};

fn script(prefix: &str, program: &str) -> Result<tempfile::NamedTempFile, String> {
    if prefix.is_empty()
        || prefix.len() > 44
        || !prefix.as_bytes()[0].is_ascii_alphanumeric()
        || !prefix
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        || matches!(prefix, "api" | "assets")
        || program.is_empty()
        || program.contains('\0')
    {
        return Err("Invalid vault name or program.".into());
    }
    let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
    let mut file = tempfile::Builder::new()
        .prefix("studio-vault-keys-")
        .suffix(".command")
        .tempfile()
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    write!(
        file,
        "#!/bin/sh\nrm -f \"$0\"\nexec {} keys --project {} --clear\n",
        quote(program),
        quote(prefix)
    )
    .map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    Ok(file)
}

fn program() -> Result<String, String> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let contents = executable
        .parent()
        .and_then(Path::parent)
        .ok_or("Missing application resources.")?;
    let bundled = contents.join("Resources/workspace/bin/layr-desk");
    if bundled.is_file() {
        return bundled
            .into_os_string()
            .into_string()
            .map_err(|_| "Invalid application path.".into());
    }
    #[cfg(debug_assertions)]
    return Ok(std::env::var("STUDIO_LAYR_DESK_BIN").unwrap_or_else(|_| "layr-desk".into()));
    #[cfg(not(debug_assertions))]
    Err("The bundled vault tool is missing.".into())
}

pub async fn show(value: &Value) -> Result<Value, String> {
    if !cfg!(target_os = "macos") {
        return Err("Vault keys require macOS Terminal.".into());
    }
    let prefix = value.as_str().ok_or("Invalid vault name.")?;
    let file = script(prefix, &program()?)?;
    // Keep only after a successful launcher exit. The command removes itself.
    // No shell or key process output is piped back into the client.
    let status = tokio::process::Command::new("/usr/bin/open")
        .arg(file.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("Could not open Terminal for the vault key.".into());
    }
    let (_, path) = file.keep().map_err(|e| e.to_string())?;
    drop(path);
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(unix)]
    fn command_runs_only_the_requested_program_and_removes_itself() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("tool with ' quote");
        std::fs::write(&program, "#!/bin/sh\n[ \"$#\" = 4 ] && [ \"$1\" = keys ] && [ \"$2\" = --project ] && [ \"$3\" = project_1 ] && [ \"$4\" = --clear ]\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let file = script("project_1", program.to_str().unwrap()).unwrap();
        let status = std::process::Command::new("/bin/sh")
            .arg(file.path())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(!file.path().exists());
    }
    #[test]
    fn rejects_names_and_preserves_literal_arguments_without_capturing_keys() {
        for prefix in ["", "api", "assets", "-option", "a;b", "a/b", "a\nb"] {
            assert!(script(prefix, "layr-desk").is_err());
        }
        let file = script("project_1", "/tmp/a'b/tool").unwrap();
        let content = std::fs::read_to_string(file.path()).unwrap();
        assert_eq!(
            content,
            "#!/bin/sh\nrm -f \"$0\"\nexec '/tmp/a'\\''b/tool' keys --project 'project_1' --clear\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                file.as_file().metadata().unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}
