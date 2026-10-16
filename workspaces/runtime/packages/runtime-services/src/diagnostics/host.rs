use std::collections::{BTreeMap, BTreeSet};

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

/// Read the reported host and process fields once, with a fixed process limit.
pub fn system_snapshot(root_pid: u32) -> (Option<Value>, Value) {
    let mut system = System::new();
    system.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
    system.refresh_cpu_list(CpuRefreshKind::nothing());
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_memory()
            .with_cpu()
            .without_tasks(),
    );

    let root = Pid::from_u32(root_pid);
    let mut rows = BTreeMap::new();
    if let Some(process) = system.process(root) {
        rows.insert(root_pid, process_snapshot(process));
        for (pid, process) in system.processes().iter().filter(|(pid, _)| **pid != root) {
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
    let mut descendants = BTreeSet::from([root_pid]);
    loop {
        let children = rows
            .iter()
            .filter_map(|(pid, row)| {
                (descendants.contains(&row.parent)
                    && !descendants.contains(pid)
                    && *pid != row.parent)
                    .then_some(*pid)
            })
            .take(PROCESS_LIMIT.saturating_sub(descendants.len()))
            .collect::<Vec<_>>();
        let before = descendants.len();
        descendants.extend(children);
        if descendants.len() == before || descendants.len() >= PROCESS_LIMIT {
            break;
        }
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

    #[test]
    fn classifier_uses_only_process_name_and_never_retains_arguments() {
        assert_eq!(process_kind("codex"), "codex_app_server");
        assert_eq!(process_kind("python3.13"), "python");
        assert_eq!(process_kind("unknown-private-token"), "other");
    }
}
