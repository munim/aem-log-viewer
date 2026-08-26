use std::sync::LazyLock;

use regex::{Captures, NoExpand, Regex};

use super::frame::RequestContext;

const IP: &str = "[REDACTED:ip]";
const EMAIL: &str = "[REDACTED:email]";
const QUERY: &str = "[REDACTED:query]";
const BEARER: &str = "[REDACTED:bearer]";
const JWT: &str = "[REDACTED:jwt]";
const AUTHORIZATION: &str = "[REDACTED:authorization]";
const PASSWORD: &str = "[REDACTED:password]";
const API_KEY: &str = "[REDACTED:api_key]";
const TOKEN: &str = "[REDACTED:token]";
const EXTRA: &str = "[REDACTED]";

#[derive(Clone, Debug, Default)]
pub(super) struct Redactor {
    extra: Vec<Regex>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub(super) struct RedactedRequestContext {
    pub client_ip: String,
    pub request_id: String,
    pub method: String,
    pub path: String,
    pub protocol: String,
}

impl Redactor {
    pub(super) fn new(extra: Vec<Regex>) -> Self {
        Self { extra }
    }

    /// Built-in classes, then extra patterns. Never interpolates capture groups.
    pub(super) fn redact(&self, input: &str) -> String {
        let mut out = apply_builtins(input);
        for pattern in &self.extra {
            out = pattern.replace_all(&out, NoExpand(EXTRA)).into_owned();
        }
        out
    }

    /// Representative and parser sample bodies. `--raw-sample` skips this path.
    pub(super) fn redact_sample(&self, input: &str, include_raw: bool) -> String {
        if include_raw {
            input.to_owned()
        } else {
            self.redact(input)
        }
    }

    /// Structured request context is always redacted, including under `--raw-sample`.
    pub(super) fn request_context(&self, ctx: &RequestContext<'_>) -> RedactedRequestContext {
        RedactedRequestContext {
            client_ip: self.redact(ctx.client_ip),
            request_id: ctx.request_id.to_owned(),
            method: ctx.method.to_owned(),
            path: self.redact(ctx.path),
            protocol: ctx.protocol.to_owned(),
        }
    }
}

fn apply_builtins(input: &str) -> String {
    let mut out = input.to_owned();
    out = keep_prefix(&AUTHORIZATION_RE, &out, AUTHORIZATION);
    out = keep_prefix(&BEARER_RE, &out, BEARER);
    out = JWT_RE.replace_all(&out, NoExpand(JWT)).into_owned();
    out = keep_prefix(&PASSWORD_RE, &out, PASSWORD);
    out = keep_prefix(&API_KEY_RE, &out, API_KEY);
    out = keep_prefix(&TOKEN_RE, &out, TOKEN);
    out = keep_prefix(&QUERY_RE, &out, QUERY);
    out = EMAIL_RE.replace_all(&out, NoExpand(EMAIL)).into_owned();
    out = IPV6_RE.replace_all(&out, NoExpand(IP)).into_owned();
    out = LOOPBACK_RE
        .replace_all(&out, |caps: &Captures| {
            format!("{}{IP}{}", &caps[1], &caps[3])
        })
        .into_owned();
    out = IPV4_RE.replace_all(&out, NoExpand(IP)).into_owned();
    out
}

fn keep_prefix(re: &Regex, input: &str, marker: &str) -> String {
    re.replace_all(input, |caps: &Captures| format!("{}{marker}", &caps[1]))
        .into_owned()
}

static AUTHORIZATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(authorization\s*:\s*)\S+(?:\s+\S+)?").expect("authorization regex")
});
static BEARER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(\bbearer\s+)[A-Za-z0-9._\-+/=]{8,}").expect("bearer regex"));
static JWT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b").expect("jwt regex")
});
static PASSWORD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(\b(?:password|passwd|pwd)\s*[=:]\s*)(?:"[^"]*"|'[^']*'|[^\s,;&]+)"#)
        .expect("password regex")
});
static API_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(\bapi[_-]?key\s*[=:]\s*)(?:"[^"]*"|'[^']*'|[^\s,;&]+)"#)
        .expect("api_key regex")
});
static TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(\b(?:(?:access|refresh|id)[_-]?token|token)\s*[=:]\s*)(?:"[^"]*"|'[^']*'|[^\s,;&]+)"#)
        .expect("token regex")
});
static QUERY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([?&][^=&#\s]+=)([^&#\s]*)").expect("query regex"));
static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[A-Z0-9._%+\-]+@[A-Z0-9.\-]+\.[A-Z]{2,}\b").expect("email regex")
});
static IPV4_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.){3}(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\b",
    )
    .expect("ipv4 regex")
});
static IPV6_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:(?:[0-9a-f]{1,4}:){7}[0-9a-f]{1,4}|(?:[0-9a-f]{1,4}:)*[0-9a-f]{2,4}::[0-9a-f]{1,4}(?::[0-9a-f]{1,4})*)\b",
    )
    .expect("ipv6 regex")
});
static LOOPBACK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^|[^0-9A-Za-z:])(::1)([^0-9A-Za-z:]|$)").expect("ipv6 loopback regex")
});

