//! What the model can know about this machine.
//!
//! A short baseline always: the directory, the git refs a command could name,
//! and how the words in the request are spelled here. Heavier facts wait until
//! the model asks. Nothing in this module changes the machine.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const SHOW: usize = 40;
const STORE: usize = 400;
const PROBE_LIST: usize = 200;

/// id, what it answers, whether it only makes sense in a git repository.
const PROBES: &[(&str, &str, bool)] = &[
    ("git.status", "changed files, staged and unstaged", true),
    (
        "git.refs",
        "branch and tag names past the lists above",
        true,
    ),
    ("files.here", "names in the working directory", false),
    (
        "project.scripts",
        "scripts and targets this directory already defines",
        false,
    ),
    ("services", "running systemd services", false),
    ("docker", "running containers", false),
];

const TOOLS: &[&str] = &[
    "apt",
    "bat",
    "brew",
    "bun",
    "bundle",
    "cargo",
    "composer",
    "curl",
    "docker",
    "eza",
    "fd",
    "fdfind",
    "git",
    "go",
    "jq",
    "just",
    "kubectl",
    "lsd",
    "make",
    "npm",
    "pacman",
    "php",
    "pip",
    "pnpm",
    "podman",
    "psql",
    "python",
    "python3",
    "rg",
    "ruby",
    "sqlite3",
    "systemctl",
    "uv",
    "wget",
    "yarn",
];

const MARKERS: &[&str] = &[
    "CMakeLists.txt",
    "Cargo.toml",
    "Dockerfile",
    "Gemfile",
    "Makefile",
    "Pipfile",
    "build.gradle",
    "build.gradle.kts",
    "composer.json",
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
    "flake.nix",
    "go.mod",
    "justfile",
    "Justfile",
    "mix.exs",
    "package.json",
    "pom.xml",
    "pyproject.toml",
    "requirements.txt",
];

const STOP: &[&str] = &[
    "a",
    "add",
    "after",
    "again",
    "all",
    "also",
    "an",
    "and",
    "as",
    "at",
    "back",
    "before",
    "blame",
    "both",
    "branch",
    "branches",
    "build",
    "by",
    "checkout",
    "cherry",
    "clean",
    "clone",
    "command",
    "commands",
    "commit",
    "copy",
    "create",
    "current",
    "delete",
    "did",
    "diff",
    "directory",
    "do",
    "does",
    "drop",
    "edit",
    "fetch",
    "file",
    "files",
    "find",
    "folder",
    "for",
    "from",
    "git",
    "give",
    "here",
    "how",
    "in",
    "init",
    "install",
    "into",
    "it",
    "its",
    "just",
    "keep",
    "list",
    "local",
    "log",
    "make",
    "me",
    "merge",
    "move",
    "my",
    "need",
    "no",
    "not",
    "now",
    "of",
    "off",
    "on",
    "onto",
    "open",
    "or",
    "our",
    "out",
    "over",
    "pick",
    "place",
    "please",
    "print",
    "prune",
    "pull",
    "push",
    "put",
    "rebase",
    "remote",
    "remove",
    "rename",
    "repo",
    "repository",
    "reset",
    "restore",
    "revert",
    "run",
    "search",
    "shell",
    "show",
    "start",
    "stash",
    "status",
    "stop",
    "switch",
    "tag",
    "tell",
    "that",
    "the",
    "then",
    "there",
    "these",
    "this",
    "those",
    "to",
    "tracking",
    "up",
    "update",
    "use",
    "using",
    "via",
    "want",
    "with",
    "without",
    "working",
    "yes",
    "your",
];

pub(crate) struct Dossier {
    cwd: PathBuf,
    git: Option<GitFacts>,
    project: Vec<String>,
    tools: Vec<String>,
    names: Vec<String>,
    extra: Vec<String>,
    seen: BTreeSet<String>,
}

struct GitFacts {
    root: PathBuf,
    branch: String,
    sha: Option<String>,
    upstream: Option<String>,
    ahead: Option<u32>,
    behind: Option<u32>,
    local: Vec<String>,
    remote: Vec<String>,
    tags: Vec<String>,
    clipped: bool,
    dirty: bool,
}

