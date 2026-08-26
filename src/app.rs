mod cli;
mod error;

use std::io::IsTerminal;

use clap::Parser;

use cli::RawArgs;
pub(crate) use error::Error;

pub(crate) fn run() -> Result<(), Error> {
    execute(RawArgs::parse())
}

fn execute(raw: RawArgs) -> Result<(), Error> {
    let request = cli::Request::try_from(raw)?;
    if !request.json && !std::io::stdout().is_terminal() {
        return Err(Error::NonTty);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> RawArgs {
        RawArgs::try_parse_from(args).expect("parse")
    }

    #[test]
    fn json_invocation_runs_without_aio_or_tty() {
        let raw = parse(&[
            "aemlog",
            "--program-id",
            "p1",
            "--environment-id",
            "e1",
            "--service",
            "author",
            "--json",
        ]);
        execute(raw).expect("accepted json invocation");
    }

    #[test]
    fn redirected_stdout_without_json_is_usage_error() {
        let raw = parse(&[
            "aemlog",
            "--program-id",
            "p1",
            "--environment-id",
            "e1",
            "--service",
            "publish",
        ]);
        match execute(raw) {
            Err(Error::NonTty) => {}
            other => panic!("expected NonTty, got {other:?}"),
        }
    }
}
