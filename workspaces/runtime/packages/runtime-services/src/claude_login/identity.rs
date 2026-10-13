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
            "stat=".into(),
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
    let observation = std::str::from_utf8(&result.stdout)
        .map_err(|_| LoginError::Unavailable)?
        .trim();
    if result.code != Some(0) {
        return Err(LoginError::Unavailable);
    }
    parse(pid, observation)
}

fn parse(pid: u32, observation: &str) -> Result<Option<ChildIdentity>, LoginError> {
    let (state, start) = observation
        .split_once(char::is_whitespace)
        .ok_or(LoginError::Unavailable)?;
    let start = start.trim();
    if state.starts_with('Z') {
        return Ok(None);
    }
    if start.is_empty() || start.len() > 128 {
        return Err(LoginError::Unavailable);
    }
    Ok(Some(ChildIdentity {
        pid,
        start: start.to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn zombie_is_gone_and_live_start_identity_is_preserved() {
        assert_eq!(
            parse(123, "Z Tue Oct 13 12:00:00 2026").expect("zombie"),
            None
        );
        assert_eq!(
            parse(123, "S+ Tue Oct 13 12:00:00 2026").expect("live"),
            Some(ChildIdentity {
                pid: 123,
                start: "Tue Oct 13 12:00:00 2026".into()
            })
        );
        assert!(parse(123, "").is_err());
        assert!(parse(123, "S ").is_err());
    }
}