impl Dossier {
    pub(crate) fn capture_in(dir: &Path, request: &str, notes: &[String]) -> Self {
        let cwd = dir.to_path_buf();
        let git = read_git(&cwd);
        let root = git.as_ref().map(|git| git.root.clone());
        let project = project_files(&cwd, root.as_deref());
        let tools = tools_from_env();
        let names = resolve_names(request, notes, git.as_ref(), &cwd);
        Self {
            cwd,
            git,
            project,
            tools,
            names,
            extra: Vec::new(),
            seen: BTreeSet::new(),
        }
    }

    pub(crate) fn text(&self) -> String {
        let mut parts = Vec::new();
        let mut head = vec![format!("Working directory: {}", self.cwd.display())];
        match &self.git {
            None => head.push("Not a git repository.".into()),
            Some(git) => head.extend(git.lines()),
        }
        if !self.project.is_empty() {
            head.push(format!("Project files: {}", self.project.join(", ")));
        }
        head.push(tools_line(&self.tools));
        parts.push(head.join("\n"));
        if !self.names.is_empty() {
            let mut block = String::from("Names:\n");
            block.push_str(&self.names.join("\n"));
            parts.push(block);
        }
        parts.extend(self.extra.iter().cloned());
        let catalog = self.catalog();
        if !catalog.is_empty() {
            parts.push(catalog);
        }
        parts.join("\n\n")
    }

    /// Run probes the model asked for. False when every id was already answered.
    pub(crate) fn grant(&mut self, ids: &[String]) -> bool {
        let mut fresh = false;
        for id in ids.iter().take(8) {
            if !self.seen.insert(id.clone()) {
                continue;
            }
            fresh = true;
            let body = if PROBES.iter().any(|(name, _, _)| *name == id) {
                self.probe(id)
            } else {
                "not available".to_string()
            };
            self.extra.push(format!("{id}:\n{body}"));
        }
        fresh
    }

    fn catalog(&self) -> String {
        let mut lines = Vec::new();
        for (id, detail, git_only) in PROBES {
            if *git_only && self.git.is_none() {
                continue;
            }
            if self.seen.contains(*id) {
                continue;
            }
            lines.push(format!("{id} — {detail}"));
        }
        if lines.is_empty() {
            return String::new();
        }
        format!("You can ask for:\n{}", lines.join("\n"))
    }

    fn probe(&self, id: &str) -> String {
        match id {
            "git.status" => self.git_status(),
            "git.refs" => self.git_refs(),
            "files.here" => files_here(&self.cwd),
            "project.scripts" => {
                project_scripts(&self.cwd, self.git.as_ref().map(|git| git.root.as_path()))
            }
            "services" => services(),
            "docker" => containers(),
            _ => "not available".into(),
        }
    }

    fn git_status(&self) -> String {
        if self.git.is_none() {
            return "Not a git repository.".into();
        }
        match git_output(&self.cwd, &["status", "--porcelain=v1", "-b"]) {
            Some(text) if text.is_empty() => "nothing to commit".into(),
            Some(text) => clip_lines(&text, SHOW),
            None => "git status failed".into(),
        }
    }

    fn git_refs(&self) -> String {
        let Some(git) = &self.git else {
            return "Not a git repository.".into();
        };
        if !git.clipped
            && git.local.len() <= SHOW
            && git.remote.len() <= SHOW
            && git.tags.len() <= SHOW
        {
            return "The lists above are complete.".into();
        }
        let mut lines = vec![
            listed("Local branches", &git.local, PROBE_LIST),
            listed("Remote branches", &git.remote, PROBE_LIST),
        ];
        if !git.tags.is_empty() {
            lines.push(listed("Tags", &git.tags, PROBE_LIST));
        }
        if git.clipped {
            lines.push("Still truncated.".into());
        }
        lines.join("\n")
    }
}

