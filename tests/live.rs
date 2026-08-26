use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const FAKE_AIO: &str = r#"#!/bin/sh
if [ -n "${AEMLOG_FAKE_AIO_STARTS-}" ]; then
  printf 'x\n' >> "$AEMLOG_FAKE_AIO_STARTS"
fi

if [ -n "${AEMLOG_FAKE_AIO_RECORD-}" ]; then
  : > "$AEMLOG_FAKE_AIO_RECORD"
  for arg in "$@"; do
    printf '%s\0' "$arg" >> "$AEMLOG_FAKE_AIO_RECORD"
  done
fi

if [ -n "${AEMLOG_FAKE_AIO_STDIN_RECORD-}" ]; then
  bytes=$(dd bs=4096 count=1 2>/dev/null | wc -c | tr -d ' \t')
  printf '%s' "$bytes" > "$AEMLOG_FAKE_AIO_STDIN_RECORD"
fi

log_line() {
  printf '%s\n' "26.08.2026 12:00:00.123 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo $1"
}

mode="${AEMLOG_FAKE_AIO_MODE-}"
exit_code="${AEMLOG_FAKE_AIO_EXIT:-0}"

case "$mode" in
  stdout-flood)
    i=0
    while [ "$i" -lt 8000 ]; do
      log_line "flood"
      i=$((i + 1))
    done
    exit "$exit_code"
    ;;
  stderr-flood)
    log_line "before stderr flood"
    dd if=/dev/zero bs=1024 count=1024 2>/dev/null | tr '\0' 'x' >&2
    printf '%s' 'STDERR_TAIL_MARKER' >&2
    log_line "after stderr flood"
    exit "$exit_code"
    ;;
  both-flood)
    log_line "simultaneous start"
    dd if=/dev/zero bs=1024 count=1024 2>/dev/null | tr '\0' 'x' >&2 &
    i=0
    while [ "$i" -lt 8000 ]; do
      log_line "flood"
      i=$((i + 1))
    done
    wait
    printf '%s' 'STDERR_TAIL_MARKER' >&2
    exit "$exit_code"
    ;;
  broken-pipe)
    log_line "before close stdout"
    exec 1>&-
    dd if=/dev/zero bs=1024 count=1024 2>/dev/null | tr '\0' 'x' >&2
    printf '%s' 'STDERR_TAIL_MARKER' >&2
    exit "$exit_code"
    ;;
  auth)
    printf '%s\n' "Error: Not logged in. Run aio login." >&2
    printf '%s\n' "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ4In0.sig password=super-secret" >&2
    exit "${AEMLOG_FAKE_AIO_EXIT:-1}"
    ;;
  network)
    printf '%s\n' "getaddrinfo ENOTFOUND cloudmanager.adobe.io" >&2
    printf '%s\n' "request failed: network error token=abc123" >&2
    exit "${AEMLOG_FAKE_AIO_EXIT:-1}"
    ;;
esac

if [ -n "${AEMLOG_FAKE_AIO_STDERR-}" ]; then
  printf '%s\n' "$AEMLOG_FAKE_AIO_STDERR" >&2
fi

if [ -n "${AEMLOG_FAKE_AIO_LOGS-}" ]; then
  cat "$AEMLOG_FAKE_AIO_LOGS"
else
  cat <<'EOF'
26.08.2026 12:00:00.123 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo Failed to start bundle
26.08.2026 12:00:00.456 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo Failed to start bundle
26.08.2026 12:00:00.789 author-0 *WARN* [FelixDispatchQueue] com.example.Bar ignored warn
26.08.2026 12:00:01.001 author-0 *ERROR* [FelixDispatchQueue] com.example.Baz other error
not a log line
EOF
fi

exit "$exit_code"
"#;

const JSON_ARGS: &[&str] = &[
    "--program-id",
    "p1",
    "--environment-id",
    "e1",
    "--service",
    "author",
    "--json",
];

struct FakeAio {
    dir: PathBuf,
    record: PathBuf,
    stdin_record: PathBuf,
    logs: PathBuf,
    starts: PathBuf,
}

