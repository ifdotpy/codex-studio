use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Mutex, PoisonError};

use serde_json::{Value, json};
use sysinfo::{
    CpuRefreshKind, MemoryRefreshKind, Pid, Process, ProcessRefreshKind, ProcessesToUpdate, System,
};

const PROCESS_LIMIT: usize = 8192;
const MIB: f64 = 1024.0 * 1024.0;

#[derive(Clone, Copy)]
struct ProcessSnapshot {
    parent: u32,
    rss_bytes: u64,
    cpu_percent: f64,
    kind: &'static str,
}

/// The sampler outlives requests: a process's CPU percent is the change since its
/// previous refresh, so a fresh `System` per request would always report 0.
static SAMPLER: Mutex<Option<System>> = Mutex::new(None);

/// Process ids of `root` and its descendants, read from the kernel's per-thread
/// `children` lists so that only this server's own tree is visited. `None` when the
/// kernel does not expose them (or off Linux); the caller then scans every process.
#[cfg(target_os = "linux")]
fn own_tree_pids(root: u32) -> Option<Vec<u32>> {
    fn children(pid: u32, out: &mut Vec<u32>) -> std::io::Result<()> {
        for task in std::fs::read_dir(format!("/proc/{pid}/task"))? {
            let list = std::fs::read_to_string(task?.path().join("children"))?;
            out.extend(
                list.split_ascii_whitespace()
                    .filter_map(|p| p.parse::<u32>().ok()),
            );
        }
        Ok(())
    }
    let mut pids = vec![root];
    let mut seen = HashSet::from([root]);
    let mut next = 0;
    while next < pids.len() && pids.len() < PROCESS_LIMIT {
        let mut found = Vec::new();
        match children(pids[next], &mut found) {
            Ok(()) => {}
            // The root's own list must exist; a child may exit while the tree is walked.
            Err(_) if next > 0 => {}
            Err(_) => return None,
        }
        for pid in found {
            if pids.len() < PROCESS_LIMIT && seen.insert(pid) {
                pids.push(pid);
            }
        }
        next += 1;
    }
    Some(pids)
}

#[cfg(not(target_os = "linux"))]
fn own_tree_pids(_root: u32) -> Option<Vec<u32>> {
    None
}

/// Read the reported host and process fields once, with a fixed process limit.
pub fn system_snapshot(root_pid: u32) -> (Option<Value>, Value) {
    let mut sampler = SAMPLER.lock().unwrap_or_else(PoisonError::into_inner);
    let system = sampler.get_or_insert_with(System::new);
    system.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
    system.refresh_cpu_list(CpuRefreshKind::nothing());
    let own_tree = own_tree_pids(root_pid);
    let listed_pids = own_tree
        .as_ref()
        .map(|pids| pids.iter().copied().collect::<HashSet<_>>());
    let refresh = ProcessRefreshKind::nothing()
        .with_memory()
        .with_cpu()
        .without_tasks();
    match &own_tree {
        Some(pids) => {
            let pids = pids
                .iter()
                .map(|pid| Pid::from_u32(*pid))
                .collect::<Vec<_>>();
            system.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), true, refresh);
        }
        None => {
            system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        }
    }

    let root = Pid::from_u32(root_pid);
    let mut rows = BTreeMap::new();
    if let Some(process) = system.process(root) {
        rows.insert(root_pid, process_snapshot(process));
        let listed = |pid: &Pid| {
            listed_pids
                .as_ref()
                .is_none_or(|pids| pids.contains(&pid.as_u32()))
        };
        for (pid, process) in system
            .processes()
            .iter()
            .filter(|(pid, _)| **pid != root && listed(pid))
        {
            if rows.len() >= PROCESS_LIMIT {
                break;
            }
            rows.insert(pid.as_u32(), process_snapshot(process));
        }
    }
    let process_tree = build_process_tree(root_pid, &rows);
    let cpu_count = system.cpus().len();
    let total_memory = system.total_memory();
    let available_memory = system.available_memory();
    let resources = json!({
        "cpuCount": (cpu_count > 0).then_some(cpu_count),
        "totalMemoryBytes": (total_memory > 0).then_some(total_memory),
        "availableMemoryBytes": (available_memory > 0).then_some(available_memory),
    });
    (process_tree, resources)
}

fn process_snapshot(process: &Process) -> ProcessSnapshot {
    ProcessSnapshot {
        parent: process.parent().map(|pid| pid.as_u32()).unwrap_or_default(),
        rss_bytes: process.memory(),
        cpu_percent: f64::from(process.cpu_usage()),
        kind: process_kind(&process.name().to_string_lossy()),
    }
}