impl GitFacts {
    fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!("Git root: {}", self.root.display())];
        lines.push(match &self.sha {
            Some(sha) => format!("Current branch: detached HEAD at {sha}"),
            None => format!("Current branch: {}", self.branch),
        });
        lines.push(self.upstream_line());
        lines.push(listed("Local branches", &self.local, SHOW));
        lines.push(listed("Remote branches", &self.remote, SHOW));
        if !self.tags.is_empty() {
            lines.push(listed("Tags", &self.tags, SHOW));
        }
        let work = if self.dirty {
            "uncommitted changes"
        } else {
            "clean"
        };
        lines.push(format!("Worktree: {work}"));
        lines
    }

    fn upstream_line(&self) -> String {
        let Some(name) = &self.upstream else {
            return "Upstream: none".into();
        };
        match (self.ahead, self.behind) {
            (Some(ahead), Some(behind)) => {
                format!("Upstream: {name}, {ahead} ahead, {behind} behind")
            }
            _ => format!("Upstream: {name}"),
        }
    }
}

fn read_git(dir: &Path) -> Option<GitFacts> {
    if git_output(dir, &["rev-parse", "--is-inside-work-tree"]).as_deref() != Some("true") {
        return None;
    }
    let root = PathBuf::from(git_output(dir, &["rev-parse", "--show-toplevel"])?);
    let current = git_output(dir, &["branch", "--show-current"]).unwrap_or_default();
    let (branch, sha) = if current.is_empty() {
        (
            "detached HEAD".to_string(),
            git_output(dir, &["rev-parse", "--short", "HEAD"]),
        )
    } else {
        (current, None)
    };
    let (local, remote, tags, clipped) = refs(dir);
    let upstream = git_output(
        dir,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .filter(|name| !name.is_empty());
    let (ahead, behind) = match &upstream {
        Some(_) => ahead_behind(dir),
        None => (None, None),
    };
    let dirty = git_output(dir, &["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|text| !text.is_empty());
    Some(GitFacts {
        root,
        branch,
        sha,
        upstream,
        ahead,
        behind,
        local,
        remote,
        tags,
        clipped,
        dirty,
    })
}

fn refs(dir: &Path) -> (Vec<String>, Vec<String>, Vec<String>, bool) {
    let Some(text) = git_output(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
    ) else {
        return (Vec::new(), Vec::new(), Vec::new(), false);
    };
    let mut local = Vec::new();
    let mut remote = Vec::new();
    let mut tags = Vec::new();
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("refs/heads/") {
            if !name.is_empty() {
                local.push(name.to_string());
            }
        } else if let Some(name) = line.strip_prefix("refs/remotes/") {
            if !name.is_empty() && !name.ends_with("/HEAD") {
                remote.push(name.to_string());
            }
        } else if let Some(name) = line.strip_prefix("refs/tags/") {
            if !name.is_empty() {
                tags.push(name.to_string());
            }
        }
    }
    local.sort();
    remote.sort();
    tags.sort();
    let clipped = trim_store(&mut local) || trim_store(&mut remote) || trim_store(&mut tags);
    (local, remote, tags, clipped)
}

fn trim_store(names: &mut Vec<String>) -> bool {
    if names.len() > STORE {
        names.truncate(STORE);
        true
    } else {
        false
    }
}

fn ahead_behind(dir: &Path) -> (Option<u32>, Option<u32>) {
    let Some(text) = git_output(
        dir,
        &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
    ) else {
        return (None, None);
    };
    let mut parts = text.split_whitespace();
    let ahead = parts.next().and_then(|text| text.parse().ok());
    let behind = parts.next().and_then(|text| text.parse().ok());
    (ahead, behind)
}

fn listed(title: &str, names: &[String], cap: usize) -> String {
    if names.is_empty() {
        return format!("{title}: (none)");
    }
    let shown = names
        .iter()
        .take(cap)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > cap {
        format!("{title}: {shown}, …")
    } else {
        format!("{title}: {shown}")
    }
}

fn resolve_names(
    request: &str,
    notes: &[String],
    git: Option<&GitFacts>,
    cwd: &Path,
) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut lines = Vec::new();
    let mut consider = |text: &str| {
        for word in text.split_whitespace() {
            let Some(token) = clean_token(word) else {
                continue;
            };
            if !seen.insert(token.clone()) {
                continue;
            }
            if let Some(line) = explain_name(&token, git, cwd) {
                lines.push(line);
            }
        }
    };
    consider(request);
    for note in notes {
        consider(note);
    }
    lines
}

fn clean_token(word: &str) -> Option<String> {
    let token = word.trim_matches(|ch: char| {
        matches!(
            ch,
            ',' | ';' | ':' | '!' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | '`'
        )
    });
    let token = token
        .strip_suffix('.')
        .filter(|rest| !rest.is_empty())
        .unwrap_or(token);
    if token.chars().count() < 2
        || token.chars().count() > 200
        || token.chars().all(|ch| ch.is_ascii_digit())
        || token.chars().any(char::is_control)
        || is_stopword(token)
    {
        return None;
    }
    Some(token.to_string())
}

fn is_stopword(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    STOP.binary_search(&lower.as_str()).is_ok()
}

fn explain_name(token: &str, git: Option<&GitFacts>, cwd: &Path) -> Option<String> {
    let local = git.is_some_and(|git| git.local.iter().any(|name| name == token));
    let current = local && git.is_some_and(|git| git.branch == token);
    let mut remotes: Vec<&str> = git
        .map(|git| {
            git.remote
                .iter()
                .filter(|name| name.as_str() == token || remote_branch(name) == token)
                .map(String::as_str)
                .collect()
        })
        .unwrap_or_default();
    remotes.sort_unstable();
    remotes.dedup();
    let tag = git.is_some_and(|git| git.tags.iter().any(|name| name == token));
    let here = path_kind(cwd, token);
    let at_root = git.and_then(|git| {
        if git.root == cwd {
            None
        } else {
            path_kind(&git.root, token)
        }
    });
    if !local && remotes.is_empty() && !tag && here.is_none() && at_root.is_none() {
        return None;
    }

    let mut clauses = Vec::new();
    if local && current {
        clauses.push("local branch, and it is the current branch".to_string());
    } else if local {
        clauses.push("local branch".to_string());
    } else if !remotes.is_empty() || tag {
        clauses.push("no local branch".to_string());
    }
    if remotes.len() == 1 {
        clauses.push(format!("Remote-tracking branch {}", remotes[0]));
    } else if remotes.len() > 1 {
        let shown: Vec<&str> = remotes.iter().copied().take(5).collect();
        let mut list = join_and(&shown);
        if remotes.len() > 5 {
            list.push_str(", …");
        }
        clauses.push(format!("Remote-tracking branches {list}"));
    }
    if tag {
        clauses.push("tag".to_string());
    }
    if let Some(kind) = here {
        clauses.push(format!("{kind} in the working directory"));
    }
    if let Some(kind) = at_root {
        clauses.push(format!("{kind} at the git root"));
    }

    let spell = if local {
        Some(token.to_string())
    } else if remotes.len() == 1 && !tag {
        Some(remotes[0].to_string())
    } else if tag && remotes.is_empty() {
        Some(token.to_string())
    } else {
        None
    };

    let body = clauses
        .iter()
        .map(|clause| sentence(clause))
        .collect::<Vec<_>>()
        .join(" ");
    let mut line = format!("{token}: {body}");
    if let Some(spell) = spell {
        if current {
            line.push_str(&format!(
                " Use the name {spell}, and leave a checkout of it out."
            ));
        } else {
            line.push_str(&format!(" Use the name {spell}."));
        }
    }
    Some(line)
}

fn remote_branch(short: &str) -> &str {
    short
        .split_once('/')
        .map(|(_, branch)| branch)
        .unwrap_or(short)
}

fn join_and(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_string(),
        [first, second] => format!("{first} and {second}"),
        _ => {
            let (last, rest) = names.split_last().unwrap();
            format!("{}, and {last}", rest.join(", "))
        }
    }
}

