mod auth;
mod context;
mod grok;
mod interpret;
mod motion;
mod picker;
mod style;
mod typeout;
mod update;

use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(Stop::Cancel) => ExitCode::from(1),
        Err(Stop::Fail(error)) => {
            eprintln!("{}", style::failure(&error, style::stderr_color()));
            ExitCode::from(1)
        }
    }
}

enum Stop {
    Cancel,
    Fail(String),
}

fn run() -> Result<(), Stop> {
    let args = Args::parse().map_err(Stop::Fail)?;
    if let Some(pending) = args.type_when_ready {
        typeout::type_when_ready(&pending.address, &pending.command);
        return Ok(());
    }
    if args.version {
        let rev =
            update::installed_rev().unwrap_or_else(|| "not installed by install.sh".to_string());
        println!(
            "{}",
            style::version_line(env!("CARGO_PKG_VERSION"), &rev, style::stdout_color())
        );
        return Ok(());
    }
    if args.update {
        let status = update::run_installer().map_err(|error| {
            Stop::Fail(format!(
                "could not run the installer ({error}); nothing was updated."
            ))
        })?;
        if !status.success() {
            return Err(Stop::Fail(
                "the update did not finish; your previous version is still installed.".into(),
            ));
        }
        return Ok(());
    }
    if args.help {
        print_help();
        return Ok(());
    }

    update::offer_if_newer().map_err(Stop::Fail)?;

    let request = args.request.trim();
    if request.is_empty() {
        return Err(Stop::Fail(
            "Tell cmd what you want to do. Example: cmd git command to merge dev into test".into(),
        ));
    }
    if matches!(env::var("TERM").as_deref(), Ok("dumb")) {
        return Err(Stop::Fail(
            "cmd needs a terminal to show the choices.".into(),
        ));
    }

    match picker::run(request).map_err(Stop::Fail)? {
        Some(command) => typeout::deliver(&command, args.print_only),
        None => return Err(Stop::Cancel),
    }
    Ok(())
}

struct Args {
    help: bool,
    version: bool,
    update: bool,
    print_only: bool,
    request: String,
    type_when_ready: Option<PendingType>,
}

struct PendingType {
    address: String,
    command: String,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut help = false;
        let mut version = false;
        let mut update = false;
        let mut print_only = false;
        let mut type_address: Option<String> = None;
        let mut command: Option<String> = None;
        let mut request_parts = Vec::new();
        let mut raw = env::args().skip(1);
        while let Some(arg) = raw.next() {
            match arg.as_str() {
                "-h" | "--help" => help = true,
                "-V" | "--version" => version = true,
                "--update" => update = true,
                "--print" => print_only = true,
                "--type-when-ready" => {
                    let address = raw
                        .next()
                        .ok_or_else(|| "--type-when-ready needs the window address.".to_string())?;
                    type_address = Some(address);
                }
                "--" => {
                    if type_address.is_some() {
                        command = Some(raw.next().ok_or_else(|| {
                            "--type-when-ready needs the command after --.".to_string()
                        })?);
                    } else {
                        request_parts.extend(raw);
                    }
                    break;
                }
                other if other.starts_with('-') => {
                    return Err(format!("Unknown option `{other}`. Run `cmd --help`."));
                }
                other => request_parts.push(other.to_string()),
            }
        }
        let type_when_ready = match type_address {
            Some(address) => Some(PendingType {
                address,
                command: command
                    .ok_or_else(|| "--type-when-ready needs the command after --.".to_string())?,
            }),
            None => None,
        };
        Ok(Self {
            help,
            version,
            update,
            print_only,
            request: request_parts.join(" "),
            type_when_ready,
        })
    }
}

fn print_help() {
    println!(
        "\
AI Command — turn a plain-language request into a shell command and place it on the line

Usage:
  cmd <request>    Show up to three readings, then place the one you pick
  cmd --print      Print that command instead of typing it
  cmd --update     Download and install the latest version
  cmd --version    Show the installed version

The readings come from your Grok login (~/.grok/auth.json). Nothing runs.
The line waits on the prompt. Enter there is what runs it.

A sure request shows one reading. An ambiguous one shows two or three.
The last row is always yours: type a clarification and the readings are asked again.

Example:
  cmd git command to merge dev into test

That can mean merge dev into test and keep dev, or merge and then delete dev.

↑ and ↓ move. Enter places the focused line. z jumps to clarify. Esc cancels.

Before the card, cmd checks for a newer version. Enter installs it
and continues. n keeps this version.

CMD_MODEL            Model id. Defaults to the model in ~/.grok/config.toml.
CMD_EFFORT           Reasoning effort. Defaults to low.
CMD_NO_UPDATE_CHECK  Set to skip the update check.
CMD_NO_FX            Set to skip the card's animation."
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_shell_helper_places_the_line_and_does_not_run_it() {
        let zsh = include_str!("../cmd.zsh");
        assert!(zsh.contains("command cmd --print"), "{zsh}");
        assert!(zsh.contains("print -z"), "{zsh}");
        assert!(!zsh.contains("eval"), "{zsh}");
    }

    #[test]
    fn the_installer_names_this_repo_and_the_helper() {
        let script = include_str!("../install.sh");
        assert!(
            script.contains("https://github.com/uxeric/ai-command.git"),
            "{script}"
        );
        assert!(script.contains("https://raw.githubusercontent.com/uxeric/ai-command/HEAD/install.sh"));
        assert!(script.contains("cmd.zsh"));
        assert!(script.contains("--uninstall"));
        assert!(script.contains("installed-rev"));
        assert!(script.contains("name = \"cmd\""));
    }
}
