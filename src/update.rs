//! Update check against the revision `install.sh` recorded.

use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const REPO: &str = "https://github.com/uxeric/ai-command.git";
const INSTALL_URL: &str = "https://raw.githubusercontent.com/uxeric/ai-command/HEAD/install.sh";
const CHECK_TIMEOUT: Duration = Duration::from_secs(8);

fn data_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .map(|dir| dir.join("cmd"))
}

pub fn installed_rev() -> Option<String> {
    let rev = std::fs::read_to_string(data_dir()?.join("installed-rev")).ok()?;
    let rev = rev.trim();
    (!rev.is_empty()).then(|| rev.to_string())
}

/// A short hash and the full hash of the same commit are not an update.
pub fn is_newer(installed: &str, remote: &str) -> bool {
    !installed.is_empty()
        && !remote.is_empty()
        && !remote.starts_with(installed)
        && !installed.starts_with(remote)
}

/// Ask before replacing this binary. Enter installs and continues in the new one.
/// A declined offer, a missing install record, or a check that cannot finish
/// leaves the current version running.
pub fn offer_if_newer() -> Result<(), String> {
    if cfg!(test) || std::env::var_os("CMD_NO_UPDATE_CHECK").is_some() {
        return Ok(());
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin())
        || !std::io::IsTerminal::is_terminal(&std::io::stderr())
    {
        return Ok(());
    }
    let Some(installed) = installed_rev() else {
        return Ok(());
    };
    let repo = std::env::var("CMD_REPO").unwrap_or_else(|_| REPO.to_string());
    let Some(remote) = remote_rev(&repo) else {
        return Ok(());
    };
    if !is_newer(&installed, &remote) {
        return Ok(());
    }
    let stdin = std::io::stdin();
    if !wants_update(
        &installed,
        &remote,
        &mut stdin.lock(),
        &mut std::io::stderr(),
        crate::style::stderr_color(),
    ) {
        return Ok(());
    }
    install_and_continue()
}

pub fn wants_update(
    installed: &str,
    remote: &str,
    input: &mut impl BufRead,
    mut err: impl Write,
    color: bool,
) -> bool {
    if color {
        let _ = writeln!(
            err,
            "{chip} a new version is ready.",
            chip = crate::style::CHIP
        );
        let _ = writeln!(
            err,
            "  {amber}{installed}{reset} {ice}→{reset} {amber}{remote}{reset}",
            amber = crate::style::AMBER,
            ice = crate::style::ICE,
            reset = crate::style::RESET,
        );
    } else {
        let _ = writeln!(err, "cmd: a new version is ready.");
        let _ = writeln!(err, "  {installed} → {remote}");
    }
    let _ = writeln!(err, "Press Enter to update, or n to keep this version.");
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) | Err(_) => return false,
        Ok(_) => {}
    }
    let answer = line.trim();
    answer.is_empty() || answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
}

fn remote_rev(repo: &str) -> Option<String> {
    let mut child = Command::new("git")
        .args(["ls-remote", repo, "HEAD"])
        .current_dir(std::env::temp_dir())
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait().ok()? {
            Some(status) if status.success() => break,
            Some(_) => return None,
            None if started.elapsed() > CHECK_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let sha = out.split_whitespace().next()?;
    (sha.len() >= 7).then(|| sha[..7].to_string())
}

pub fn run_installer() -> std::io::Result<ExitStatus> {
    let url = std::env::var("CMD_INSTALL_URL").unwrap_or_else(|_| INSTALL_URL.to_string());
    // The zsh helper captures stdout for the commit line. Installer output
    // goes to the terminal instead of becoming part of that line.
    let captured = !std::io::IsTerminal::is_terminal(&std::io::stdout());
    launch_installer(&url, captured)
}

fn launch_installer(url: &str, output_to_stderr: bool) -> std::io::Result<ExitStatus> {
    if url.contains('\'') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "installer URL contains a quote",
        ));
    }
    let command = if output_to_stderr {
        format!("curl -fsSL '{url}' | bash >&2")
    } else {
        format!("curl -fsSL '{url}' | bash")
    };
    Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::inherit())
        .status()
}

