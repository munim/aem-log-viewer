use std::io::{ErrorKind, Read};
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};

use super::cli::Request;
use super::Error;

pub(super) const AIO_PROGRAM: &str = "aio";
pub(super) const AEMERROR: &str = "aemerror";
pub(super) const STDERR_LIMIT: usize = 64 * 1024;

pub(super) const STATE_STARTING: &str = "Starting";
pub(super) const STATE_RUNNING: &str = "AIO running / awaiting logs";
pub(super) const STATE_ENDED: &str = "Ended";

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

pub(super) fn spawn(request: &Request) -> Result<Child, Error> {
    command(request).spawn().map_err(map_spawn_error)
}

fn map_spawn_error(err: std::io::Error) -> Error {
    match err.kind() {
        ErrorKind::NotFound => Error::MissingAio,
        _ => Error::Spawn(err.to_string()),
    }
}

/// Byte-bounded stderr tail. Keeps the last [`STDERR_LIMIT`] bytes and counts discarded prefix bytes.
#[derive(Debug, Default)]
pub(super) struct StderrTail {
    buf: Vec<u8>,
    discarded: u64,
}

impl StderrTail {
    pub(super) fn new() -> Self {
        Self {
            buf: Vec::with_capacity(STDERR_LIMIT),
            discarded: 0,
        }
    }

    pub(super) fn push(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if data.len() >= STDERR_LIMIT {
            self.discarded += self.buf.len() as u64 + (data.len() - STDERR_LIMIT) as u64;
            self.buf.clear();
            self.buf
                .extend_from_slice(&data[data.len() - STDERR_LIMIT..]);
            return;
        }
        let overflow = (self.buf.len() + data.len()).saturating_sub(STDERR_LIMIT);
        if overflow > 0 {
            self.buf.drain(..overflow);
            self.discarded += overflow as u64;
        }
        self.buf.extend_from_slice(data);
    }

    pub(super) fn discarded_any(&self) -> bool {
        self.discarded > 0
    }

    /// UTF-8 lossy tail. When bytes were dropped, prefix a discarded-byte marker.
    pub(super) fn snapshot(&self) -> String {
        let tail = String::from_utf8_lossy(&self.buf);
        if self.discarded == 0 {
            tail.into_owned()
        } else {
            format!("[discarded {} bytes]\n{tail}", self.discarded)
        }
    }
}