fn sentence(clause: &str) -> String {
    let mut chars = clause.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out: String = first.to_uppercase().collect();
    out.push_str(chars.as_str());
    if !out.ends_with('.') {
        out.push('.');
    }
    out
}

fn path_kind(dir: &Path, token: &str) -> Option<&'static str> {
    if token.is_empty()
        || token.starts_with('/')
        || token.starts_with('~')
        || token.split(['/', '\\']).any(|part| part == "..")
    {
        return None;
    }
    let meta = dir.join(token).symlink_metadata().ok()?;
    if meta.file_type().is_symlink() {
        Some("symlink")
    } else if meta.is_dir() {
        Some("directory")
    } else if meta.is_file() {
        Some("file")
    } else {
        None
    }
}

fn project_files(cwd: &Path, root: Option<&Path>) -> Vec<String> {
    let mut found = Vec::new();
    for name in MARKERS {
        if cwd.join(name).is_file() {
            found.push((*name).to_string());
        } else if root.is_some_and(|root| root != cwd && root.join(name).is_file()) {
            found.push(format!("{name} (at the git root)"));
        }
    }
    found
}

fn project_scripts(cwd: &Path, root: Option<&Path>) -> String {
    let mut blocks = Vec::new();
    collect_scripts(&mut blocks, cwd);
    if let Some(root) = root {
        if root != cwd {
            collect_scripts(&mut blocks, root);
        }
    }
    if blocks.is_empty() {
        "none found".into()
    } else {
        blocks.join("\n")
    }
}