fn install_and_continue() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| {
        format!("could not find where cmd is installed ({error}); nothing was updated.")
    })?;
    match run_installer() {
        Ok(status) if status.success() => {}
        Ok(_) => {
            return Err(
                "the update did not finish; your previous version is still installed.".to_string(),
            );
        }
        Err(error) => {
            return Err(format!(
                "could not run the installer ({error}); nothing was updated."
            ));
        }
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    use std::os::unix::process::CommandExt;
    let error = Command::new(&exe).args(&args).exec();
    Err(format!(
        "updated, but could not restart {} ({error}). Run cmd again.",
        exe.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn a_different_commit_is_newer_and_a_shorter_hash_of_the_same_commit_is_not() {
        assert!(is_newer("40e5f66", "5a1b2c3"));
        assert!(!is_newer("40e5f66", "40e5f66"));
        assert!(
            !is_newer("40e5f661", "40e5f66"),
            "a longer local hash of the same commit"
        );
        assert!(!is_newer("40e5f66", "40e5f66a"));
        assert!(!is_newer("", "40e5f66"));
        assert!(!is_newer("40e5f66", ""));
    }

    #[test]
    fn enter_updates_and_n_keeps_this_version() {
        let mut shown = Vec::new();
        assert!(wants_update(
            "abc1234",
            "def5678",
            &mut Cursor::new("\n"),
            &mut shown,
            false
        ));
        let text = String::from_utf8(shown).unwrap();
        assert!(text.contains("abc1234"), "{text}");
        assert!(text.contains("def5678"), "{text}");
        assert!(text.contains("Press Enter to update"), "{text}");
        assert!(text.contains("n to keep this version"), "{text}");
        assert!(text.contains("cmd: a new version is ready."), "{text}");

        let mut colored = Vec::new();
        assert!(
            !wants_update(
                "abc1234",
                "def5678",
                &mut Cursor::new("n\n"),
                &mut colored,
                true
            ),
            "n still keeps this version when the prompt is colored"
        );
        let colored = String::from_utf8(colored).unwrap();
        assert!(colored.contains("a new version is ready."), "{colored}");
        assert!(colored.contains("48;2;255;42;109"), "hot chip: {colored}");
        assert!(
            colored.contains("38;2;246;196;69"),
            "amber revisions: {colored}"
        );
        assert!(colored.contains("38;2;41;240;255"), "ice arrow: {colored}");
        assert!(
            !colored.contains("cmd:"),
            "the chip replaces the plain prefix: {colored}"
        );

        let silent = std::io::sink;
        assert!(wants_update(
            "a",
            "b",
            &mut Cursor::new("y\n"),
            &mut silent(),
            false
        ));
        assert!(wants_update(
            "a",
            "b",
            &mut Cursor::new("yes\n"),
            &mut silent(),
            false
        ));
        assert!(!wants_update(
            "a",
            "b",
            &mut Cursor::new("n\n"),
            &mut silent(),
            false
        ));
        assert!(!wants_update(
            "a",
            "b",
            &mut Cursor::new("no\n"),
            &mut silent(),
            false
        ));
        assert!(!wants_update(
            "a",
            "b",
            &mut Cursor::new("later\n"),
            &mut silent(),
            false
        ));
        assert!(
            !wants_update("a", "b", &mut Cursor::new(""), &mut silent(), false),
            "a closed prompt does not update"
        );
    }

    #[test]
    fn the_remote_revision_is_the_repository_head() {
        let dir = std::env::temp_dir().join(format!("cmd-rev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "cmd")
                .env("GIT_AUTHOR_EMAIL", "cmd@example.com")
                .env("GIT_COMMITTER_NAME", "cmd")
                .env("GIT_COMMITTER_EMAIL", "cmd@example.com")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{args:?} {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "--quiet"]);
        std::fs::write(dir.join("note"), "hello\n").unwrap();
        run(&["add", "note"]);
        run(&["commit", "--quiet", "-m", "note"]);
        let head = Command::new("git")
            .args(["rev-parse", "--short=7", "HEAD"])
            .current_dir(&dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        let expected = String::from_utf8(head.stdout).unwrap();
        let found = remote_rev(dir.to_str().unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found.as_deref(), Some(expected.trim()));
    }

    #[test]
    fn the_installer_script_runs_on_a_terminal_and_when_stdout_is_captured() {
        let dir = std::env::temp_dir().join(format!("cmd-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for output_to_stderr in [false, true] {
            let name = if output_to_stderr {
                "captured"
            } else {
                "shown"
            };
            let marker = dir.join(name);
            let script = dir.join(format!("{name}.sh"));
            std::fs::write(&script, format!("touch '{}'\n", marker.display())).unwrap();
            let url = format!("file://{}", script.display());
            let status = launch_installer(&url, output_to_stderr).unwrap();
            assert!(status.success(), "{name}");
            assert!(marker.is_file(), "{name}: the installer script did not run");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
