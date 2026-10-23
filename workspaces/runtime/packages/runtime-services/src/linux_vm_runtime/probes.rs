//! Guest health and LAYR probes of the watchdog, their records in
//! `guest-health.json`, and the unresponsive episodes in `watchdog.json`
//! (Python `Client.record_health`, `record_layr`, `layr_fault`,
//! `_guest_failure`, `ping` and `Watchdog.probe`, `probe_layr`, `_unresponsive`,
//! `summary`). An unresponsive guest and a LAYR fault are shown, never
//! restarted: no answer does not prove death.

use super::*;

/// A guest that answered in this boot and then gave no answer for this many
/// seconds, over at least [`UNANSWERED_PROBES`] probes, is unresponsive.
const UNANSWERED_SECONDS: f64 = 180.0;
const UNANSWERED_PROBES: u64 = 3;
/// A longer gap between two probes (Mac sleep, backend stop) starts a new
/// failing span, in seconds.
const PROBE_GAP_SECONDS: f64 = 90.0;
/// Wait of one guest ping, in seconds.
const PING_SECONDS: u64 = 15;
/// A close without an answer this soon is a full guest service (32 clients)
/// or a full helper (64 channels), not a dead guest, in seconds. The helper
/// closes a connection that a dead kernel never accepts only after 15 seconds.
const BUSY_CLOSE_SECONDS: f64 = 10.0;
/// The probe result of a busy guest: neither an answer nor a failure.
const BUSY: &str = "busy";
/// The watchdog asks `layr.health` at most once in this many seconds, after a
/// guest answer.
const LAYR_PROBE_SECONDS: f64 = 60.0;
/// Wait of one `layr.health`, in seconds.
const LAYR_PROBE_WAIT_SECONDS: u64 = 20;
/// [`LAYR_FAULT_PROBES`] failed answers over this many seconds make a LAYR
/// fault; a slow layr command or one restart of a unit does not.
const LAYR_FAULT_SECONDS: f64 = 120.0;
const LAYR_FAULT_PROBES: u64 = 3;
/// A fault record older than this many seconds (a stopped VM, a stopped
/// backend) is not shown.
const LAYR_FAULT_FRESH_SECONDS: f64 = 300.0;
/// The longest saved probe error, in characters.
const PROBE_ERROR_CHARS: usize = 300;

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64)
}

fn count(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or(0)
}

fn helper_pid(pid: Option<&Value>) -> Option<&Value> {
    pid.filter(|pid| pid.is_i64() || pid.is_u64())
}

/// Record one probe in `record`: `None` is an answer, any text a failure.
/// `gap` is the longest pause between two probes of one failing span.
fn record_result(
    record: &mut Map<String, Value>,
    error: Option<&str>,
    at: f64,
    gap: f64,
    ready: &str,
) {
    let previous = number(record.get("lastProbeAt"));
    match error {
        None => {
            for (field, value) in [
                (ready, json!(at)),
                ("failingSince", Value::Null),
                ("lastFailureAt", Value::Null),
                ("failures", json!(0)),
                ("error", Value::Null),
            ] {
                record.insert(field.into(), value);
            }
        }
        Some(error) => {
            if record.get("failingSince").is_none_or(Value::is_null)
                || previous.is_none_or(|previous| at - previous > gap)
            {
                record.insert("failingSince".into(), json!(at));
                record.insert("failures".into(), json!(0));
            }
            let failures = count(record.get("failures")) + 1;
            record.insert("failures".into(), json!(failures));
            record.insert("lastFailureAt".into(), json!(at));
            record.insert(
                "error".into(),
                json!(error.chars().take(PROBE_ERROR_CHARS).collect::<String>()),
            );
        }
    }
    record.insert("lastProbeAt".into(), json!(at));
}

impl VmRuntime {
    fn save_health(&self, saved: Map<String, Value>) {
        if let Err(error) = write_json(&self.state_dir.join(HEALTH_FILE), &Value::Object(saved)) {
            eprintln!("linux-vm: the guest health record is not saved: {error}");
        }
    }

