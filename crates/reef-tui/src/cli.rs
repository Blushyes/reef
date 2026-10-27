//! Parse startup commands without entering the terminal UI.

#[derive(Debug, PartialEq, Eq)]
pub enum CliCommand {
    Open(AppArgs),
    Update,
    Help,
    UpdateHelp,
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<CliCommand, String> {
    let mut args = args.into_iter().peekable();
    if args.peek().is_some_and(|arg| arg == "update") {
        args.next();
        return match (args.next().as_deref(), args.next()) {
            (None, None) => Ok(CliCommand::Update),
            (Some("--help" | "-h"), None) => Ok(CliCommand::UpdateHelp),
            _ => Err(
                "usage: reef update [--help]; to open a path named update, use reef ./update"
                    .into(),
            ),
        };
    }
    let mut agent_exec: Option<String> = None;
    let mut ssh_target: Option<String> = None;
    let mut path: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                return Ok(CliCommand::Help);
            }
            "--" => {
                for arg in args.by_ref() {
                    if path.replace(arg).is_some() {
                        return Err("expected one path after --".into());
                    }
                }
            }
            "--agent-exec" => {
                agent_exec = Some(
                    args.next()
                        .ok_or_else(|| "--agent-exec requires a value".to_string())?,
                );
            }
            "--ssh" => {
                ssh_target = Some(
                    args.next()
                        .ok_or_else(|| "--ssh requires a [user@]host[:path] value".to_string())?,
                );
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown argument: {other}"));
            }
            other => {
                if path.is_some() {
                    return Err(format!("unexpected extra path argument: {other}"));
                }
                path = Some(other.to_string());
            }
        }
    }
    let modes = [agent_exec.is_some(), ssh_target.is_some(), path.is_some()]
        .into_iter()
        .filter(|b| *b)
        .count();
    if modes > 1 {
        return Err("--agent-exec, --ssh, and <PATH> are mutually exclusive".into());
    }
    Ok(CliCommand::Open(AppArgs {
        agent_exec,
        ssh_target,
        path,
    }))
}

pub fn print_usage() {
    println!("reef — terminal UI for git");
    println!();
    println!("USAGE:");
    println!("    reef update                     # upgrade through Homebrew or global npm");
    println!("    reef update --help              # update behavior and supported installs");
    println!("    reef                            # open cwd with local backend");
    println!("    reef <DIR>                      # open DIR (supports ~/)");
    println!(
        "    reef <FILE>                     # open FILE's parent dir, drop straight into 纯预览"
    );
    println!(
        "    reef --ssh user@host            # open remote HOME on host (agent auto-installed)"
    );
    println!("    reef --ssh user@host:/path      # open /path on host");
    println!(
        "    reef --agent-exec \"CMD...\"      # advanced: drive reef through a custom agent pipe"
    );
    println!();
    println!("The --agent-exec value is whitespace-split into argv. Typical remote:");
    println!("    reef --agent-exec \"ssh user@host reef-agent --stdio --workdir /path\"");
}

#[derive(Debug, PartialEq, Eq)]
pub struct AppArgs {
    pub agent_exec: Option<String>,
    pub ssh_target: Option<String>,
    pub path: Option<String>,
}

pub fn print_update_usage() {
    println!("USAGE: reef update");
    println!();
    println!("Homebrew: refresh package metadata, then upgrade blushyes/tap/reef.");
    println!("Global npm: install @reef-tui/cli@latest through the npm launcher.");
    println!("Local npm installs: run npm install @reef-tui/cli@latest in the project.");
    println!("Temporary npx runs: use npx @reef-tui/cli@latest.");
    println!(
        "Source builds and manual binaries: rebuild or replace using your original installation method."
    );
    println!();
    println!("To open a directory named update, use reef ./update or reef -- update.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<CliCommand, String> {
        parse_args(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn update_is_a_startup_command() {
        assert_eq!(parse(&["update"]), Ok(CliCommand::Update));
    }

    #[test]
    fn explicit_update_paths_open_the_workspace() {
        for path in ["./update", "/tmp/update"] {
            assert_eq!(
                parse(&[path]),
                Ok(CliCommand::Open(AppArgs {
                    path: Some(path.into()),
                    agent_exec: None,
                    ssh_target: None,
                }))
            );
        }
        assert_eq!(
            parse(&["--", "update"]),
            Ok(CliCommand::Open(AppArgs {
                path: Some("update".into()),
                agent_exec: None,
                ssh_target: None,
            }))
        );
    }

    #[test]
    fn update_as_an_ssh_value_is_not_a_command() {
        assert_eq!(
            parse(&["--ssh", "update"]),
            Ok(CliCommand::Open(AppArgs {
                path: None,
                agent_exec: None,
                ssh_target: Some("update".into()),
            }))
        );
    }

    #[test]
    fn update_rejects_open_options_and_extra_arguments() {
        for args in [
            &["update", "other"][..],
            &["update", "--ssh", "host"],
            &["update", "--help", "extra"],
        ] {
            assert!(parse(args).is_err());
        }
    }

    #[test]
    fn startup_modes_remain_mutually_exclusive() {
        assert!(parse(&["--ssh", "host", "./update"]).is_err());
        assert!(parse(&["--agent-exec", "agent", "--ssh", "host"]).is_err());
    }
}