fn collect_scripts(blocks: &mut Vec<String>, dir: &Path) {
    if let Some(names) = json_keys(&dir.join("package.json"), "scripts") {
        blocks.push(format!("package.json: {}", names.join(", ")));
    }
    if let Some(names) = json_keys(&dir.join("composer.json"), "scripts") {
        blocks.push(format!("composer.json: {}", names.join(", ")));
    }
    for name in ["Makefile", "makefile", "GNUmakefile"] {
        if let Some(targets) = make_targets(&dir.join(name)) {
            blocks.push(format!("{name}: {}", targets.join(", ")));
            break;
        }
    }
    for name in ["justfile", "Justfile"] {
        if let Some(recipes) = make_targets(&dir.join(name)) {
            blocks.push(format!("{name}: {}", recipes.join(", ")));
            break;
        }
    }
    if dir.join("Cargo.toml").is_file() {
        blocks.push("Cargo.toml: cargo build, cargo test, cargo run".into());
    }
    if pyproject_has_pytest(dir) {
        blocks.push("pyproject.toml: pytest".into());
    }
}

fn json_keys(path: &Path, field: &str) -> Option<Vec<String>> {
    let text = read_capped(path, 1_000_000)?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let object = value.get(field)?.as_object()?;
    let mut names: Vec<String> = object.keys().cloned().collect();
    if names.is_empty() {
        return None;
    }
    names.sort();
    names.truncate(SHOW);
    Some(names)
}

fn make_targets(path: &Path) -> Option<Vec<String>> {
    let text = read_capped(path, 256_000)?;
    let mut names = Vec::new();
    for line in text.lines() {
        if line.starts_with([' ', '\t', '#', '.']) {
            continue;
        }
        if line.contains(":=") || line.contains("?=") || line.contains("::=") {
            continue;
        }
        let Some((name, _)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty()
            || name.contains([' ', '%', '$'])
            || !name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.')
        {
            continue;
        }
        if !names.iter().any(|seen: &String| seen == name) {
            names.push(name.to_string());
        }
        if names.len() == SHOW {
            break;
        }
    }
    if names.is_empty() {
        None
    } else {
        Some(names)
    }
}

fn pyproject_has_pytest(dir: &Path) -> bool {
    read_capped(&dir.join("pyproject.toml"), 512_000)
        .is_some_and(|text| text.contains("[tool.pytest"))
}

fn read_capped(path: &Path, max: usize) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut handle = file.take(max as u64 + 1);
    let mut buf = String::new();
    handle.read_to_string(&mut buf).ok()?;
    if buf.len() > max {
        return None;
    }
    Some(buf)
}

fn files_here(dir: &Path) -> String {
    let Ok(read) = fs::read_dir(dir) else {
        return "the working directory could not be read".into();
    };
    let mut names = Vec::new();
    for entry in read.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let suffix = match entry.file_type() {
            Ok(kind) if kind.is_dir() => "/",
            _ => "",
        };
        names.push(format!("{name}{suffix}"));
    }
    if names.is_empty() {
        return "(empty)".into();
    }
    names.sort();
    let more = names.len() > 80;
    names.truncate(80);
    let mut text = names.join("\n");
    if more {
        text.push_str("\n…");
    }
    text
}

