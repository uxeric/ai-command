//! The chip on a message, and the colors of the card.
//!
//! Void, hot, and ice are the same inks as gitmsg. Leaf marks a sure reading.
//! Amber is the clarify field. Ghost is an explanation you have not chosen.
//! `NO_COLOR` and a dumb terminal get the plain text.

use std::io::{self, IsTerminal};

pub const ICE: &str = "\x1b[38;2;41;240;255m";
pub const AMBER: &str = "\x1b[38;2;246;196;69m";
pub const GHOST: &str = "\x1b[38;2;110;106;130m";
pub const RESET: &str = "\x1b[0m";

/// Void (`#07060f`) on hot (`#ff2a6d`), with a space of padding inside the chip.
pub const CHIP: &str = "\x1b[1;38;2;7;6;15;48;2;255;42;109m cmd \x1b[0m";

pub mod ink {
    use ratatui::style::Color;

    pub const VOID: Color = Color::Rgb(0x07, 0x06, 0x0F);
    pub const SCAN: Color = Color::Rgb(0x10, 0x0C, 0x1A);
    pub const HOT: Color = Color::Rgb(0xFF, 0x2A, 0x6D);
    pub const ICE: Color = Color::Rgb(0x29, 0xF0, 0xFF);
    pub const AMBER: Color = Color::Rgb(0xF6, 0xC4, 0x45);
    pub const LEAF: Color = Color::Rgb(0x3D, 0xFF, 0x9A);
    pub const GHOST: Color = Color::Rgb(0x6E, 0x6A, 0x82);
    pub const TEXT: Color = Color::Rgb(0xE8, 0xE6, 0xF2);
    pub const SELECT: Color = Color::Rgb(0x3A, 0x14, 0x32);
    pub const FIELD: Color = Color::Rgb(0x14, 0x10, 0x22);
    pub const FIELD_ON: Color = Color::Rgb(0x2A, 0x22, 0x10);
    pub const KEYCAP: Color = Color::Rgb(0x2E, 0x26, 0x56);
}

pub fn stderr_color() -> bool {
    color_enabled() && io::stderr().is_terminal()
}

pub fn stdout_color() -> bool {
    color_enabled() && io::stdout().is_terminal()
}

pub fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none()
        && !matches!(std::env::var("TERM").as_deref(), Ok("dumb"))
}

pub fn effects_enabled() -> bool {
    color_enabled() && std::env::var_os("CMD_NO_FX").is_none()
}

pub fn failure(error: &str, color: bool) -> String {
    if color {
        format!("{CHIP} {error}")
    } else {
        format!("cmd: {error}")
    }
}

pub fn version_line(version: &str, rev: &str, color: bool) -> String {
    if color {
        format!("{CHIP} {version} {GHOST}({rev}){RESET}")
    } else {
        format!("cmd {version} ({rev})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chip_leads_a_failure_and_plain_text_keeps_the_prefix() {
        let colored = failure("Could not reach Grok: timed out", true);
        assert!(colored.starts_with(CHIP), "{colored}");
        assert!(colored.contains("Could not reach Grok: timed out"));
        assert!(colored.contains("48;2;255;42;109"), "{colored}");
        assert!(!colored.contains("cmd:"), "{colored}");

        assert_eq!(
            failure("Could not reach Grok: timed out", false),
            "cmd: Could not reach Grok: timed out"
        );
    }

    #[test]
    fn a_version_line_names_the_build_either_way() {
        let colored = version_line("0.1.0", "abc1234", true);
        assert!(colored.contains("0.1.0"), "{colored}");
        assert!(colored.contains("abc1234"), "{colored}");
        assert!(colored.contains(CHIP), "{colored}");
        assert_eq!(
            version_line("0.1.0", "abc1234", false),
            "cmd 0.1.0 (abc1234)"
        );
    }
}
