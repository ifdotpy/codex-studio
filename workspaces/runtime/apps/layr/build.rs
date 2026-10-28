use std::process::Command;

fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git(args: &[&str]) -> Option<String> {
    output("git", args)
}

fn main() {
    println!("cargo:rerun-if-env-changed=LAYR_BUILD_ID");
    for path in ["build.rs", "src", "crates", "Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={path}");
    }
    for path in ["HEAD", "index"] {
        if let Some(path) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&["rev-parse", "--git-path", &reference]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let build = std::env::var("LAYR_BUILD_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| git(&["describe", "--always", "--dirty", "--exclude", "*"]))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=LAYR_BUILD_ID={build}");
    // `layr version` shows the build identity (LAYR_BUILD_ID) and the day of the build.
    let date = output("date", &["+%Y-%m-%d"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=LAYR_BUILD_DATE={date}");
}
