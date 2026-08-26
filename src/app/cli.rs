use std::path::PathBuf;
use std::str::FromStr;

use clap::{Parser, ValueEnum};

use super::Error;

const AFTER_HELP: &str = "\
Output modes:
  Default: interactive TUI. Requires a terminal on stdout.
  --json:  machine-readable NDJSON on stdout. Required when stdout is not a TTY.

Redirected or piped stdout without --json exits with status 2. Use --json.

--level may be repeated (--level ERROR --level WARN). Values are case-insensitive;
duplicates are dropped; the default effective level is ERROR.

--service accepts Author or Publish, case-insensitively.

Invalid program, environment, service, level, timezone, or output-flag combinations
fail before any AIO process starts. Without --config, the first existing regular file
among ~/aemlog.toml, ~/.config/aemlog/config.toml, executable-directory aemlog.toml,
and working-directory aemlog.toml is loaded once. A loaded file must set version = 1
and may contain only analyzer tuning. Unknown fields are rejected. Files are never merged.
Internal startup failures exit with status 1; invalid CLI input exits with status 2.
";

#[derive(Debug, Parser)]
#[command(
    name = "aemlog",
    about = "Group live AEM as a Cloud Service error-log events",
    long_about = "\
Group live AEM as a Cloud Service error-log events.

Tails one Cloud Manager aemerror stream for an explicit program ID, environment ID, \
and Author or Publish service. The default interface is a TUI and requires a terminal. \
Use --json for NDJSON on stdout, including when stdout is redirected.

Source identifiers remain strings. Service and level values are case-insensitive. \
Validation completes before any AIO process is spawned.",
    after_help = AFTER_HELP,
    version
)]
pub(super) struct RawArgs {
    /// Cloud Manager program ID (string; never parsed as a number)
    #[arg(long = "program-id", value_name = "PROGRAM_ID", required = true)]
    program_id: String,

    /// Cloud Manager environment ID (string; never parsed as a number)
    #[arg(
        long = "environment-id",
        value_name = "ENVIRONMENT_ID",
        required = true
    )]
    environment_id: String,

    /// Target service: author or publish (case-insensitive)
    #[arg(long, value_enum, ignore_case = true)]
    service: Service,

    /// Log levels to select. Repeatable. Default: ERROR
    // Empty vec means the flag was not supplied; do not add a Clap default.
    #[arg(long, value_enum, ignore_case = true, value_name = "LEVEL")]
    level: Vec<Level>,

    /// Adobe IMS context name passed to aio
    #[arg(long = "ims-context", value_name = "CONTEXT")]
    ims_context: Option<String>,

    /// Path to analyzer TOML configuration. Authoritative; skips automatic discovery.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Timezone for zone-less source timestamps: utc (default), local, or IANA name
    // None means the flag was not supplied; do not add a Clap default.
    #[arg(long, value_name = "TIMEZONE")]
    timezone: Option<String>,

    /// Write version-1 NDJSON to stdout instead of starting the TUI
    #[arg(long)]
    json: bool,

    /// Include unredacted representative samples in JSON output; requires --json
    #[arg(long = "raw-sample", requires = "json")]
    raw_sample: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, ValueEnum)]
#[value(rename_all = "lower")]
pub(super) enum Service {
    Author,
    Publish,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, ValueEnum)]
#[value(rename_all = "UPPER")]
pub(super) enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Timezone {
    Utc,
    Local,
    Iana(chrono_tz::Tz),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct CliInput {
    pub(super) program_id: String,
    pub(super) environment_id: String,
    pub(super) service: Service,
    pub(super) levels: Vec<Level>,
    pub(super) ims_context: Option<String>,
    pub(super) config: Option<PathBuf>,
    pub(super) timezone: Option<Timezone>,
    pub(super) json: bool,
    pub(super) raw_sample: bool,
}

#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)]
pub(super) struct Request {
    pub(super) program_id: String,
    pub(super) environment_id: String,
    pub(super) service: Service,
    pub(super) levels: Vec<Level>,
    pub(super) ims_context: Option<String>,
    pub(super) config: Option<PathBuf>,
    pub(super) timezone: Timezone,
    pub(super) json: bool,
    pub(super) raw_sample: bool,
    pub(super) tuning: super::tuning::Tuning,
}

impl Service {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Author => "author",
            Self::Publish => "publish",
        }
    }
}