fn build_process_tree(root_pid: u32, rows: &BTreeMap<u32, ProcessSnapshot>) -> Option<Value> {
    if !rows.contains_key(&root_pid) {
        return None;
    }
    // One index of children by parent: a level costs its own rows, so a deep
    // tree is not a scan of every row per level.
    let mut children = BTreeMap::<u32, Vec<u32>>::new();
    for (pid, row) in rows {
        if *pid != row.parent {
            children.entry(row.parent).or_default().push(*pid);
        }
    }
    // Level by level from the root; each level is taken in PID order up to the limit.
    let mut descendants = BTreeSet::from([root_pid]);
    let mut level = vec![root_pid];
    while !level.is_empty() && descendants.len() < PROCESS_LIMIT {
        let mut next = level
            .iter()
            .filter_map(|parent| children.get(parent))
            .flatten()
            .copied()
            .filter(|pid| !descendants.contains(pid))
            .collect::<Vec<_>>();
        next.sort_unstable();
        next.dedup();
        next.truncate(PROCESS_LIMIT - descendants.len());
        descendants.extend(next.iter().copied());
        level = next;
    }

    let mut processes = Vec::with_capacity(descendants.len());
    let mut kinds = BTreeMap::<&str, (u64, u64, f64)>::new();
    let mut total_rss = 0_u64;
    for pid in descendants {
        let Some(row) = rows.get(&pid) else { continue };
        processes.push(json!({
            "pid": pid,
            "parentPid": row.parent,
            "kind": row.kind,
            "rssMiB": round_one(row.rss_bytes as f64 / MIB),
            "cpuPercent": round_one(row.cpu_percent),
        }));
        total_rss = total_rss.saturating_add(row.rss_bytes);
        let totals = kinds.entry(row.kind).or_default();
        totals.0 = totals.0.saturating_add(1);
        totals.1 = totals.1.saturating_add(row.rss_bytes);
        totals.2 += row.cpu_percent;
    }
    let kinds = kinds
        .into_iter()
        .map(|(kind, (count, rss, cpu))| {
            (
                kind.to_owned(),
                json!({
                    "count": count,
                    "rssMiB": round_one(rss as f64 / MIB),
                    "cpuPercent": round_one(cpu),
                }),
            )
        })
        .collect::<serde_json::Map<String, Value>>();
    Some(json!({
        "rootPid": root_pid,
        "processes": processes,
        "kinds": kinds,
        "totalRssMiB": round_one(total_rss as f64 / MIB),
    }))
}

pub fn process_kind(name: &str) -> &'static str {
    if name.contains("codex-code-mode-host") {
        "code_mode_host"
    } else if name == "codex" {
        "codex_app_server"
    } else if name == "claude" {
        "claude_cli"
    } else if name.contains("node_repl") {
        "node_repl"
    } else if name == "anarlog" {
        "anarlog_mcp"
    } else if name.contains("codex-canvas") {
        "studio_backend"
    } else if name.starts_with("chrome-headless") {
        "headless_browser"
    } else if name.to_ascii_lowercase().starts_with("python") {
        "python"
    } else if name == "node" {
        "node"
    } else if matches!(name, "sh" | "bash" | "zsh") {
        "shell"
    } else {
        "other"
    }
}

fn round_one(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

pub fn current_process_id() -> u32 {
    std::process::id()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_tree_is_bounded_and_keeps_root_first() {
        let rows = (1..=PROCESS_LIMIT as u32 + 200)
            .map(|pid| {
                (
                    pid,
                    ProcessSnapshot {
                        parent: if pid == 1 { 0 } else { pid - 1 },
                        rss_bytes: 1024,
                        cpu_percent: 0.0,
                        kind: "other",
                    },
                )
            })
            .collect();
        let tree = build_process_tree(1, &rows).expect("root exists");
        assert_eq!(
            tree["processes"].as_array().map(Vec::len),
            Some(PROCESS_LIMIT)
        );
        assert_eq!(tree["processes"][0]["pid"], 1);
        assert_eq!(tree["rootPid"], 1);
    }

    /// The tree holds every descendant at any depth, once, and no process
    /// outside it, including one that names itself as its parent.
    #[test]
    fn process_tree_holds_descendants_only() {
        let row = |parent| ProcessSnapshot {
            parent,
            rss_bytes: 1024,
            cpu_percent: 0.0,
            kind: "other",
        };
        let rows = BTreeMap::from([
            (1, row(0)),
            (10, row(1)),
            (11, row(10)),
            (12, row(11)),
            (5, row(10)),
            (20, row(2)),
            (2, row(2)),
        ]);
        let tree = build_process_tree(10, &rows).expect("root exists");
        let pids = tree["processes"]
            .as_array()
            .expect("process rows")
            .iter()
            .map(|row| row["pid"].as_u64().expect("pid"))
            .collect::<Vec<_>>();
        assert_eq!(pids.len(), 4, "each process once: {pids:?}");
        assert_eq!(BTreeSet::from_iter(pids), BTreeSet::from([5, 10, 11, 12]));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn snapshot_covers_this_process_and_its_children_only() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn a child");
        let root = std::process::id();
        let started = std::time::Instant::now();
        let (tree, _) = system_snapshot(root);
        eprintln!("own-tree snapshot took {:?}", started.elapsed());
        let tree = tree.expect("this process is in its own tree");
        let pids = tree["processes"]
            .as_array()
            .expect("process rows")
            .iter()
            .map(|row| row["pid"].as_u64().expect("pid"))
            .collect::<Vec<_>>();
        child.kill().expect("stop the child");
        child.wait().expect("reap the child");
        assert_eq!(pids[0], u64::from(root));
        assert!(pids.contains(&u64::from(child.id())));
        assert!(
            !pids.contains(&1),
            "unrelated host processes stay out of the tree"
        );
    }

    #[test]
    fn classifier_uses_only_process_name_and_never_retains_arguments() {
        assert_eq!(process_kind("codex"), "codex_app_server");
        assert_eq!(process_kind("python3.13"), "python");
        assert_eq!(process_kind("unknown-private-token"), "other");
    }
}
