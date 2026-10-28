//! layr-retest: run only the tests that a change can affect. See docs/retest.md.
//! Uses the public `layr` CLI, `cargo nextest` and the LLVM tools. No internal crate of layr.

mod deps;
mod parse;
mod record;
mod select;
mod tools;

use anyhow::{bail, Context, Result};
use record::{test_key, FileCov, Record, TestRec};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tools::Tool;

/// Folder of the raw profiles of one run. The wrapper reads it from this variable.
const PROFILE_DIR_VAR: &str = "LAYR_RETEST_PROFILE_DIR";
/// Number of parallel coverage exports.
const EXPORT_WORKERS: usize = 4;
/// Files that no record keeps: the standard library and build output. Cargo's own folders stay:
/// they name the packages that a test used.
const IGNORE_FILES: &str = r"(/\.rustup/|/rustc/|/target/)";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("wrap") => wrap(&args[1..]),
        Some("run") => run(&args[1..]),
        _ => {
            eprintln!("usage: layr-retest run [--full] [-- <nextest args>]\n       layr-retest wrap -- <test binary> <args>");
            std::process::exit(2);
        }
    };
    match r {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("layr-retest: {e:#}");
            std::process::exit(1);
        }
    }
}

/// The one owner of the folder of the raw profiles of a test. Every process that the test starts
/// writes its own file there (`%p` is the process id).
fn profile_dir(dir: &Path, binary_id: &str, test: &str) -> PathBuf {
    let clean = |s: &str| -> String {
        s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' { c } else { '_' }).collect()
    };
    dir.join(clean(binary_id)).join(clean(test))
}

/// Cargo target runner. In the run phase nextest sets the test name; the list phase has none.
fn wrap(args: &[String]) -> Result<i32> {
    let args = args.strip_prefix(&["--".to_string()]).unwrap_or(args);
    let Some((prog, rest)) = args.split_first() else { bail!("wrap: no test binary") };
    let mut cmd = Command::new(prog);
    cmd.args(rest);
    if let (Ok(dir), Ok(bin), Ok(test)) =
        (std::env::var(PROFILE_DIR_VAR), std::env::var("NEXTEST_BINARY_ID"), std::env::var("NEXTEST_TEST_NAME"))
    {
        let p = profile_dir(Path::new(&dir), &bin, &test);
        std::fs::create_dir_all(&p)?;
        cmd.env("LLVM_PROFILE_FILE", p.join("%p.profraw"));
    }
    Err(cmd.exec()).with_context(|| format!("cannot start {prog}"))
}

fn output(cmd: &mut Command) -> Result<Vec<u8>> {
    let o = cmd.output().with_context(|| format!("cannot start {:?}", cmd.get_program()))?;
    if !o.status.success() {
        bail!("{:?} failed: {}", cmd.get_program(), String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(o.stdout)
}

fn layr(args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&output(Command::new("layr").args(args))?).into_owned())
}

fn state_id(rev: &str) -> Result<String> {
    let out = layr(&["log", "--json", "-n", "1", rev])?;
    let v: Value = serde_json::from_str(out.lines().next().context("no such state")?)?;
    Ok(v["id"].as_str().context("state without id")?.to_string())
}

/// Save the working copy if it changed. Returns the id of the state to record, and the id of the
/// head when the working copy equals the head (a new line can have the head, not an auto state).
fn current_state() -> Result<(String, Option<String>)> {
    let out = layr(&["save"])?;
    let status: Value = serde_json::from_str(&layr(&["status", "--json"])?)?;
    let clean =
        ["conflicts", "staged", "unstaged", "untracked"].iter().all(|k| status[k].as_array().is_some_and(|a| a.is_empty()));
    let head = if clean { Some(state_id("head")?) } else { None };
    let id32 = out.split_whitespace().last().filter(|w| w.len() == 32 && w.chars().all(|c| c.is_ascii_hexdigit()));
    // "No changes" means the working copy is the head, or the newest auto state when it differs from the head.
    let current = match (id32, &head) {
        (Some(id), _) => state_id(id)?,
        (None, Some(h)) => h.clone(),
        (None, None) => state_id("head@{0}")?,
    };
    Ok((current, head))
}

/// Newest state in the line's log (autos included) that has a record.
fn find_baseline(root: &Path, tool: Tool) -> Result<Option<String>> {
    for l in layr(&["log", "--json", "--auto"])?.lines() {
        let v: Value = serde_json::from_str(l)?;
        let has = |id: &str| match tool {
            Tool::Nextest => record::has_record(root, id),
            t => record::tool_record_path(root, t.name(), id).is_file(),
        };
        if let Some(id) = v["id"].as_str().filter(|id| has(id)) {
            return Ok(Some(id.to_string()));
        }
    }
    Ok(None)
}