impl Level {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
            Self::Fatal => "FATAL",
        }
    }

    pub(super) fn from_aem(token: &str) -> Option<Self> {
        Some(match token {
            "TRACE" => Self::Trace,
            "DEBUG" => Self::Debug,
            "INFO" => Self::Info,
            "WARN" => Self::Warn,
            "ERROR" => Self::Error,
            "FATAL" => Self::Fatal,
            _ => return None,
        })
    }
}

impl TryFrom<RawArgs> for CliInput {
    type Error = Error;

    fn try_from(raw: RawArgs) -> Result<Self, Error> {
        let program_id = nonempty(raw.program_id, Error::EmptyProgramId)?;
        let environment_id = nonempty(raw.environment_id, Error::EmptyEnvironmentId)?;
        let ims_context = match raw.ims_context {
            Some(value) => Some(nonempty(value, Error::EmptyImsContext)?),
            None => None,
        };
        let config = match raw.config {
            Some(path) if path.as_os_str().is_empty() => return Err(Error::EmptyConfig),
            other => other,
        };
        if raw.raw_sample && !raw.json {
            return Err(Error::RawSampleWithoutJson);
        }
        let timezone = match raw.timezone {
            Some(value) => Some(Timezone::parse(&value)?),
            None => None,
        };
        Ok(Self {
            program_id,
            environment_id,
            service: raw.service,
            levels: raw.level,
            ims_context,
            config,
            timezone,
            json: raw.json,
            raw_sample: raw.raw_sample,
        })
    }
}

impl Timezone {
    pub(super) fn parse(raw: &str) -> Result<Self, Error> {
        if raw.trim().is_empty() {
            return Err(Error::InvalidTimezone(raw.to_owned()));
        }
        if raw.eq_ignore_ascii_case("utc") {
            return Ok(Self::Utc);
        }
        if raw.eq_ignore_ascii_case("local") {
            return Ok(Self::Local);
        }
        chrono_tz::Tz::from_str(raw)
            .map(Self::Iana)
            .map_err(|_| Error::InvalidTimezone(raw.to_owned()))
    }
}

fn nonempty(value: String, err: Error) -> Result<String, Error> {
    if value.trim().is_empty() {
        Err(err)
    } else {
        Ok(value)
    }
}

