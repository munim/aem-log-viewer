mod cli;
mod config;
mod error;
mod live;
mod source;

use std::io::IsTerminal;

use clap::Parser;

use cli::RawArgs;
use config::SearchRoots;
pub(crate) use error::Error;

pub(crate) fn run() -> Result<(), Error> {
    execute(RawArgs::parse(), SearchRoots::from_process())
}

fn execute(raw: RawArgs, roots: SearchRoots) -> Result<(), Error> {
    let input = cli::CliInput::try_from(raw)?;
    let loaded = config::load(input.config.as_deref(), &roots)?;
    let request = config::resolve(input, loaded)?;
    if !request.json && !std::io::stdout().is_terminal() {
        return Err(Error::NonTty);
    }
    if request.json {
        live::run(&request)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> RawArgs {
        RawArgs::try_parse_from(args).expect("parse")
    }

    #[test]
    fn json_request_is_accepted_before_source_start() {
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
        let input = cli::CliInput::try_from(raw).expect("accepted json invocation");
        let request = config::resolve(input, None).expect("resolved json invocation");
        assert!(request.json);
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
        match execute(raw, SearchRoots::default()) {
            Err(Error::NonTty) => {}
            other => panic!("expected NonTty, got {other:?}"),
        }
    }
}
