//! Put the commit command on the terminal prompt without running it.

use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use serde_json::Value;

const TERMINAL_MARKERS: &[&str] = &[
    "ghostty",
    "kitty",
    "alacritty",
    "foot",
    "wezterm",
    "warp",
    "xterm",
    "konsole",
    "gnome-terminal",
    "terminal",
    "kgx",
    "console",
    "tilix",
    "terminator",
    "hyper",
    "tabby",
    "rio",
    "contour",
    "blackbox",
    "ptyxis",
    "st",
];

struct ActiveWindow {
    address: String,
    class: String,
    initial_class: String,
}

pub fn deliver(command: &str, print_only: bool) {
    if print_only || !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        println!("{command}");
        return;
    }
    let Some(window) = active_window() else {
        eprintln!("Could not see the focused window, so the command is printed here.");
        println!("{command}");
        return;
    };
    if !is_terminal(&window.class) && !is_terminal(&window.initial_class) {
        eprintln!("The focused window is not a terminal, so the command is printed here.");
        println!("{command}");
        return;
    }
    if !wtype_installed() {
        eprintln!("wtype is not installed, so the command is printed here.");
        println!("{command}");
        return;
    }
    match schedule(&window.address, command) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("Could not type into the terminal ({error}).");
            println!("{command}");
        }
    }
}

/// Runs in a detached process, after the parent has exited and the prompt is back.
pub fn type_when_ready(address: &str, command: &str) {
    thread::sleep(Duration::from_millis(450));
    let Some(window) = active_window() else {
        eprintln!("The terminal lost focus, so this was not typed:\n{command}");
        return;
    };
    if window.address != address
        || (!is_terminal(&window.class) && !is_terminal(&window.initial_class))
    {
        eprintln!("The terminal lost focus, so this was not typed:\n{command}");
        return;
    }
    let status = Command::new("wtype")
        .args(["-d", "5", "--", command])
        .stdin(Stdio::null())
        .status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => {
            eprintln!("wtype exited with {status}. The command was not typed:\n{command}");
        }
        Err(error) => {
            eprintln!("Could not type into the terminal ({error}).\n{command}");
        }
    }
}

fn schedule(address: &str, command: &str) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    // Type after this process exits, so the keystrokes land on the shell prompt.
    Command::new("setsid")
        .args([
            "-f",
            &exe.to_string_lossy(),
            "--type-when-ready",
            address,
            "--",
            command,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    Ok(())
}

fn active_window() -> Option<ActiveWindow> {
    let output = Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let doc: Value = serde_json::from_slice(&output.stdout).ok()?;
    let address = doc.get("address")?.as_str()?.to_string();
    if address.is_empty() {
        return None;
    }
    Some(ActiveWindow {
        address,
        class: doc
            .get("class")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        initial_class: doc
            .get("initialClass")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

pub fn is_terminal(class: &str) -> bool {
    let class = class.to_ascii_lowercase();
    if class.is_empty() {
        return false;
    }
    TERMINAL_MARKERS.iter().any(|marker| class.contains(marker))
}

fn wtype_installed() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join("wtype").is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_terminal_classes() {
        assert!(is_terminal("ghostty"));
        assert!(is_terminal("com.mitchellh.ghostty"));
        assert!(is_terminal("Alacritty"));
        assert!(is_terminal("kitty"));
        assert!(!is_terminal("signal"));
        assert!(!is_terminal(""));
        assert!(!is_terminal("google-chrome"));
    }
}
