//! A short look at the working directory, for the request that needs it.
//!
//! Branch names and whether the worktree is clean. Not file names, not diffs.

use std::process::{Command, Stdio};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub cwd: String,
    pub git: Option<Git>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Git {
    pub branch: String,
    pub branches: Vec<String>,
    pub dirty: bool,
}

pub fn capture_text() -> String {
    format_context(&capture())
}

pub fn capture() -> Snapshot {
    let cwd = std::env::current_dir()
        .ok()
        .map(|path| path.display().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| ".".to_string());
    let git = if git(&["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true") {
        Some(read_git())
    } else {
        None
    };
    Snapshot { cwd, git }
}

pub fn format_context(snapshot: &Snapshot) -> String {
    let mut lines = vec![format!("Working directory: {}", snapshot.cwd)];
    match &snapshot.git {
        None => lines.push("Not a git repository.".to_string()),
        Some(git) => {
            lines.push(format!("Current branch: {}", git.branch));
            let branches = if git.branches.is_empty() {
                "(none)".to_string()
            } else {
                git.branches.join(", ")
            };
            lines.push(format!("Local branches: {branches}"));
            let work = if git.dirty {
                "uncommitted changes"
            } else {
                "clean"
            };
            lines.push(format!("Worktree: {work}"));
        }
    }
    lines.join("\n")
}

fn read_git() -> Git {
    let branch = git(&["branch", "--show-current"]).unwrap_or_default();
    let branch = if branch.is_empty() {
        "detached HEAD".to_string()
    } else {
        branch
    };
    let mut branches: Vec<String> =
        git(&["for-each-ref", "--format=%(refname:short)", "refs/heads"])
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .take(40)
            .map(str::to_string)
            .collect();
    if branches.len() == 40 {
        branches.push("…".to_string());
    }
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|text| !text.is_empty());
    Git {
        branch,
        branches,
        dirty,
    }
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repository_names_the_branch_and_a_plain_directory_does_not() {
        let repo = Snapshot {
            cwd: "/work/app".to_string(),
            git: Some(Git {
                branch: "test".to_string(),
                branches: vec!["dev".to_string(), "test".to_string()],
                dirty: false,
            }),
        };
        let text = format_context(&repo);
        assert!(text.contains("Working directory: /work/app"), "{text}");
        assert!(text.contains("Current branch: test"), "{text}");
        assert!(text.contains("Local branches: dev, test"), "{text}");
        assert!(text.contains("Worktree: clean"), "{text}");
        assert!(!text.contains("diff"), "{text}");

        let plain = Snapshot {
            cwd: "/tmp".to_string(),
            git: None,
        };
        let text = format_context(&plain);
        assert!(text.contains("Not a git repository."), "{text}");
        assert!(!text.contains("Current branch"), "{text}");
    }
}