pub(super) fn dedupe(levels: Vec<Level>) -> Vec<Level> {
    let mut out = Vec::with_capacity(levels.len());
    for level in levels {
        if !out.contains(&level) {
            out.push(level);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Request {
        let raw = RawArgs::try_parse_from(args).expect("parse");
        let input = CliInput::try_from(raw).expect("validate");
        super::super::config::resolve(input, None).expect("resolve")
    }

    fn parse_input(args: &[&str]) -> CliInput {
        let raw = RawArgs::try_parse_from(args).expect("parse");
        CliInput::try_from(raw).expect("validate")
    }

    fn parse_err(args: &[&str]) -> Error {
        let raw = match RawArgs::try_parse_from(args) {
            Ok(raw) => raw,
            Err(err) => panic!("expected validation error after clap parse, got clap error: {err}"),
        };
        CliInput::try_from(raw).expect_err("expected validation error")
    }

    fn clap_err(args: &[&str]) -> clap::Error {
        RawArgs::try_parse_from(args).expect_err("expected clap error")
    }

    const BASE: &[&str] = &[
        "aemlog",
        "--program-id",
        "p1",
        "--environment-id",
        "e1",
        "--service",
        "author",
        "--json",
    ];

    #[test]
    fn default_level_is_error() {
        let request = parse(BASE);
        assert_eq!(request.levels, vec![Level::Error]);
        assert_eq!(request.service, Service::Author);
        assert_eq!(request.timezone, Timezone::Utc);
        assert!(request.json);
        assert!(!request.raw_sample);
        assert_eq!(request.program_id, "p1");
        assert_eq!(request.environment_id, "e1");
        assert_eq!(request.config, None);
    }

    #[test]
    fn omitted_level_and_timezone_are_not_cli_supplied() {
        let input = parse_input(BASE);
        assert!(input.levels.is_empty());
        assert_eq!(input.timezone, None);
    }

    #[test]
    fn repeated_levels_are_case_insensitive_and_deduped() {
        let mut args = BASE.to_vec();
        args.extend(["--level", "warn", "--level", "ERROR", "--level", "WARN"]);
        let request = parse(&args);
        assert_eq!(request.levels, vec![Level::Warn, Level::Error]);
    }

    #[test]
    fn service_is_case_insensitive() {
        let mut publish = BASE.to_vec();
        publish[6] = "PUBLISH";
        assert_eq!(parse(&publish).service, Service::Publish);

        let mut author = BASE.to_vec();
        author[6] = "Author";
        assert_eq!(parse(&author).service, Service::Author);
    }

    #[test]
    fn timezone_local_and_iana_are_accepted() {
        let mut local = BASE.to_vec();
        local.extend(["--timezone", "local"]);
        assert_eq!(parse(&local).timezone, Timezone::Local);

        let mut iana = BASE.to_vec();
        iana.extend(["--timezone", "America/New_York"]);
        match parse(&iana).timezone {
            Timezone::Iana(tz) => assert_eq!(tz.name(), "America/New_York"),
            other => panic!("expected IANA timezone, got {other:?}"),
        }
    }

    #[test]
    fn optional_ims_context_config_and_raw_sample() {
        let mut args = BASE.to_vec();
        args.extend([
            "--ims-context",
            "ctx",
            "--config",
            "/tmp/aemlog.toml",
            "--raw-sample",
        ]);
        let input = parse_input(&args);
        assert_eq!(input.ims_context.as_deref(), Some("ctx"));
        assert_eq!(
            input.config.as_deref(),
            Some(std::path::Path::new("/tmp/aemlog.toml"))
        );
        assert!(input.raw_sample);
    }

    #[test]
    fn empty_identifiers_are_rejected() {
        let mut program = BASE.to_vec();
        program[2] = "  ";
        assert_eq!(parse_err(&program), Error::EmptyProgramId);

        let mut environment = BASE.to_vec();
        environment[4] = "";
        assert_eq!(parse_err(&environment), Error::EmptyEnvironmentId);
    }

    #[test]
    fn empty_ims_context_and_invalid_timezone_are_rejected() {
        let mut ims = BASE.to_vec();
        ims.extend(["--ims-context", " "]);
        assert_eq!(parse_err(&ims), Error::EmptyImsContext);

        let mut tz = BASE.to_vec();
        tz.extend(["--timezone", "Not/AZone"]);
        assert_eq!(
            parse_err(&tz),
            Error::InvalidTimezone("Not/AZone".to_owned())
        );
    }

    #[test]
    fn invalid_service_and_level_are_clap_errors() {
        let mut service = BASE.to_vec();
        service[6] = "preview";
        assert!(clap_err(&service).kind() == clap::error::ErrorKind::InvalidValue);

        let mut level = BASE.to_vec();
        level.extend(["--level", "loud"]);
        assert!(clap_err(&level).kind() == clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn identifiers_stay_strings() {
        let mut args = BASE.to_vec();
        args[2] = "00123";
        args[4] = "00abc";
        let request = parse(&args);
        assert_eq!(request.program_id, "00123");
        assert_eq!(request.environment_id, "00abc");
    }
}
