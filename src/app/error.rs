use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum Error {
    #[error("program ID must be a non-empty string")]
    EmptyProgramId,
    #[error("environment ID must be a non-empty string")]
    EmptyEnvironmentId,
    #[error("IMS context must be a non-empty string")]
    EmptyImsContext,
    #[error("config path must be a non-empty string")]
    EmptyConfig,
    #[error("config file not found: {}", .0.display())]
    ConfigNotFound(PathBuf),
    #[error("config path is not a regular file: {}", .0.display())]
    ConfigNotRegular(PathBuf),
    #[error("config file is unreadable: {}: {message}", path.display())]
    ConfigUnreadable { path: PathBuf, message: String },
    #[error("invalid config file {}: {message}", path.display())]
    ConfigInvalid { path: PathBuf, message: String },
    #[error("at least one --level is required")]
    EmptyLevels,
    #[error("invalid timezone '{0}': expected utc, local, or an IANA timezone name")]
    InvalidTimezone(String),
    #[error("--raw-sample requires --json")]
    RawSampleWithoutJson,
    #[error(
        "stdout is not a terminal. TUI mode requires a TTY; use --json for redirected or piped output."
    )]
    NonTty,
    #[error("aio executable not found on PATH")]
    MissingAio,
    #[error("failed to start aio: {0}")]
    Spawn(String),
    #[error("aio I/O error: {0}")]
    Io(String),
    #[error("aio authentication failed (status {status})")]
    AuthFailure { status: String },
    #[error("aio network failure (status {status})")]
    NetworkFailure { status: String },
    #[error("aio exited normally (status {0}); live tail stopped")]
    NormalExit(String),
    #[error("source ended unexpectedly (aio status {0})")]
    UnexpectedEnd(String),
    /// Reserved for other startup failures after CLI validation (exit 1).
    #[allow(dead_code)]
    #[error("{0}")]
    Internal(String),
}

impl Error {
    pub(crate) fn exit_code(&self) -> ExitCode {
        match self {
            Self::Internal(_)
            | Self::MissingAio
            | Self::Spawn(_)
            | Self::Io(_)
            | Self::AuthFailure { .. }
            | Self::NetworkFailure { .. }
            | Self::NormalExit(_)
            | Self::UnexpectedEnd(_) => ExitCode::from(1),
            _ => ExitCode::from(2),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_input_exits_2_and_internal_failures_exit_1() {
        assert_eq!(Error::NonTty.exit_code(), ExitCode::from(2));
        assert_eq!(Error::EmptyProgramId.exit_code(), ExitCode::from(2));
        assert_eq!(
            Error::InvalidTimezone("x".into()).exit_code(),
            ExitCode::from(2)
        );
        assert_eq!(
            Error::ConfigNotFound(std::path::PathBuf::from("missing.toml")).exit_code(),
            ExitCode::from(2)
        );
        assert_eq!(
            Error::ConfigInvalid {
                path: PathBuf::from("bad.toml"),
                message: "x".into(),
            }
            .exit_code(),
            ExitCode::from(2)
        );
        assert_eq!(
            Error::Internal("startup failed".into()).exit_code(),
            ExitCode::from(1)
        );
        assert_eq!(
            Error::Spawn("not found".into()).exit_code(),
            ExitCode::from(1)
        );
        assert_eq!(Error::MissingAio.exit_code(), ExitCode::from(1));
        assert_eq!(
            Error::AuthFailure { status: "1".into() }.exit_code(),
            ExitCode::from(1)
        );
        assert_eq!(
            Error::NetworkFailure { status: "1".into() }.exit_code(),
            ExitCode::from(1)
        );
        assert_eq!(Error::NormalExit("0".into()).exit_code(), ExitCode::from(1));
        assert_eq!(
            Error::UnexpectedEnd("0".into()).exit_code(),
            ExitCode::from(1)
        );
    }
}
