//! Adapters for tools that select tests themselves: vitest, jest and pytest-testmon.
//! layr-retest gives them the changed files (vitest, jest) and keeps the baseline record.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tool {
    Nextest,
    Vitest,
    Jest,
    Pytest,
}

impl Tool {
    pub fn parse(s: &str) -> Result<Tool> {
        Ok(match s {
            "nextest" => Tool::Nextest,
            "vitest" => Tool::Vitest,
            "jest" => Tool::Jest,
            "pytest" => Tool::Pytest,
            _ => bail!("unknown tool {s}; use nextest, vitest, jest or pytest"),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Tool::Nextest => "nextest",
            Tool::Vitest => "vitest",
            Tool::Jest => "jest",
            Tool::Pytest => "pytest",
        }
    }

    /// The command line of the tool: the environment variable `LAYR_RETEST_<TOOL>`, else the default.
    pub fn command(self, env: Option<String>) -> Vec<String> {
        let default = match self {
            Tool::Vitest => "npx vitest",
            Tool::Jest => "npx jest",
            _ => "pytest",
        };
        env.as_deref().unwrap_or(default).split_whitespace().map(str::to_string).collect()
    }

    pub fn env_var(self) -> String {
        format!("LAYR_RETEST_{}", self.name().to_uppercase())
    }
}

/// Command line for one run. `files` is `None` for a run of all tests (no baseline, or `--full`),
/// else the changed files that still exist. `None` back: nothing to run.
pub fn build(tool: Tool, cmd: &[String], files: Option<&[String]>, full: bool, extra: &[String]) -> Option<Vec<String>> {
    let mut a: Vec<String> = cmd.to_vec();
    match (tool, files) {
        (Tool::Pytest, _) => a.push(if full { "--testmon-noselect" } else { "--testmon" }.into()),
        (Tool::Vitest, None) => a.push("run".into()),
        (Tool::Vitest, Some([])) | (Tool::Jest, Some([])) => return None,
        (Tool::Vitest, Some(f)) => a.extend(["related".into(), "--run".into()].into_iter().chain(f.iter().cloned())),
        (Tool::Jest, Some(f)) => {
            a.extend(extra.iter().cloned());
            a.push("--findRelatedTests".into());
            a.extend(f.iter().cloned());
            return Some(a);
        }
        _ => {}
    }
    a.extend(extra.iter().cloned());
    Some(a)
}

/// Record of a passing run of one of these tools: `.layr-retest/<tool>/<state>.json`.
#[derive(Serialize, Deserialize, Debug)]
pub struct ToolRecord {
    pub tool: String,
    pub baseline: Option<String>,
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn vitest_arguments() {
        let c = Tool::Vitest.command(None);
        assert_eq!(c, v(&["npx", "vitest"]));
        assert_eq!(build(Tool::Vitest, &c, None, false, &v(&["--bail"])), Some(v(&["npx", "vitest", "run", "--bail"])));
        let f = v(&["src/a.ts", "src/b.ts"]);
        assert_eq!(
            build(Tool::Vitest, &c, Some(&f), false, &v(&["--bail"])),
            Some(v(&["npx", "vitest", "related", "--run", "src/a.ts", "src/b.ts", "--bail"]))
        );
        assert_eq!(build(Tool::Vitest, &c, Some(&[]), false, &[]), None);
    }

    #[test]
    fn jest_arguments() {
        let c = Tool::Jest.command(Some("pnpm exec jest".into()));
        assert_eq!(c, v(&["pnpm", "exec", "jest"]));
        assert_eq!(build(Tool::Jest, &c, None, true, &v(&["-i"])), Some(v(&["pnpm", "exec", "jest", "-i"])));
        let f = v(&["a.js"]);
        assert_eq!(
            build(Tool::Jest, &c, Some(&f), false, &v(&["-i"])),
            Some(v(&["pnpm", "exec", "jest", "-i", "--findRelatedTests", "a.js"]))
        );
        assert_eq!(build(Tool::Jest, &c, Some(&[]), false, &[]), None);
    }

    #[test]
    fn pytest_arguments() {
        let c = Tool::Pytest.command(None);
        assert_eq!(build(Tool::Pytest, &c, None, false, &v(&["-q"])), Some(v(&["pytest", "--testmon", "-q"])));
        assert_eq!(build(Tool::Pytest, &c, None, true, &[]), Some(v(&["pytest", "--testmon-noselect"])));
    }

    #[test]
    fn tool_names() {
        assert_eq!(Tool::parse("jest").unwrap(), Tool::Jest);
        assert!(Tool::parse("mocha").is_err());
        assert_eq!(Tool::Pytest.env_var(), "LAYR_RETEST_PYTEST");
    }
}