#[cfg(test)]
mod tests {
    use super::*;

    fn redact(input: &str) -> String {
        Redactor::default().redact(input)
    }

    fn extra(pattern: &str) -> Redactor {
        Redactor::new(vec![Regex::new(pattern).expect("test pattern")])
    }

    fn assert_hides(out: &str, secret: &str, marker: &str) {
        assert!(
            !out.contains(secret),
            "secret fragment {secret:?} leaked in {out:?}"
        );
        assert!(out.contains(marker), "missing {marker} in {out:?}");
    }

    #[test]
    fn each_builtin_class_uses_typed_placeholder_without_secret_fragment() {
        let jwt = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJ4In0.sigvalue";
        let cases = [
            ("192.0.2.10", "192.0.2.10", IP),
            ("2001:db8::1", "2001:db8::1", IP),
            ("::1", "::1", IP),
            ("ops@example.com", "ops@example.com", EMAIL),
            (
                "Authorization: Bearer abcdefghijklmnop",
                "abcdefghijklmnop",
                AUTHORIZATION,
            ),
            ("Bearer abcdefghijklmnop", "abcdefghijklmnop", BEARER),
            (jwt, jwt, JWT),
            ("password=hunter2", "hunter2", PASSWORD),
            ("api_key=abcd1234", "abcd1234", API_KEY),
            ("token=s3cretValue", "s3cretValue", TOKEN),
        ];
        for (input, secret, marker) in cases {
            assert_hides(&redact(input), secret, marker);
        }
    }

    #[test]
    fn uri_path_and_query_keys_remain_values_replaced() {
        assert_eq!(
            redact("/content/site/us/en.html?foo=bar&baz=qux"),
            format!("/content/site/us/en.html?foo={QUERY}&baz={QUERY}")
        );
        assert_eq!(
            redact("/content/site/us/en.html?q=%73%65%63%72%65%74"),
            format!("/content/site/us/en.html?q={QUERY}")
        );
        assert_eq!(
            redact("https://example.com/x?empty=&keep#frag"),
            format!("https://example.com/x?empty={QUERY}&keep#frag")
        );
        assert_eq!(
            redact("/content/site/us/en.html"),
            "/content/site/us/en.html"
        );
        assert_eq!(redact("/path?onlykey"), "/path?onlykey");
    }

    #[test]
    fn auth_bearer_jwt_password_api_key_and_token_are_case_insensitive() {
        assert_hides(
            &redact("AUTHORIZATION: BASIC dXNlcjpwYXNz"),
            "dXNlcjpwYXNz",
            AUTHORIZATION,
        );
        assert_hides(
            &redact("bearer 0123456789abcdef"),
            "0123456789abcdef",
            BEARER,
        );
        assert_hides(
            &redact("eyjhbGciOiJub25lIn0.eyJzdWIiOiJ4In0.sigvalue"),
            "eyjhbGciOiJub25lIn0.eyJzdWIiOiJ4In0.sigvalue",
            JWT,
        );
        assert_hides(&redact("Password: hunter2"), "hunter2", PASSWORD);
        assert_hides(&redact("PWD=hunter2"), "hunter2", PASSWORD);
        assert_hides(&redact("API-Key=abcd1234"), "abcd1234", API_KEY);
        assert_hides(&redact("Access-Token=s3cretValue"), "s3cretValue", TOKEN);
    }

