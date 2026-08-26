use std::process::{Command, Stdio};

use super::cli::Request;

pub(super) const AIO_PROGRAM: &str = "aio";
pub(super) const AEMERROR: &str = "aemerror";

/// Exact supported AIO argument vector, never passed through a shell.
pub(super) fn tail_log_args(request: &Request) -> Vec<String> {
    let mut args = vec![
        "cloudmanager".to_owned(),
        "tail-log".to_owned(),
        request.environment_id.clone(),
        request.service.as_str().to_owned(),
        AEMERROR.to_owned(),
        "--programId".to_owned(),
        request.program_id.clone(),
    ];
    if let Some(context) = &request.ims_context {
        args.push("--imsContextName".to_owned());
        args.push(context.clone());
    }
    args
}

pub(super) fn command(request: &Request) -> Command {
    let mut cmd = Command::new(AIO_PROGRAM);
    cmd.args(tail_log_args(request))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::app::cli::{Level, Service, Timezone};
    use crate::app::tuning::Tuning;

    fn request(program: &str, environment: &str, ims: Option<&str>) -> Request {
        Request {
            program_id: program.to_owned(),
            environment_id: environment.to_owned(),
            service: Service::Author,
            levels: vec![Level::Error],
            ims_context: ims.map(str::to_owned),
            config: None::<PathBuf>,
            timezone: Timezone::Utc,
            json: true,
            raw_sample: false,
            tuning: Tuning::default(),
        }
    }

    #[test]
    fn argument_vector_is_explicit_tokens_without_shell() {
        let args = tail_log_args(&request("p1", "e1", None));
        assert_eq!(
            args,
            [
                "cloudmanager",
                "tail-log",
                "e1",
                "author",
                "aemerror",
                "--programId",
                "p1",
            ]
        );
        assert!(!args.iter().any(|arg| arg.contains(' ')));
    }

    #[test]
    fn optional_ims_context_appends_exact_flag_pair() {
        let args = tail_log_args(&request("p1", "e1", Some("ctx")));
        assert_eq!(args[7..], ["--imsContextName".to_owned(), "ctx".to_owned()]);
    }

    #[test]
    fn spaces_and_shell_metacharacters_stay_literal_arguments() {
        let program = "p 1; rm -rf /";
        let environment = "e1 $(uname) && echo pwned | cat";
        let ims = Some("ctx`id`;echo owned");
        let args = tail_log_args(&request(program, environment, ims));
        assert_eq!(args[2], environment);
        assert_eq!(args[6], program);
        assert_eq!(args[8], ims.unwrap());
        assert_eq!(args.len(), 9);
        assert!(!args.iter().any(|arg| arg == "-c" || arg == "sh"));
    }

    #[test]
    fn publish_service_is_lowercase_aio_token() {
        let mut req = request("00123", "00abc", None);
        req.service = Service::Publish;
        let args = tail_log_args(&req);
        assert_eq!(args[3], "publish");
        assert_eq!(args[2], "00abc");
        assert_eq!(args[6], "00123");
    }
}