impl FakeAio {
    fn install() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "aemlog-fake-aio-{}-{}-{:?}",
            std::process::id(),
            nanos,
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let bin = dir.join("aio");
        fs::write(&bin, FAKE_AIO).expect("write fake aio");
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("chmod aio");
        Self {
            record: dir.join("args"),
            stdin_record: dir.join("stdin"),
            logs: dir.join("logs"),
            starts: dir.join("starts"),
            dir,
        }
    }

    fn path(&self) -> String {
        format!("{}:/usr/bin:/bin", self.dir.display())
    }

    fn recorded_args(&self) -> Vec<String> {
        let bytes = fs::read(&self.record).expect("read args");
        bytes
            .split(|b| *b == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8(part.to_vec()).expect("arg utf8"))
            .collect()
    }

    fn stdin_bytes(&self) -> u64 {
        fs::read_to_string(&self.stdin_record)
            .expect("stdin record")
            .parse()
            .expect("stdin count")
    }

    fn start_count(&self) -> usize {
        fs::read_to_string(&self.starts)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.is_empty())
            .count()
    }
}

impl Drop for FakeAio {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn aemlog() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aemlog"))
}

fn run_with_fake(fake: &FakeAio, args: &[&str], stdin: Option<&[u8]>) -> Output {
    run_with_fake_env(fake, args, stdin, &[])
}

fn run_with_fake_env(
    fake: &FakeAio,
    args: &[&str],
    stdin: Option<&[u8]>,
    extra: &[(&str, &str)],
) -> Output {
    let mut cmd = aemlog();
    cmd.args(args)
        .env_clear()
        .env("PATH", fake.path())
        .env("AEMLOG_FAKE_AIO_RECORD", &fake.record)
        .env("AEMLOG_FAKE_AIO_STDIN_RECORD", &fake.stdin_record)
        .env("AEMLOG_FAKE_AIO_STARTS", &fake.starts)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let has_stderr = extra.iter().any(|(k, _)| *k == "AEMLOG_FAKE_AIO_STDERR");
    if !has_stderr {
        cmd.env("AEMLOG_FAKE_AIO_STDERR", "AIO_STDERR_MARKER");
    }
    if fake.logs.exists() {
        cmd.env("AEMLOG_FAKE_AIO_LOGS", &fake.logs);
    }
    for (key, value) in extra {
        cmd.env(key, value);
    }
    if let Some(bytes) = stdin {
        cmd.stdin(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn aemlog");
        {
            let mut child_stdin = child.stdin.take().expect("piped stdin");
            child_stdin.write_all(bytes).expect("write stdin");
        }
        wait_output_timeout(child, Duration::from_secs(20))
    } else {
        cmd.stdin(Stdio::null());
        wait_output_timeout(cmd.spawn().expect("spawn aemlog"), Duration::from_secs(20))
    }
}

fn wait_output_timeout(child: std::process::Child, timeout: Duration) -> Output {
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result.expect("wait aemlog"),
        Err(_) => {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            panic!("aemlog timed out after {timeout:?} (possible pipe deadlock); killed pid {pid}");
        }
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json_lines(output: &Output) -> Vec<serde_json::Value> {
    stdout(output)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|err| {
                panic!(
                    "stdout line is not JSON ({err}): {line}\nstderr={}",
                    stderr(output)
                )
            })
        })
        .collect()
}

fn is_uuid_v4(id: &str) -> bool {
    let parts: Vec<&str> = id.split('-').collect();
    parts.len() == 5
        && parts[0].len() == 8
        && parts[1].len() == 4
        && parts[2].len() == 4
        && parts[3].len() == 4
        && parts[4].len() == 12
        && parts[2].starts_with('4')
        && parts[3]
            .chars()
            .next()
            .is_some_and(|c| matches!(c, '8' | '9' | 'a' | 'b' | 'A' | 'B'))
}