fn services() -> String {
    if !on_path("systemctl") {
        return "systemctl is not on PATH".into();
    }
    match run_bounded(
        "systemctl",
        &[
            "list-units",
            "--type=service",
            "--state=running",
            "--no-legend",
            "--plain",
            "--no-pager",
        ],
        Duration::from_secs(2),
    ) {
        Some(text) if !text.is_empty() => clip_lines(&text, SHOW),
        Some(_) => "no running services".into(),
        None => "no reply from systemctl".into(),
    }
}

fn containers() -> String {
    if !on_path("docker") {
        return "docker is not on PATH".into();
    }
    match run_bounded(
        "docker",
        &["ps", "--format", "{{.Names}}"],
        Duration::from_secs(2),
    ) {
        Some(text) if !text.is_empty() => clip_lines(&text, SHOW),
        Some(_) => "no running containers".into(),
        None => "no reply from docker".into(),
    }
}

fn tools_from_env() -> Vec<String> {
    match std::env::var_os("PATH") {
        Some(path) => tools_in(&path),
        None => Vec::new(),
    }
}

fn tools_line(tools: &[String]) -> String {
    if tools.is_empty() {
        "On PATH: none of the watched commands".into()
    } else {
        format!("On PATH: {}", tools.join(", "))
    }
}

fn tools_in(path_var: &std::ffi::OsStr) -> Vec<String> {
    TOOLS
        .iter()
        .filter(|name| command_in_path(name, path_var))
        .map(|name| (*name).to_string())
        .collect()
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| command_in_path(name, &path))
}

fn command_in_path(name: &str, path_var: &std::ffi::OsStr) -> bool {
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(name);
        let Ok(meta) = candidate.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return true;
    }
    false
}

fn clip_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max {
        return text.trim_end().to_string();
    }
    let mut out = lines[..max].join("\n");
    out.push_str("\n…");
    out
}

fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
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
    let text = String::from_utf8_lossy(&output.stdout);
    Some(text.trim_end().to_string())
}