    /// Save one probe of the helper with this PID (one boot): `None` is an
    /// answer, [`BUSY`] neither an answer nor a failure, any other text a failure.
    pub(super) fn record_probe(&self, pid: Option<&Value>, error: Option<&str>) {
        let Some(pid) = helper_pid(pid) else {
            return;
        };
        let mut saved = self.read_json(HEALTH_FILE);
        if saved.get("helperPid") != Some(pid) {
            saved = Map::new();
            saved.insert("helperPid".into(), pid.clone());
            saved.insert("healthyAt".into(), Value::Null);
        }
        let at = now();
        if error == Some(BUSY) {
            saved.insert("busyAt".into(), json!(at));
            saved.insert("lastProbeAt".into(), json!(at));
        } else {
            record_result(&mut saved, error, at, PROBE_GAP_SECONDS, "healthyAt");
        }
        self.save_health(saved);
    }

    /// One ping of the guest service: `None` when it answers, [`BUSY`] for a
    /// fast close without an answer, else why not. Any answer proves the
    /// guest: the service answers this unknown method with an error from its
    /// event loop, without the disk, layr or the root broker.
    pub(super) fn ping(&self) -> Option<String> {
        let started = Instant::now();
        match self
            .guest
            .call("ping", &json!({}), None, Duration::from_secs(PING_SECONDS))
        {
            Ok(_) | Err(GuestError::Rejected { .. }) => None,
            // A malformed answer still came from the guest.
            Err(GuestError::Protocol { message, .. }) if message.contains("timeout") => Some(
                format!("The guest gave no answer in {PING_SECONDS} seconds."),
            ),
            Err(GuestError::Protocol { .. }) => None,
            Err(GuestError::Transport {
                uncertain: true, ..
            }) if started.elapsed().as_secs_f64() < BUSY_CLOSE_SECONDS => Some(BUSY.into()),
            Err(GuestError::Transport {
                uncertain: true, ..
            }) => Some("The guest connection closed without an answer.".into()),
            Err(error) => Some(format!("The guest connection failed: {error}")),
        }
    }

    /// The unanswered guest behind a running helper: it answered in this boot
    /// and then gave no answer for [`UNANSWERED_SECONDS`].
    pub(super) fn unanswered(&self, pid: Option<&Value>) -> Option<Value> {
        let pid = helper_pid(pid)?;
        let health = self.read_json(HEALTH_FILE);
        let since = number(health.get("failingSince"))?;
        let last = number(health.get("lastFailureAt"))?;
        (health.get("helperPid") == Some(pid)
            && health.get("healthyAt").is_some_and(|at| !at.is_null())
            && count(health.get("failures")) >= UNANSWERED_PROBES
            && last - since >= UNANSWERED_SECONDS)
            .then(|| {
                json!({"kind": "unanswered", "since": since,
                    "reason": format!("The guest has not answered for {} seconds.", (last - since) as u64)})
            })
    }

    /// A layr daemon or root broker that has not answered for
    /// [`LAYR_FAULT_SECONDS`] while the guest answers.
    pub(super) fn layr_fault(&self, pid: Option<&Value>, at: f64) -> Option<Value> {
        let health = self.read_json(HEALTH_FILE);
        let layr = health.get("layr")?.as_object()?;
        let since = number(layr.get("failingSince"))?;
        let last = number(layr.get("lastFailureAt"))?;
        let error = layr.get("error").and_then(Value::as_str);
        (helper_pid(pid).is_some_and(|pid| health.get("helperPid") == Some(pid))
            && at - last <= LAYR_FAULT_FRESH_SECONDS
            && count(layr.get("failures")) >= LAYR_FAULT_PROBES
            && last - since >= LAYR_FAULT_SECONDS)
            .then(|| {
                json!({"kind": "layr_unavailable", "since": since, "error": error,
                    "reason": format!("LAYR in the VM has not answered for {} seconds: {}",
                        (last - since) as u64, error.unwrap_or("no reason"))})
            })
    }

