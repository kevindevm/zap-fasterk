//! Console entry point for local automation: the same commands as the
//! desktop app's automation surface, as a console program so interactive
//! shells wait for it and show its output. Never opens a window.

use clap::{CommandFactory, Parser};
use zapfasterk::cli::{self, Control};

/// Zap Fasterk local automation (console companion to zap-fasterk).
#[derive(Debug, Parser)]
#[command(name = "zapfasterk-cli", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Control>,
}

fn main() {
    let cli = Cli::parse();
    let Some(command) = &cli.command else {
        // A bare run lists what this companion does, like --help does.
        let _ = Cli::command().print_help();
        println!();
        std::process::exit(2);
    };
    std::process::exit(cli::run_control(command));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn companion_accepts_the_automation_surface() {
        let cli = Cli::try_parse_from(["zapfasterk-cli", "chats", "--account", "2"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Control::Access(_)) | Some(Control::Automation { .. })
        ));
        // With a single linked account the flag is optional.
        assert!(Cli::try_parse_from(["zapfasterk-cli", "chats"]).is_ok());
        // Groups and channels list their own kinds, most recent first.
        assert!(Cli::try_parse_from(["zapfasterk-cli", "groups"]).is_ok());
        assert!(Cli::try_parse_from(["zapfasterk-cli", "channels", "--unread"]).is_ok());
        // A bare number or name singles out the send target.
        assert!(Cli::try_parse_from([
            "zapfasterk-cli",
            "send",
            "5491150531403",
            "hola",
            "--request-id",
            "0123456789abcdef0123456789abcdef",
        ])
        .is_ok());
        // The request id is optional; the CLI mints and reports one.
        assert!(Cli::try_parse_from(["zapfasterk-cli", "send", "5491150531403", "hola"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "zapfasterk-cli",
                "chats",
                "--kind",
                "all",
                "--sort",
                "name",
                "--unread",
                "--archived",
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["zapfasterk-cli", "quit"]).is_ok());
        assert!(Cli::try_parse_from(["zapfasterk-cli", "--start-hidden"]).is_err());
    }
}