    #[test]
    fn content_paths_loggers_exceptions_and_stacks_remain() {
        let sample = "\
26.08.2026 12:00:01.456 author-0 *ERROR* [FelixDispatchQueue] com.example.core.filters.ErrorFilter Uncaught request exception
java.lang.IllegalStateException: resource resolver closed
\tat com.example.core.filters.ErrorFilter.doFilter(ErrorFilter.java:64)
GET /content/site/us/en.html HTTP/1.1";
        let out = redact(sample);
        assert_eq!(out, sample);
    }

    #[test]
    fn extra_patterns_run_after_builtins_and_replace_full_match() {
        let redactor = extra(r"\[REDACTED:email\]|secret-[0-9]+");
        let out = redactor.redact("ops@example.com and secret-99 leftover");
        assert_eq!(out, format!("{EXTRA} and {EXTRA} leftover"));
        assert!(!out.contains("ops@example.com"));
        assert!(!out.contains("secret-99"));
        assert!(!out.contains(EMAIL));
    }

    #[test]
    fn extra_capturing_groups_cannot_leak_the_match() {
        let redactor = extra(r"(secret-[0-9]+)");
        let out = redactor.redact("prefix secret-99 suffix");
        assert_eq!(out, format!("prefix {EXTRA} suffix"));
        assert!(!out.contains("secret-99"));
        assert!(!out.contains("$1"));
    }

    #[test]
    fn secrets_at_string_boundaries_and_multiple_per_line() {
        assert_eq!(redact("ops@example.com"), EMAIL);
        assert_eq!(redact("192.0.2.10"), IP);
        let line = "ops@example.com admin@example.net from 192.0.2.10 and 198.51.100.7";
        let out = redact(line);
        assert_eq!(out, format!("{EMAIL} {EMAIL} from {IP} and {IP}"));
        assert!(!out.contains('@'));
        assert!(!out.contains("192.0.2.10"));
        assert!(!out.contains("198.51.100.7"));
    }

    #[test]
    fn multiline_samples_redact_each_line() {
        let sample = "user ops@example.com failed\nnext 192.0.2.10\npassword=hunter2";
        let out = redact(sample);
        assert_eq!(
            out,
            format!("user {EMAIL} failed\nnext {IP}\npassword={PASSWORD}")
        );
        assert!(!out.contains("ops@example.com"));
        assert!(!out.contains("192.0.2.10"));
        assert!(!out.contains("hunter2"));
    }

    #[test]
    fn false_positives_stay_put() {
        for original in [
            "Bearer token missing",
            "authorization failed",
            "26.08.2026 12:00:00.123",
            "com.example.bundle.Activator",
            "Foo.java:42",
            "version 1.2.3",
            "token count increased",
        ] {
            assert_eq!(redact(original), original, "{original}");
        }
    }

    #[test]
    fn raw_sample_skips_only_sample_bodies() {
        let redactor = extra("INTERNAL-[0-9]+");
        let sample = "ops@example.com INTERNAL-7 192.0.2.10";
        assert_eq!(redactor.redact_sample(sample, true), sample);
        let redacted = redactor.redact_sample(sample, false);
        assert_hides(&redacted, "ops@example.com", EMAIL);
        assert_hides(&redacted, "192.0.2.10", IP);
        assert_hides(&redacted, "INTERNAL-7", EXTRA);
        let ctx = RequestContext {
            client_ip: "192.0.2.10",
            request_id: "1724666401456",
            method: "GET",
            path: "/content/site/us/en.html?foo=bar",
            protocol: "HTTP/1.1",
        };
        let redacted_ctx = redactor.request_context(&ctx);
        assert_eq!(redacted_ctx.client_ip, IP);
        assert_eq!(redacted_ctx.request_id, "1724666401456");
        assert_eq!(redacted_ctx.method, "GET");
        assert_eq!(
            redacted_ctx.path,
            format!("/content/site/us/en.html?foo={QUERY}")
        );
        assert_eq!(redacted_ctx.protocol, "HTTP/1.1");
        assert_eq!(
            redactor.redact("AIO token=s3cretValue"),
            format!("AIO token={TOKEN}")
        );
    }
}