fn assert_observable_states(output: &Output, recs: &[serde_json::Value]) {
    let err = stderr(output);
    assert!(err.contains("source: Starting"), "{err}");
    assert!(err.contains("source: AIO running / awaiting logs"), "{err}");
    assert!(err.contains("source: Ended"), "{err}");
    assert!(!err.to_ascii_lowercase().contains("connected"), "{err}");
    assert_eq!(recs[0]["source_state"], "AIO running / awaiting logs");
    let last = recs.last().unwrap();
    assert_eq!(last["type"], "source_ended");
    assert_eq!(last["source_state"], "Ended");
    let blob = recs.iter().map(|r| r.to_string()).collect::<String>();
    assert!(!blob.to_ascii_lowercase().contains("connected"), "{blob}");
}

#[test]
fn fake_aio_emits_ndjson_session_groups_and_unexpected_end() {
    let fake = FakeAio::install();
    let output = run_with_fake(&fake, JSON_ARGS, None);
    assert_eq!(output.status.code(), Some(1), "stderr={}", stderr(&output));
    assert!(
        stderr(&output).contains("aio exited normally"),
        "{}",
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains("AIO_STDERR_MARKER"),
        "aio stderr must be piped, not inherited: {}",
        stderr(&output)
    );

    let recs = json_lines(&output);
    assert_eq!(recs[0]["type"], "session_started");
    assert_eq!(recs[0]["version"], 1);
    let session = recs[0]["session_id"].as_str().unwrap();
    assert!(is_uuid_v4(session), "{session}");
    assert!(recs[0]["emitted_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(recs[0]["source"]["program_id"], "p1");
    assert_eq!(recs[0]["source"]["environment_id"], "e1");
    assert_eq!(recs[0]["source"]["service"], "author");
    assert_eq!(recs[0]["source"]["log"], "aemerror");
    assert_eq!(recs[0]["levels"], serde_json::json!(["ERROR"]));
    assert_observable_states(&output, &recs);

    assert_eq!(recs[1]["type"], "group_created");
    assert_eq!(recs[1]["group_id"], 1);
    assert_eq!(recs[1]["count"], 1);
    assert_eq!(recs[2]["type"], "group_updated");
    assert_eq!(recs[2]["group_id"], 1);
    assert_eq!(recs[2]["count"], 2);
    assert_eq!(recs[3]["type"], "group_created");
    assert_eq!(recs[3]["group_id"], 2);
    let last = recs.last().unwrap();
    assert_eq!(last["type"], "source_ended");
    assert_eq!(last["status"], 0);
    assert_eq!(last["reason"], "normal_exit");
    assert_eq!(last["stderr_discarded"], false);
    assert!(
        last["stderr"]
            .as_str()
            .unwrap()
            .contains("AIO_STDERR_MARKER"),
        "{}",
        last["stderr"]
    );
    assert_eq!(last["session_id"], session);

    let types: Vec<&str> = recs.iter().map(|r| r["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "session_started",
            "group_created",
            "group_updated",
            "group_created",
            "source_ended"
        ]
    );

    assert_eq!(
        fake.recorded_args(),
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
    assert_eq!(fake.stdin_bytes(), 0);
    assert_eq!(fake.start_count(), 1, "live tail must not retry after exit");
}

#[test]
fn literal_shell_metacharacters_and_ims_context_reach_child() {
    let fake = FakeAio::install();
    let marker = fake.dir.join("pwned");
    let program = "p 1; rm -rf /";
    let environment = format!(
        "e1 $(uname) && echo pwned | cat; touch {}",
        marker.display()
    );
    let ims = "ctx`id`; echo owned";
    let output = run_with_fake(
        &fake,
        &[
            "--program-id",
            program,
            "--environment-id",
            &environment,
            "--service",
            "publish",
            "--ims-context",
            ims,
            "--level",
            "ERROR",
            "--json",
        ],
        Some(b"SHOULD_NOT_REACH_AIO\n"),
    );
    assert_eq!(output.status.code(), Some(1), "stderr={}", stderr(&output));
    let recs = json_lines(&output);
    assert_eq!(recs[0]["type"], "session_started");
    assert_eq!(recs[0]["source"]["service"], "publish");
    assert_eq!(recs[0]["source"]["ims_context"], ims);
    assert_eq!(recs[0]["source"]["environment_id"], environment);

    let args = fake.recorded_args();
    assert_eq!(
        args,
        vec![
            "cloudmanager".to_owned(),
            "tail-log".to_owned(),
            environment,
            "publish".to_owned(),
            "aemerror".to_owned(),
            "--programId".to_owned(),
            program.to_owned(),
            "--imsContextName".to_owned(),
            ims.to_owned(),
        ]
    );
    assert!(!args.iter().any(|arg| arg == "-c" || arg == "sh"));
    assert_eq!(fake.stdin_bytes(), 0);
    assert!(
        !marker.exists(),
        "shell metacharacters executed and created {}",
        marker.display()
    );
}

#[test]
fn missing_aio_exits_1_without_non_json_stdout() {
    let output = aemlog()
        .args(JSON_ARGS)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run aemlog");
    assert_eq!(output.status.code(), Some(1), "stderr={}", stderr(&output));
    assert!(
        stderr(&output).contains("aio executable not found on PATH"),
        "{}",
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains("failed to start aio"),
        "{}",
        stderr(&output)
    );
    for line in stdout(&output).lines().filter(|line| !line.is_empty()) {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|err| panic!("stdout line is not JSON ({err}): {line}"));
    }
}

#[test]
fn spawn_failure_is_distinct_from_missing_aio() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "aemlog-spawn-fail-{}-{}-{:?}",
        std::process::id(),
        nanos,
        std::thread::current().id()
    ));
    fs::create_dir_all(&dir).expect("temp dir");
    fs::create_dir(dir.join("aio")).expect("aio directory");
    let output = aemlog()
        .args(JSON_ARGS)
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", dir.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run aemlog");
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(output.status.code(), Some(1), "stderr={}", stderr(&output));
    assert!(
        stderr(&output).contains("failed to start aio"),
        "{}",
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains("aio executable not found on PATH"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn custom_logs_skip_unselected_levels() {
    let fake = FakeAio::install();
    fs::write(
        &fake.logs,
        "\
26.08.2026 12:00:00.123 n *WARN* [t] ignored\n\
26.08.2026 12:00:00.124 n *ERROR* [t] keep me\n",
    )
    .unwrap();
    let output = run_with_fake(&fake, JSON_ARGS, None);
    let recs = json_lines(&output);
    let types: Vec<&str> = recs.iter().map(|r| r["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["session_started", "group_created", "source_ended"]);
    assert!(recs[1]["sample"].as_str().unwrap().contains("keep me"));
}

#[test]
fn stdout_flood_does_not_deadlock_and_exits_1() {
    let fake = FakeAio::install();
    let output = run_with_fake_env(
        &fake,
        JSON_ARGS,
        None,
        &[("AEMLOG_FAKE_AIO_MODE", "stdout-flood")],
    );
    assert_eq!(output.status.code(), Some(1), "stderr={}", stderr(&output));
    let recs = json_lines(&output);
    assert_observable_states(&output, &recs);
    assert_eq!(recs[1]["type"], "group_created");
    assert_eq!(recs[2]["type"], "group_updated");
    assert!(recs[2]["count"].as_u64().unwrap() > 1);
    assert_eq!(recs.last().unwrap()["reason"], "normal_exit");
    assert_eq!(fake.start_count(), 1);
}

#[test]
fn stderr_flood_beyond_pipe_capacity_does_not_deadlock() {
    let fake = FakeAio::install();
    let output = run_with_fake_env(
        &fake,
        JSON_ARGS,
        None,
        &[("AEMLOG_FAKE_AIO_MODE", "stderr-flood")],
    );
    assert_eq!(output.status.code(), Some(1), "stderr={}", stderr(&output));
    let recs = json_lines(&output);
    assert_observable_states(&output, &recs);
    let last = recs.last().unwrap();
    assert_eq!(last["stderr_discarded"], true);
    let captured = last["stderr"].as_str().unwrap();
    assert!(
        captured.contains("[discarded "),
        "missing discarded-byte marker: {captured}"
    );
    assert!(
        captured.contains("STDERR_TAIL_MARKER"),
        "tail must retain the final stderr bytes: {captured}"
    );
    assert!(recs.iter().any(|r| r["type"] == "group_created"));
}

#[test]
fn simultaneous_stdout_and_stderr_flood_does_not_deadlock() {
    let fake = FakeAio::install();
    let output = run_with_fake_env(
        &fake,
        JSON_ARGS,
        None,
        &[("AEMLOG_FAKE_AIO_MODE", "both-flood")],
    );
    assert_eq!(output.status.code(), Some(1), "stderr={}", stderr(&output));
    let recs = json_lines(&output);
    assert_observable_states(&output, &recs);
    assert_eq!(recs.last().unwrap()["stderr_discarded"], true);
    assert!(recs.iter().any(|r| r["type"] == "group_updated"));
}

#[test]
fn broken_stdout_pipe_still_drains_stderr() {
    let fake = FakeAio::install();
    let output = run_with_fake_env(
        &fake,
        JSON_ARGS,
        None,
        &[("AEMLOG_FAKE_AIO_MODE", "broken-pipe")],
    );
    assert_eq!(output.status.code(), Some(1), "stderr={}", stderr(&output));
    let recs = json_lines(&output);
    assert_observable_states(&output, &recs);
    let last = recs.last().unwrap();
    assert_eq!(last["stderr_discarded"], true);
    assert!(
        last["stderr"]
            .as_str()
            .unwrap()
            .contains("STDERR_TAIL_MARKER"),
        "{}",
        last["stderr"]
    );
}

#[test]
fn authentication_failure_is_distinct_and_redacts_stderr() {
    let fake = FakeAio::install();
    let output = run_with_fake_env(&fake, JSON_ARGS, None, &[("AEMLOG_FAKE_AIO_MODE", "auth")]);
    assert_eq!(output.status.code(), Some(1), "stderr={}", stderr(&output));
    assert!(
        stderr(&output).contains("aio authentication failed"),
        "{}",
        stderr(&output)
    );
    let recs = json_lines(&output);
    assert_observable_states(&output, &recs);
    let last = recs.last().unwrap();
    assert_eq!(last["reason"], "authentication_failure");
    assert_eq!(last["status"], 1);
    let captured = last["stderr"].as_str().unwrap();
    assert!(captured.contains("Not logged in"), "{captured}");
    assert!(!captured.contains("super-secret"), "{captured}");
    assert!(!captured.contains("eyJhbGci"), "{captured}");
    assert!(captured.contains("[REDACTED]"), "{captured}");
    let stdout_blob = stdout(&output);
    assert!(!stdout_blob.contains("super-secret"), "{stdout_blob}");
    assert!(!stdout_blob.contains("eyJhbGci"), "{stdout_blob}");
    assert_eq!(fake.start_count(), 1);
}

#[test]
fn network_failure_is_distinct_and_redacts_tokens() {
    let fake = FakeAio::install();
    let output = run_with_fake_env(
        &fake,
        JSON_ARGS,
        None,
        &[("AEMLOG_FAKE_AIO_MODE", "network")],
    );
    assert_eq!(output.status.code(), Some(1), "stderr={}", stderr(&output));
    assert!(
        stderr(&output).contains("aio network failure"),
        "{}",
        stderr(&output)
    );
    let recs = json_lines(&output);
    assert_observable_states(&output, &recs);
    let last = recs.last().unwrap();
    assert_eq!(last["reason"], "network_failure");
    assert_eq!(last["status"], 1);
    let captured = last["stderr"].as_str().unwrap();
    assert!(captured.contains("ENOTFOUND"), "{captured}");
    assert!(!captured.contains("abc123"), "{captured}");
    assert!(captured.contains("[REDACTED]"), "{captured}");
}

#[test]
fn non_zero_child_status_is_unexpected_end() {
    let fake = FakeAio::install();
    let output = run_with_fake_env(&fake, JSON_ARGS, None, &[("AEMLOG_FAKE_AIO_EXIT", "2")]);
    assert_eq!(output.status.code(), Some(1), "stderr={}", stderr(&output));
    assert!(
        stderr(&output).contains("source ended unexpectedly (aio status 2)"),
        "{}",
        stderr(&output)
    );
    let recs = json_lines(&output);
    assert_observable_states(&output, &recs);
    let last = recs.last().unwrap();
    assert_eq!(last["reason"], "unexpected_exit");
    assert_eq!(last["status"], 2);
    assert_eq!(fake.start_count(), 1);
}