    /// Ask the root broker for layr health after a guest answer, at most once
    /// in [`LAYR_PROBE_SECONDS`] per boot. The watchdog only shows a layr
    /// fault: systemd restarts a dead daemon or broker, and a VM restart would
    /// end every provider.
    fn probe_layr(&self, pid: &Value) {
        let saved = self.read_json(HEALTH_FILE);
        if saved.get("helperPid") != Some(pid) {
            return;
        }
        let last = number(saved.get("layr").and_then(|layr| layr.get("lastProbeAt")));
        if last.is_some_and(|last| now() - last < LAYR_PROBE_SECONDS) {
            return;
        }
        let error = match self.guest.call(
            "layr.health",
            &json!({}),
            None,
            Duration::from_secs(LAYR_PROBE_WAIT_SECONDS),
        ) {
            Ok(health) if health.get("state").and_then(Value::as_str) == Some("ready") => None,
            Ok(health) => Some(format!(
                "layr state {}",
                match health.get("state") {
                    Some(Value::String(state)) => state.clone(),
                    Some(state) => state.to_string(),
                    None => health.to_string(),
                }
            )),
            Err(error) => Some(error.to_string()),
        };
        // The record of this boot again: a probe takes up to 20 seconds.
        let mut saved = self.read_json(HEALTH_FILE);
        if saved.get("helperPid") != Some(pid) {
            return;
        }
        let mut layr = saved
            .get("layr")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        record_result(
            &mut layr,
            error.as_deref(),
            now(),
            LAYR_PROBE_SECONDS + PROBE_GAP_SECONDS,
            "readyAt",
        );
        saved.insert("layr".into(), Value::Object(layr));
        self.save_health(saved);
    }

    /// Probe a running or unresponsive guest and record the result; true when
    /// it answered. One answer clears the unresponsive state.
    pub(super) fn probe(&self, status: &Map<String, Value>) -> bool {
        let error = self.ping();
        self.record_probe(status.get("pid"), error.as_deref());
        if error.is_none()
            && let Some(pid) = helper_pid(status.get("pid"))
        {
            self.probe_layr(pid);
        }
        error.is_none()
    }

    /// Open an episode for an unresponsive guest (`Some` status), or end the
    /// open one (`None`).
    pub(super) fn unresponsive_episode(&self, status: Option<&Map<String, Value>>) {
        let mut saved = self.restarts();
        let mut episodes: Vec<Value> = saved
            .get("unresponsive")
            .and_then(Value::as_array)
            .map(|rows| rows.iter().filter(|row| row.is_object()).cloned().collect())
            .unwrap_or_default();
        let open = episodes
            .last()
            .is_some_and(|episode| episode.get("endedAt").is_none_or(Value::is_null));
        let at = now();
        match status {
            None => {
                if !open {
                    return;
                }
                if let Some(episode) = episodes.last_mut() {
                    episode["endedAt"] = json!(at);
                }
            }
            Some(status) => {
                if open
                    && episodes
                        .last()
                        .is_some_and(|episode| episode.get("helperPid") == status.get("pid"))
                {
                    return;
                }
                episodes.push(json!({
                    "since": status.get("guestFailure").and_then(|failure| failure.get("since")),
                    "detectedAt": at, "time": utc(at),
                    "helperPid": status.get("pid"), "reason": status.get("error"),
                }));
            }
        }
        saved.insert("unresponsive".into(), Value::Array(episodes));
        self.save_restarts(saved);
    }

    /// The part of the watchdog records that the status shows.
    pub(super) fn summary(&self) -> Option<Value> {
        let saved = self.restarts();
        let restarts = saved
            .get("restarts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let held = saved.get("held").filter(|held| !held.is_null());
        let last_unresponsive = saved
            .get("unresponsive")
            .and_then(Value::as_array)
            .and_then(|rows| rows.last());
        if restarts.is_empty() && held.is_none() && last_unresponsive.is_none() {
            return None;
        }
        let at = now();
        let mut result = Map::new();
        result.insert(
            "restartsInLastDay".into(),
            json!(
                restarts
                    .iter()
                    .filter(|row| number(row.get("at")).is_some_and(|time| at - time < DAY_SECONDS))
                    .count()
            ),
        );
        if let Some(last) = restarts.last() {
            let fields = [
                "id",
                "time",
                "kind",
                "reason",
                "stage",
                "outcome",
                "error",
                "consoleLog",
            ];
            result.insert(
                "lastRestart".into(),
                Value::Object(
                    fields
                        .iter()
                        .map(|field| {
                            (
                                (*field).to_owned(),
                                last.get(*field).cloned().unwrap_or(Value::Null),
                            )
                        })
                        .collect(),
                ),
            );
        }
        if let Some(held) = held {
            result.insert("held".into(), held.clone());
        }
        if let Some(episode) = last_unresponsive {
            result.insert("lastUnresponsive".into(), episode.clone());
        }
        Some(Value::Object(result))
    }
}