/// Drain stderr on a dedicated thread so a noisy child cannot fill the OS pipe.
pub(super) fn drain_stderr(mut stderr: impl Read + Send + 'static) -> JoinHandle<StderrTail> {
    thread::spawn(move || {
        let mut tail = StderrTail::new();
        let mut buf = [0u8; 8192];
        loop {
            match stderr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => tail.push(&buf[..n]),
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        tail
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EndReason {
    NormalExit,
    AuthenticationFailure,
    NetworkFailure,
    UnexpectedExit,
}

impl EndReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::NormalExit => "normal_exit",
            Self::AuthenticationFailure => "authentication_failure",
            Self::NetworkFailure => "network_failure",
            Self::UnexpectedExit => "unexpected_exit",
        }
    }

    pub(super) fn into_error(self, status: Option<i32>) -> Error {
        let status = status
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".into());
        match self {
            Self::AuthenticationFailure => Error::AuthFailure { status },
            Self::NetworkFailure => Error::NetworkFailure { status },
            Self::NormalExit => Error::NormalExit(status),
            Self::UnexpectedExit => Error::UnexpectedEnd(status),
        }
    }
}

pub(super) fn classify_end(status: Option<i32>, stderr: &str) -> EndReason {
    let lower = stderr.to_ascii_lowercase();
    if is_auth_failure(&lower) {
        EndReason::AuthenticationFailure
    } else if is_network_failure(&lower) {
        EndReason::NetworkFailure
    } else if status == Some(0) {
        EndReason::NormalExit
    } else {
        EndReason::UnexpectedExit
    }
}

fn is_auth_failure(stderr: &str) -> bool {
    contains_token(stderr, "not logged in")
        || contains_token(stderr, "unauthorized")
        || contains_token(stderr, "unauthenticated")
        || contains_token(stderr, "authentication")
        || contains_token(stderr, "invalid token")
        || contains_token(stderr, "access token")
        || contains_token(stderr, "forbidden")
        || contains_status(stderr, "401")
        || contains_status(stderr, "403")
}

fn is_network_failure(stderr: &str) -> bool {
    contains_token(stderr, "econnrefused")
        || contains_token(stderr, "enotfound")
        || contains_token(stderr, "etimedout")
        || contains_token(stderr, "econnreset")
        || contains_token(stderr, "enetunreach")
        || contains_token(stderr, "getaddrinfo")
        || contains_token(stderr, "socket hang up")
        || contains_token(stderr, "network error")
        || contains_token(stderr, "network")
        || contains_token(stderr, "dns")
        || contains_token(stderr, "temporarily unavailable")
        || contains_token(stderr, "connect e")
        || contains_token(stderr, "fetch failed")
        || contains_token(stderr, "certificate")
}

fn contains_token(haystack: &str, needle: &str) -> bool {
    haystack.contains(needle)
}

fn contains_status(haystack: &str, code: &str) -> bool {
    let bytes = haystack.as_bytes();
    let needle = code.as_bytes();
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_digit();
            let after = i + needle.len();
            let after_ok = after == bytes.len() || !bytes[after].is_ascii_digit();
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Strip credentials and token-like values from AIO stderr before it reaches stdout.
pub(super) fn redact_stderr(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let bytes = input.as_bytes();
    while i < bytes.len() {
        let at_boundary = i == 0 || !is_ident(bytes[i - 1]);
        if at_boundary {
            if let Some((skip, replacement)) = match_redaction(input, &lower, i) {
                out.push_str(replacement);
                i += skip;
                continue;
            }
        }
        let ch = input[i..].chars().next().expect("char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn match_redaction(input: &str, lower: &str, i: usize) -> Option<(usize, &'static str)> {
    let rest = &input[i..];
    let lower_rest = &lower[i..];
    if lower_rest.starts_with("bearer ") {
        let value_start = 7;
        let value_end = rest[value_start..]
            .find(|c: char| c.is_ascii_whitespace())
            .map(|n| value_start + n)
            .unwrap_or(rest.len());
        if value_end > value_start {
            return Some((value_end, "Bearer [REDACTED]"));
        }
    }
    if rest.starts_with("eyJ") {
        if let Some(len) = jwt_len(rest) {
            return Some((len, "[REDACTED:jwt]"));
        }
    }
    for key in [
        "authorization",
        "password",
        "passwd",
        "secret",
        "api-key",
        "api_key",
        "apikey",
        "access_token",
        "access-token",
        "refresh_token",
        "client_secret",
        "token",
    ] {
        if lower_rest.starts_with(key) {
            let after_key = &rest[key.len()..];
            let trimmed = after_key.trim_start_matches([' ', '\t']);
            let sep_skip = after_key.len() - trimmed.len();
            if let Some(stripped) = trimmed
                .strip_prefix('=')
                .or_else(|| trimmed.strip_prefix(':'))
            {
                let value = stripped.trim_start_matches([' ', '\t']);
                let value_len = assignment_value_len(key, value);
                if value_len > 0 {
                    let total =
                        key.len() + sep_skip + 1 + (stripped.len() - value.len()) + value_len;
                    return Some((total, "[REDACTED]"));
                }
            }
        }
    }
    None
}

fn assignment_value_len(key: &str, value: &str) -> usize {
    if key == "authorization" {
        let first = value
            .find(|c: char| c.is_ascii_whitespace())
            .unwrap_or(value.len());
        if first == value.len() {
            return first;
        }
        let rest = value[first..].trim_start_matches(|c: char| c.is_ascii_whitespace());
        let gap = value.len() - rest.len();
        let second = rest
            .find(|c: char| c.is_ascii_whitespace())
            .unwrap_or(rest.len());
        return gap + second;
    }
    value
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(value.len())
}

fn jwt_len(rest: &str) -> Option<usize> {
    let mut parts = 0usize;
    let mut i = 0usize;
    let bytes = rest.as_bytes();
    while parts < 3 {
        let start = i;
        while i < bytes.len() && is_base64url(bytes[i]) {
            i += 1;
        }
        if i == start {
            return None;
        }
        parts += 1;
        if parts == 3 {
            return Some(i);
        }
        if i >= bytes.len() || bytes[i] != b'.' {
            return None;
        }
        i += 1;
    }
    None
}

fn is_base64url(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;

    use super::*;
    use crate::app::cli::{Level, Service, Timezone};

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
            loaded: None,
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

    #[test]
    fn stderr_tail_keeps_last_64kib_and_marks_discard() {
        let mut tail = StderrTail::new();
        tail.push(b"HEAD");
        let mid = vec![b'x'; STDERR_LIMIT];
        tail.push(&mid);
        tail.push(b"TAIL");
        assert!(tail.discarded_any());
        let snap = tail.snapshot();
        assert!(snap.starts_with("[discarded 8 bytes]\n"), "{snap}");
        assert!(snap.ends_with("TAIL"), "{snap}");
        assert_eq!(tail.buf.len(), STDERR_LIMIT);
        assert!(!snap.contains("HEAD"));
    }

    #[test]
    fn stderr_tail_under_limit_has_no_marker() {
        let mut tail = StderrTail::new();
        tail.push(b"only this");
        assert!(!tail.discarded_any());
        assert_eq!(tail.snapshot(), "only this");
    }

    #[test]
    fn drain_thread_consumes_reader_to_tail() {
        let data = vec![b'z'; STDERR_LIMIT + 32];
        let handle = drain_stderr(Cursor::new(data.clone()));
        let tail = handle.join().expect("drain");
        assert!(tail.discarded_any());
        assert!(
            tail.snapshot().starts_with("[discarded 32 bytes]\n"),
            "{}",
            tail.snapshot()
        );
        assert_eq!(tail.buf.len(), STDERR_LIMIT);
    }

    #[test]
    fn classify_auth_network_normal_and_unexpected() {
        assert_eq!(
            classify_end(Some(1), "Error: Not logged in"),
            EndReason::AuthenticationFailure
        );
        assert_eq!(
            classify_end(Some(1), "401 Unauthorized"),
            EndReason::AuthenticationFailure
        );
        assert_eq!(
            classify_end(Some(1), "getaddrinfo ENOTFOUND cloudmanager.adobe.io"),
            EndReason::NetworkFailure
        );
        assert_eq!(
            classify_end(Some(0), "request failed: network error"),
            EndReason::NetworkFailure
        );
        assert_eq!(classify_end(Some(0), ""), EndReason::NormalExit);
        assert_eq!(
            classify_end(Some(2), "internal boom"),
            EndReason::UnexpectedExit
        );
        assert_eq!(classify_end(None, "killed"), EndReason::UnexpectedExit);
    }

    #[test]
    fn redact_strips_bearer_jwt_and_assignments() {
        let raw = "Bearer abc.def.ghi password=super-secret token: xyz \
eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0In0.abc \
authorization: Basic hunter2 keep-this";
        let redacted = redact_stderr(raw);
        assert!(!redacted.contains("abc.def.ghi"), "{redacted}");
        assert!(!redacted.contains("super-secret"), "{redacted}");
        assert!(!redacted.contains("xyz"), "{redacted}");
        assert!(!redacted.contains("eyJhbGci"), "{redacted}");
        assert!(!redacted.contains("hunter2"), "{redacted}");
        assert!(redacted.contains("keep-this"), "{redacted}");
        assert!(redacted.contains("[REDACTED]"), "{redacted}");
        assert!(redacted.contains("[REDACTED:jwt]"), "{redacted}");
    }

    #[test]
    fn states_never_claim_connected() {
        for state in [STATE_STARTING, STATE_RUNNING, STATE_ENDED] {
            assert!(!state.to_ascii_lowercase().contains("connected"), "{state}");
        }
    }
}
