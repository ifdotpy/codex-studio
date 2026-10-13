//! Bounded process identity observation through the shared child actor.
use super::{LoginError, ports::ChildIdentity};
use std::{collections::BTreeMap, time::Duration};
use studio_runtime_process::stream_child::{ChildSpec, ExitReason, capture};

pub async fn observe(pid: u32) -> Result<Option<ChildIdentity>, LoginError> {
    if pid == 0 {
        return Err(LoginError::Unavailable);
    }
    let result = capture(ChildSpec {
        program: "/bin/ps".into(),
        args: vec![
            "-p".into(),
            pid.to_string().into(),
            "-o".into(),
            "lstart=".into(),
        ],
        cwd: "/".into(),
        environment: BTreeMap::from([("LC_ALL".into(), "C".into()), ("TZ".into(), "UTC".into())]),
        timeout: Duration::from_secs(2),
        inherited_lease: None,
    })
    .await
    .map_err(|_| LoginError::Unavailable)?;
    if result.reason != ExitReason::Completed {
        return Err(LoginError::Unavailable);
    }
    if result.code == Some(1) && result.stdout.is_empty() {
        return Ok(None);
    }
    let start = std::str::from_utf8(&result.stdout)
        .map_err(|_| LoginError::Unavailable)?
        .trim();
    if result.code != Some(0) || start.is_empty() || start.len() > 128 {
        return Err(LoginError::Unavailable);
    }
    Ok(Some(ChildIdentity {
        pid,
        start: start.to_owned(),
    }))
}