struct Suite {
    binary_id: String,
    /// Prefix of the test names in nextest's JSON events.
    prefix: String,
    path: PathBuf,
    tests: Vec<String>,
}

fn parse_list(json: &[u8]) -> Result<Vec<Suite>> {
    let v: Value = serde_json::from_slice(json)?;
    let mut out = Vec::new();
    for s in v["rust-suites"].as_object().context("no suites in the test list")?.values() {
        let tests =
            s["testcases"].as_object().map(|m| m.iter().filter(|(_, t)| t["ignored"] != true).map(|(n, _)| n.clone()).collect());
        out.push(Suite {
            binary_id: s["binary-id"].as_str().unwrap_or_default().to_string(),
            prefix: format!(
                "{}::{}",
                s["package-name"].as_str().unwrap_or_default(),
                s["binary-name"].as_str().unwrap_or_default()
            ),
            path: PathBuf::from(s["binary-path"].as_str().unwrap_or_default()),
            tests: tests.unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Escape a literal for a nextest filter expression.
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace(')', "\\)").replace(',', "\\,")
}

fn filter_expr(selected: &[(String, String)]) -> String {
    let mut by_bin: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (b, t) in selected {
        by_bin.entry(b).or_default().push(t);
    }
    by_bin
        .iter()
        .map(|(b, ts)| {
            let t: Vec<String> = ts.iter().map(|t| format!("test(={})", esc(t))).collect();
            format!("(binary_id(={}) & ({}))", esc(b), t.join(" | "))
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn rustc_host() -> Result<String> {
    let out = String::from_utf8(output(Command::new("rustc").arg("-vV"))?)?;
    out.lines().find_map(|l| l.strip_prefix("host: ")).map(str::to_string).context("no host in rustc -vV")
}

fn llvm_tool(name: &str) -> Result<PathBuf> {
    let sysroot = String::from_utf8(output(Command::new("rustc").args(["--print", "sysroot"]))?)?;
    let p = Path::new(sysroot.trim()).join("lib/rustlib").join(rustc_host()?).join("bin").join(name);
    if !p.is_file() {
        bail!("{} not found; run: rustup component add llvm-tools", p.display());
    }
    Ok(p)
}

/// Files and packages that one test executed.
type Exported = (BTreeMap<String, FileCov>, Vec<String>);

/// Function tables per test binary, built on first use. The key says whether the other bin
/// targets were exported with it.
#[derive(Default)]
struct Tables(Mutex<BTreeMap<(PathBuf, bool), Arc<OnceLock<Result<parse::FuncTable, String>>>>>);

/// `llvm-cov export` of `bin` (and `others`, when given) with the profile `data`.
fn cov_export(tools: &(PathBuf, PathBuf), bin: &Path, others: &[PathBuf], data: &Path, fmt: &[&str]) -> Command {
    let mut c = Command::new(&tools.1);
    c.arg("export").arg(bin).arg(format!("-instr-profile={}", data.display()));
    c.args(others.iter().flat_map(|o| ["-object".as_ref(), o.as_os_str()]));
    c.arg(format!("-ignore-filename-regex={IGNORE_FILES}")).args(fmt);
    c
}

/// Coverage of one test, from its raw profile and its test binary.
fn export(root: &Path, bin: &Path, others: &[PathBuf], dir: &Path, tools: &(PathBuf, PathBuf), tables: &Tables) -> Result<Exported> {
    let raws: Vec<PathBuf> =
        std::fs::read_dir(dir)?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "profraw")).collect();
    let data = dir.join("merged.profdata");
    output(Command::new(&tools.0).args(["merge", "-sparse"]).args(&raws).arg("-o").arg(&data))?;
    // One profile file means that the test process was the only process with instrumented code.
    // Another file is from a child process, which may have run another bin target of the project.
    let others = if raws.len() > 1 { others } else { &[] };
    let rel = |p: &str| -> Option<String> {
        let canon = root.canonicalize().ok();
        let p = Path::new(p);
        let r = p.strip_prefix(root).ok().or_else(|| canon.as_deref().and_then(|c| p.strip_prefix(c).ok()))?;
        Some(r.to_string_lossy().into_owned())
    };
    let lcov = String::from_utf8_lossy(&output(&mut cov_export(tools, bin, others, &data, &["-format=lcov", "-skip-functions"]))?).into_owned();
    // Function ranges do not depend on the test; the profile only says which functions ran.
    let ran = parse::parse_covered(&String::from_utf8_lossy(&output(
        Command::new(&tools.0).args(["show", "--covered", "--all-functions"]).arg(&data),
    )?));
    let cell = tables.0.lock().unwrap().entry((bin.to_path_buf(), !others.is_empty())).or_default().clone();
    let table = cell
        .get_or_init(|| {
            let json = output(&mut cov_export(tools, bin, others, &data, &["-format=text", "-skip-expansions"]));
            json.and_then(|j| parse::parse_function_table(&j, |f| rel(f).is_some())).map_err(|e| format!("{e:#}"))
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!("function table of {}: {e}", bin.display()))?;
    let funcs = table.classify(&ran);
    let _ = std::fs::remove_file(&data);
    let mut files: BTreeMap<String, FileCov> = BTreeMap::new();
    let mut packages = std::collections::BTreeSet::new();
    for (f, lines) in parse::parse_lcov(&lcov) {
        if let Some(r) = rel(&f) {
            files.entry(r).or_default().lines = lines;
        } else if let Some(p) = deps::package_of(&f).filter(|_| !lines.is_empty()) {
            packages.insert(p);
        }
    }
    for (f, (funcs, idle)) in funcs {
        if let Some(r) = rel(&f) {
            let e = files.entry(r).or_default();
            e.funcs = funcs;
            e.idle = idle;
        }
    }
    files.retain(|_, c| !c.lines.is_empty() || !c.funcs.is_empty());
    Ok((files, packages.into_iter().collect()))
}

/// The bin targets of the workspace, built instrumented in `<target>/debug/<name>`.
fn bin_objects(target: &Path) -> Result<Vec<PathBuf>> {
    let meta: Value =
        serde_json::from_slice(&output(Command::new("cargo").args(["metadata", "--no-deps", "--format-version", "1"]))?)?;
    let mut out = Vec::new();
    for t in meta["packages"].as_array().into_iter().flatten().flat_map(|p| p["targets"].as_array().into_iter().flatten()) {
        let is_bin = t["kind"].as_array().is_some_and(|k| k.iter().any(|k| k == "bin"));
        let path = target.join("debug").join(t["name"].as_str().unwrap_or_default());
        if is_bin && path.is_file() {
            out.push(path);
        }
    }
    Ok(out)
}

/// Delete `.profraw` files outside `target/` that appeared since `since`. Returns how many.
fn remove_stray_profiles(dir: &Path, since: std::time::SystemTime) -> usize {
    let mut n = 0;
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        let name = e.file_name();
        if p.is_dir() && !p.is_symlink() {
            if name != "target" && name != ".git" {
                n += remove_stray_profiles(&p, since);
            }
        } else if p.extension().is_some_and(|x| x == "profraw")
            && p.metadata().and_then(|m| m.modified()).is_ok_and(|t| t >= since)
            && std::fs::remove_file(&p).is_ok()
        {
            n += 1;
        }
    }
    n
}

/// Run vitest, jest or pytest-testmon. They select the tests; we give them the changed files and
/// keep the record of the last passing state.
fn run_tool(tool: Tool, root: &Path, full: bool, extra: &[String]) -> Result<i32> {
    let (current, head) = current_state()?;
    let baseline = if tool == Tool::Pytest || full { None } else { find_baseline(root, tool)? };
    let files: Option<Vec<String>> = match &baseline {
        Some(b) if *b != current => Some(
            layr(&["diff", b, &current, "--name-only"])?
                .lines()
                .filter(|f| !f.starts_with(record::RECORD_DIR) && root.join(f).is_file())
                .map(str::to_string)
                .collect(),
        ),
        Some(_) => Some(Vec::new()),
        None => None,
    };
    let cmd = tool.command(std::env::var(tool.env_var()).ok());
    let mut code = 0;
    match tools::build(tool, &cmd, files.as_deref(), full, extra) {
        None => eprintln!("layr-retest: no changed file since the baseline, nothing to run"),
        Some(args) => {
            eprintln!("layr-retest: {}", args.join(" "));
            let mut c = Command::new(&args[0]);
            c.args(&args[1..]).current_dir(root);
            if tool == Tool::Pytest {
                std::fs::create_dir_all(root.join(record::RECORD_DIR))?;
                c.env("TESTMON_DATAFILE", root.join(record::RECORD_DIR).join("testmondata"));
            }
            code = c.status().with_context(|| format!("cannot start {}", args[0]))?.code().unwrap_or(1);
        }
    }
    if code != 0 {
        // No pass record: the next run starts again from the last passing baseline.
        return Ok(code);
    }
    let rec = tools::ToolRecord { tool: tool.name().into(), baseline: baseline.clone(), status: "pass".into() };
    for id in std::iter::once(&current).chain(head.as_ref().filter(|h| **h != current)) {
        let p = record::tool_record_path(root, tool.name(), id);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, serde_json::to_vec(&rec)?)?;
    }
    layr(&["save", "--caches"])?;
    Ok(0)
}

fn run(args: &[String]) -> Result<i32> {
    let (mine, extra) = match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], &args[i + 1..]),
        None => (args, &args[args.len()..]),
    };
    let full = mine.iter().any(|a| a == "--full");
    let mut tool = Tool::Nextest;
    let mut it = mine.iter().filter(|a| *a != "--full");
    while let Some(a) = it.next() {
        match a.as_str() {
            "--tool" => tool = Tool::parse(it.next().context("--tool needs a name")?)?,
            bad => bail!("unknown option {bad}"),
        }
    }
    let root = std::env::current_dir()?;
    if tool != Tool::Nextest {
        return run_tool(tool, &root, full, extra);
    }
    let t0 = std::time::Instant::now();
    let started = std::time::SystemTime::now();

    let (current, head) = current_state()?;
    let baseline = find_baseline(&root, Tool::Nextest)?;
    let base_rec = baseline.as_deref().map(|b| record::load(&root, b)).transpose()?;
    let changes = match baseline.as_deref() {
        Some(b) if b != current => parse::parse_hunks(&layr(&["diff", b, &current, "-U0"])?),
        _ => Vec::new(),
    };

    let ext = match baseline.as_deref() {
        Some(b) if b != current && deps::Ext::lock_changed(&changes) => {
            let lock = |s: &str| layr(&["show", &format!("{s}:Cargo.lock")]).unwrap_or_default();
            deps::Ext::new(&changes, &lock(b), &lock(&current))
        }
        _ => deps::Ext::new(&changes, "", ""),
    };

    // Build the instrumented tests and list them.
    let target = root.join("target/retest");
    let profdir = target.join("profraw");
    let _ = std::fs::remove_dir_all(&profdir);
    let exe = std::env::current_exe()?;
    let runner_var = format!("CARGO_TARGET_{}_RUNNER", rustc_host()?.to_uppercase().replace('-', "_"));
    let flags = format!("{} -C instrument-coverage", std::env::var("RUSTFLAGS").unwrap_or_default());
    let nextest = |sub: &[&str]| {
        let mut c = Command::new("cargo");
        c.arg("nextest").args(sub).args(extra);
        // Cargo, build scripts and the list phase write here; the wrapper gives each test its own folder.
        c.env("LLVM_PROFILE_FILE", profdir.join("other/%p.profraw"));
        c.env("RUSTFLAGS", flags.trim()).env("CARGO_TARGET_DIR", &target).env(PROFILE_DIR_VAR, &profdir);
        c.env(&runner_var, format!("{} wrap --", exe.display())).env("NEXTEST_EXPERIMENTAL_LIBTEST_JSON", "1");
        c
    };
    let list = output(nextest(&["list", "--message-format", "json"]).stderr(Stdio::inherit()))?;
    let suites = parse_list(&list)?;

    // Select.
    let mut all: Vec<(String, String)> = Vec::new();
    let mut stale: Vec<(String, String)> = Vec::new();
    // Entries of unchanged tests, moved to the lines of the new state.
    let mut carried: BTreeMap<String, TestRec> = BTreeMap::new();
    for s in &suites {
        for t in &s.tests {
            all.push((s.binary_id.clone(), t.clone()));
            let old = base_rec.as_ref().and_then(|r| r.tests.get(&test_key(&s.binary_id, t)));
            match old.filter(|_| !full).and_then(|r| select::carry(r, &changes, &ext)) {
                Some(r) => {
                    carried.insert(test_key(&s.binary_id, t), r);
                }
                None => stale.push((s.binary_id.clone(), t.clone())),
            }
        }
    }
    let note = if full {
        "full run"
    } else if baseline.is_none() {
        "no record, full run"
    } else {
        "incremental"
    };
    eprintln!("layr-retest: {} tests, {} selected, {} skipped ({note})", all.len(), stale.len(), all.len() - stale.len());

    // Run.
    let mut results: BTreeMap<String, bool> = BTreeMap::new();
    let mut code = 0;
    if !stale.is_empty() {
        let mut cmd = nextest(&["run", "--no-fail-fast", "--message-format", "libtest-json"]);
        if stale.len() < all.len() {
            cmd.args(["-E", &filter_expr(&stale)]);
        }
        let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().context("cannot start cargo nextest")?;
        for l in BufReader::new(child.stdout.take().unwrap()).lines() {
            let Ok(v) = serde_json::from_str::<Value>(&l?) else { continue };
            let (Some("test"), Some(ev), Some(name)) = (v["type"].as_str(), v["event"].as_str(), v["name"].as_str()) else {
                continue;
            };
            if ev == "started" || ev == "ignored" {
                continue;
            }
            let Some((prefix, test)) = name.split_once('$') else { continue };
            // A lib and a bin of one package share a prefix; the test name tells them apart.
            if let Some(s) = suites.iter().find(|s| s.prefix == prefix && s.tests.iter().any(|t| t == test)) {
                results.insert(test_key(&s.binary_id, test), ev == "ok");
            }
        }
        code = child.wait()?.code().unwrap_or(1);
        if results.is_empty() && code != 0 {
            return Ok(code);
        }
    }

    // Export coverage of the tests that ran.
    let tools = (llvm_tool("llvm-profdata")?, llvm_tool("llvm-cov")?);
    let bins = bin_objects(&target)?;
    let jobs: Vec<(String, &Suite, &str)> = suites
        .iter()
        .flat_map(|s| s.tests.iter().map(move |t| (test_key(&s.binary_id, t), s, t.as_str())))
        .filter(|(k, ..)| results.contains_key(k))
        .collect();
    let next = AtomicUsize::new(0);
    let tables = Tables::default();
    let done: Mutex<BTreeMap<String, Result<Exported>>> = Mutex::new(BTreeMap::new());
    std::thread::scope(|sc| {
        for _ in 0..EXPORT_WORKERS {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some((key, s, test)) = jobs.get(i) else { break };
                let dir = profile_dir(&profdir, &s.binary_id, test);
                // A test that died before it wrote a profile has no coverage.
                let r = if dir.is_dir() { export(&root, &s.path, &bins, &dir, &tools, &tables) } else { Ok(Default::default()) };
                done.lock().unwrap().insert(key.clone(), r);
            });
        }
    });
    let mut done = done.into_inner().unwrap();

    // Write the record. Tests that did not run keep their old entry.
    let mut rec = Record { baseline: if full { None } else { baseline.clone() }, tests: BTreeMap::new() };
    if baseline.as_deref() == Some(current.as_str()) {
        rec.baseline = base_rec.as_ref().and_then(|r| r.baseline.clone());
    }
    let mut contradictions = 0;
    for (b, t) in &all {
        let key = test_key(b, t);
        let old = base_rec.as_ref().and_then(|r| r.tests.get(&key));
        let kept = carried.remove(&key);
        match (results.get(&key), done.remove(&key)) {
            (Some(&ok), Some(exported)) => {
                let (files, deps) = exported?;
                if full && !ok && old.is_some_and(|o| o.status == "pass") {
                    contradictions += 1;
                    eprintln!(
                        "layr-retest: CONTRADICTION: {key} failed in this full run, but the record of state {} says pass",
                        baseline.as_deref().unwrap_or("?")
                    );
                }
                rec.tests.insert(key, TestRec { status: if ok { "pass" } else { "fail" }.into(), files, deps });
            }
            _ => {
                if let Some(o) = kept {
                    rec.tests.insert(key, o);
                }
            }
        }
    }
    let stray = remove_stray_profiles(&root, started);
    if stray > 0 {
        eprintln!("layr-retest: removed {stray} stray .profraw file(s) from the project: a process with a cleared environment ran instrumented code, so that coverage is lost");
    }
    record::save(&root, &current, &rec)?;
    if let Some(h) = head.filter(|h| *h != current) {
        record::save(&root, &h, &rec)?;
    }
    // The record folder is a cache path of the line; this makes it a state of the cache overlay,
    // which a new line and `layr merge` take (docs/retest.md, "Records in a cache overlay").
    layr(&["save", "--caches"])?;
    let _ = std::fs::remove_dir_all(&profdir);
    let failed = results.values().filter(|ok| !**ok).count();
    eprintln!(
        "layr-retest: ran {}, passed {}, failed {}, skipped {}; record {}; {:.1}s{}",
        results.len(),
        results.len() - failed,
        failed,
        all.len() - stale.len(),
        current,
        t0.elapsed().as_secs_f32(),
        if contradictions > 0 { format!("; {contradictions} contradiction(s)") } else { String::new() }
    );
    Ok(code)
}