fn run_bounded(program: &str, args: &[&str], limit: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let buf = reader.join().unwrap_or_default();
    let status = status?;
    if !status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&buf);
    Some(text.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static SCRATCH: AtomicU64 = AtomicU64::new(0);

    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            let n = SCRATCH.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("aicmd-{n}-{nanos}"));
            fs::create_dir(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .arg("-c")
            .arg("user.email=cmd@example.com")
            .arg("-c")
            .arg("user.name=cmd")
            .arg("-c")
            .arg("commit.gpgsign=false")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// On `test`, with `origin/dev` and no local `dev`. This is the merge that failed.
    fn remote_only_dev() -> Scratch {
        let dir = Scratch::new();
        git(&dir.path, &["init", "-b", "test"]);
        fs::write(dir.path.join("README.md"), "hi\n").unwrap();
        git(&dir.path, &["add", "README.md"]);
        git(&dir.path, &["commit", "-m", "init"]);
        git(&dir.path, &["branch", "main"]);
        git(
            &dir.path,
            &["update-ref", "refs/remotes/origin/dev", "HEAD"],
        );
        git(
            &dir.path,
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        git(
            &dir.path,
            &["update-ref", "refs/remotes/origin/test", "HEAD"],
        );
        git(
            &dir.path,
            &["update-ref", "refs/remotes/origin/HEAD", "HEAD"],
        );
        git(&dir.path, &["config", "branch.test.remote", "origin"]);
        git(
            &dir.path,
            &["config", "branch.test.merge", "refs/heads/test"],
        );
        let url = dir.path.to_str().expect("scratch path").to_string();
        git(&dir.path, &["remote", "add", "origin", &url]);
        dir
    }

    fn line<'a>(text: &'a str, prefix: &str) -> &'a str {
        text.lines()
            .find(|line| line.starts_with(prefix))
            .unwrap_or_else(|| panic!("missing {prefix} in:\n{text}"))
    }

    #[test]
    fn stopwords_are_sorted() {
        for pair in STOP.windows(2) {
            assert!(pair[0] < pair[1], "{} then {}", pair[0], pair[1]);
        }
    }

    #[test]
    fn a_remote_only_branch_is_spelled_with_its_remote() {
        let dir = remote_only_dev();
        let text = Dossier::capture_in(&dir.path, "git command to merge dev into test", &[]).text();
        assert!(text.contains("Current branch: test"), "{text}");
        assert!(
            text.contains("Upstream: origin/test, 0 ahead, 0 behind"),
            "{text}"
        );
        assert!(text.contains("Worktree: clean"), "{text}");
        let local = line(&text, "Local branches:");
        assert!(local.contains("main") && local.contains("test"), "{local}");
        assert!(
            !local.split(',').map(str::trim).any(|name| name == "dev"),
            "{local}"
        );
        let remote = line(&text, "Remote branches:");
        assert!(remote.contains("origin/dev"), "{remote}");
        assert!(
            !remote
                .split(',')
                .map(str::trim)
                .any(|name| name == "origin/HEAD"),
            "{remote}"
        );
        assert!(
            text.contains(
                "dev: No local branch. Remote-tracking branch origin/dev. Use the name origin/dev."
            ),
            "{text}"
        );
        assert!(
            text.contains("test: Local branch, and it is the current branch. Remote-tracking branch origin/test. Use the name test, and leave a checkout of it out."),
            "{text}"
        );
        assert!(text.contains("You can ask for:"), "{text}");
        assert!(text.contains("git.status"), "{text}");
    }

    #[test]
    fn a_local_branch_wins_over_the_remote_of_the_same_name() {
        let dir = remote_only_dev();
        git(&dir.path, &["branch", "dev"]);
        let text = Dossier::capture_in(&dir.path, "merge dev into test", &[]).text();
        assert!(
            text.contains(
                "dev: Local branch. Remote-tracking branch origin/dev. Use the name dev."
            ),
            "{text}"
        );
    }

    #[test]
    fn two_remotes_with_the_same_branch_are_both_named() {
        let dir = remote_only_dev();
        git(
            &dir.path,
            &["update-ref", "refs/remotes/upstream/dev", "HEAD"],
        );
        let text = Dossier::capture_in(&dir.path, "merge dev into test", &[]).text();
        assert!(
            text.contains(
                "dev: No local branch. Remote-tracking branches origin/dev and upstream/dev."
            ),
            "{text}"
        );
        assert!(!text.contains("Use the name origin/dev."), "{text}");
    }

    #[test]
    fn a_name_past_the_printed_list_is_still_resolved() {
        let dir = remote_only_dev();
        for n in 0..50 {
            git(
                &dir.path,
                &[
                    "update-ref",
                    &format!("refs/remotes/origin/b{n:02}"),
                    "HEAD",
                ],
            );
        }
        git(
            &dir.path,
            &["update-ref", "refs/remotes/origin/zzz", "HEAD"],
        );
        let text = Dossier::capture_in(&dir.path, "merge zzz into test", &[]).text();
        let remote = line(&text, "Remote branches:");
        assert!(remote.contains('…'), "{remote}");
        assert!(
            !remote.contains("origin/zzz"),
            "the printed list is capped, so the name line has to carry it:\n{remote}"
        );
        assert!(
            text.contains(
                "zzz: No local branch. Remote-tracking branch origin/zzz. Use the name origin/zzz."
            ),
            "{text}"
        );
        let mut dossier = Dossier::capture_in(&dir.path, "merge zzz into test", &[]);
        assert!(dossier.grant(&["git.refs".into()]));
        let full = dossier.text();
        assert!(full.contains("origin/zzz"), "{full}");
    }

    #[test]
    fn a_clarification_can_name_a_branch_the_request_did_not() {
        let dir = remote_only_dev();
        git(&dir.path, &["branch", "hotfix"]);
        let text = Dossier::capture_in(
            &dir.path,
            "merge dev into test",
            &["use hotfix instead of dev".into()],
        )
        .text();
        assert!(
            text.contains("hotfix: Local branch. Use the name hotfix."),
            "{text}"
        );
    }

    #[test]
    fn an_edited_tracked_file_dirties_the_worktree_and_an_untracked_file_does_not() {
        let dir = remote_only_dev();
        fs::write(dir.path.join("notes.txt"), "new\n").unwrap();
        let clean = Dossier::capture_in(&dir.path, "merge dev into test", &[]).text();
        assert!(clean.contains("Worktree: clean"), "{clean}");
        fs::write(dir.path.join("README.md"), "changed\n").unwrap();
        let dirty = Dossier::capture_in(&dir.path, "merge dev into test", &[]).text();
        assert!(dirty.contains("Worktree: uncommitted changes"), "{dirty}");
    }

    #[test]
    fn a_plain_directory_has_no_git_facts_and_can_be_asked_for_its_files() {
        let dir = Scratch::new();
        fs::write(dir.path.join("notes.txt"), "hi\n").unwrap();
        let mut dossier = Dossier::capture_in(&dir.path, "print the working directory", &[]);
        let text = dossier.text();
        assert!(text.contains("Not a git repository."), "{text}");
        assert!(!text.contains("Current branch"), "{text}");
        assert!(!text.contains("git.status"), "{text}");
        assert!(text.contains("files.here —"), "{text}");
        assert!(dossier.grant(&["files.here".into()]));
        let listed = dossier.text();
        assert!(listed.contains("notes.txt"), "{listed}");
        assert_eq!(listed.matches("files.here:").count(), 1, "{listed}");
        assert!(!dossier.grant(&["files.here".into()]));
        assert!(dossier.grant(&["rm-rf".into()]));
        assert!(dossier.text().contains("rm-rf:\nnot available"), "{listed}");
    }

    #[test]
    fn project_markers_and_scripts_come_from_the_directory() {
        let dir = Scratch::new();
        fs::write(dir.path.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
        fs::write(
            dir.path.join("package.json"),
            r#"{"scripts":{"dev":"vite","test":"vitest"}}"#,
        )
        .unwrap();
        fs::write(
            dir.path.join("Makefile"),
            "FOO := bar\n\nbuild:\n\techo hi\n\ntest:\n\techo test\n",
        )
        .unwrap();
        fs::write(dir.path.join("compose.yaml"), "services: {}\n").unwrap();
        let mut dossier = Dossier::capture_in(&dir.path, "run the tests", &[]);
        let text = dossier.text();
        assert!(text.contains("Cargo.toml"), "{text}");
        assert!(text.contains("package.json"), "{text}");
        assert!(text.contains("compose.yaml"), "{text}");
        assert!(dossier.grant(&["project.scripts".into()]));
        let scripts = dossier.text();
        assert!(scripts.contains("package.json: dev, test"), "{scripts}");
        assert!(scripts.contains("Makefile: build, test"), "{scripts}");
        assert!(
            scripts.contains("Cargo.toml: cargo build, cargo test, cargo run"),
            "{scripts}"
        );
        assert!(!scripts.contains("echo hi"), "{scripts}");
        assert!(!scripts.contains("FOO"), "{scripts}");
    }

    #[test]
    fn a_subdirectory_still_sees_the_repository_root() {
        let dir = remote_only_dev();
        fs::write(dir.path.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
        let src = dir.path.join("src");
        fs::create_dir(&src).unwrap();
        let text = Dossier::capture_in(&src, "open README.md", &[]).text();
        assert!(text.contains("Current branch: test"), "{text}");
        assert!(text.contains("Cargo.toml (at the git root)"), "{text}");
        assert!(text.contains("README.md: File at the git root."), "{text}");
    }

    #[test]
    fn a_file_named_in_the_request_is_reported() {
        let dir = remote_only_dev();
        let text = Dossier::capture_in(&dir.path, "open README.md", &[]).text();
        assert!(
            text.contains("README.md: File in the working directory."),
            "{text}"
        );
    }

    #[test]
    fn tools_are_the_watched_commands_that_are_executable() {
        let dir = Scratch::new();
        let rg = dir.path.join("rg");
        fs::write(&rg, "#!/bin/sh\n").unwrap();
        let mut perms = fs::metadata(&rg).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
        }
        fs::set_permissions(&rg, perms).unwrap();
        let note = dir.path.join("git");
        fs::write(&note, "not executable\n").unwrap();
        let found = tools_in(dir.path.as_os_str());
        assert_eq!(found, vec!["rg".to_string()], "{found:?}");
    }
}
