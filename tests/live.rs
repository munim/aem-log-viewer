use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const FAKE_AIO: &str = r#"#!/bin/sh
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

if [ -n "${AEMLOG_FAKE_AIO_HOLD-}" ]; then
  if [ -n "${AEMLOG_FAKE_AIO_PID-}" ]; then
    printf '%s\n' "$$" > "$AEMLOG_FAKE_AIO_PID"
  fi
  if [ -n "${AEMLOG_FAKE_AIO_PGID-}" ]; then
    pgid=$(ps -o pgid= -p $$ 2>/dev/null | tr -d ' \t')
    printf '%s\n' "$pgid" > "$AEMLOG_FAKE_AIO_PGID"
  fi
  descendant=""
  if [ -n "${AEMLOG_FAKE_AIO_DESCENDANT-}" ]; then
    if [ -n "${AEMLOG_FAKE_AIO_IGNORE_TERM-}" ]; then
      ( trap '' TERM
        printf '%s\n' "$$" > "$AEMLOG_FAKE_AIO_DESCENDANT"
        while true; do sleep 1; done
      ) &
    else
      ( printf '%s\n' "$$" > "$AEMLOG_FAKE_AIO_DESCENDANT"
        while true; do sleep 1; done
      ) &
    fi
    descendant=$!
  fi
  if [ -n "${AEMLOG_FAKE_AIO_IGNORE_TERM-}" ]; then
    trap '' TERM
  else
    trap 'exit 0' TERM
  fi
  while true; do sleep 1; done
fi

exit "${AEMLOG_FAKE_AIO_EXIT:-0}"
"#;

struct FakeAio {
    dir: PathBuf,
    record: PathBuf,
    stdin_record: PathBuf,
    logs: PathBuf,
    pid_file: PathBuf,
    pgid_file: PathBuf,
    descendant: PathBuf,
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
            pid_file: dir.join("aio.pid"),
            pgid_file: dir.join("aio.pgid"),
            descendant: dir.join("desc.pid"),
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
    let mut cmd = aemlog();
    cmd.args(args)
        .env_clear()
        .env("PATH", fake.path())
        .env("AEMLOG_FAKE_AIO_RECORD", &fake.record)
        .env("AEMLOG_FAKE_AIO_STDIN_RECORD", &fake.stdin_record)
        .env("AEMLOG_FAKE_AIO_STDERR", "AIO_STDERR_MARKER")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if fake.logs.exists() {
        cmd.env("AEMLOG_FAKE_AIO_LOGS", &fake.logs);
    }
    let output = if let Some(bytes) = stdin {
        cmd.stdin(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn aemlog");
        {
            let mut child_stdin = child.stdin.take().expect("piped stdin");
            child_stdin.write_all(bytes).expect("write stdin");
        }
        child.wait_with_output().expect("wait aemlog")
    } else {
        cmd.stdin(Stdio::null()).output().expect("run aemlog")
    };
    output
}

fn pid_alive(pid: i32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn wait_pid_file(path: &std::path::Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<i32>() {
                if pid > 0 {
                    return pid;
                }
            }
        }
        if Instant::now() >= deadline {
            panic!("pid file {} not written", path.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn process_pgid(pid: u32) -> i32 {
    let output = Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .expect("ps pgid");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("pgid")
}

fn spawn_held_fake(fake: &FakeAio, ignore_term: bool) -> std::process::Child {
    let mut cmd = aemlog();
    cmd.args([
        "--program-id",
        "p1",
        "--environment-id",
        "e1",
        "--service",
        "author",
        "--json",
    ])
    .env_clear()
    .env("PATH", fake.path())
    .env("AEMLOG_FAKE_AIO_RECORD", &fake.record)
    .env("AEMLOG_FAKE_AIO_STDIN_RECORD", &fake.stdin_record)
    .env("AEMLOG_FAKE_AIO_HOLD", "1")
    .env("AEMLOG_FAKE_AIO_PID", &fake.pid_file)
    .env("AEMLOG_FAKE_AIO_PGID", &fake.pgid_file)
    .env("AEMLOG_FAKE_AIO_DESCENDANT", &fake.descendant)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    if ignore_term {
        cmd.env("AEMLOG_FAKE_AIO_IGNORE_TERM", "1");
    }
    cmd.spawn().expect("spawn aemlog")
}

fn read_until_session_started(child: &mut std::process::Child) {
    let stdout = child.stdout.as_mut().expect("piped stdout");
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match stdout.read(&mut byte) {
            Ok(1) => {
                buf.push(byte[0]);
                if byte[0] == b'\n' {
                    let line = String::from_utf8_lossy(&buf);
                    if line.contains("session_started") {
                        return;
                    }
                    buf.clear();
                }
            }
            Ok(0) => panic!(
                "aemlog exited before session_started: {}",
                String::from_utf8_lossy(&buf)
            ),
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => panic!("read aemlog stdout: {err}"),
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for session_started: {}",
                String::from_utf8_lossy(&buf)
            );
        }
    }
}

fn interrupt(pid: u32) {
    let status = Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status()
        .expect("kill -INT");
    assert!(status.success(), "kill -INT {pid} failed");
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

#[test]
fn fake_aio_emits_ndjson_session_groups_and_unexpected_end() {
    let fake = FakeAio::install();
    let output = run_with_fake(
        &fake,
        &[
            "--program-id",
            "p1",
            "--environment-id",
            "e1",
            "--service",
            "author",
            "--json",
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(1), "stderr={}", stderr(&output));
    assert!(
        stderr(&output).contains("source ended unexpectedly"),
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

    assert_eq!(recs[1]["type"], "group_created");
    assert_eq!(recs[1]["group_id"], 1);
    assert_eq!(recs[1]["count"], 1);
    assert_eq!(recs[1]["sample_truncated"], false);
    assert!(recs[1]["fast_rate"].as_f64().unwrap().is_finite());
    let created: Vec<_> = recs
        .iter()
        .filter(|r| r["type"] == "group_created")
        .collect();
    assert_eq!(created.len(), 2);
    assert_eq!(created[1]["group_id"], 2);
    let updates: Vec<_> = recs
        .iter()
        .filter(|r| r["type"] == "group_updated")
        .collect();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["group_id"], 1);
    assert_eq!(updates[0]["count"], 2);
    assert!(updates[0].get("sample").is_none());
    let last = recs.last().unwrap();
    assert_eq!(last["type"], "source_ended");
    assert_eq!(last["status"], 0);
    assert_eq!(last["session_id"], session);
    assert_eq!(last["stderr_truncated"], false);
    assert!(last["stderr"]
        .as_str()
        .unwrap()
        .contains("AIO_STDERR_MARKER"));

    let types: Vec<&str> = recs.iter().map(|r| r["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "session_started",
            "group_created",
            "group_created",
            "group_updated",
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
        .args([
            "--program-id",
            "p1",
            "--environment-id",
            "e1",
            "--service",
            "author",
            "--json",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run aemlog");
    assert_eq!(output.status.code(), Some(1), "stderr={}", stderr(&output));
    assert!(stderr(&output).contains("aio"), "{}", stderr(&output));
    for line in stdout(&output).lines().filter(|line| !line.is_empty()) {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|err| panic!("stdout line is not JSON ({err}): {line}"));
    }
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
    let output = run_with_fake(
        &fake,
        &[
            "--program-id",
            "p1",
            "--environment-id",
            "e1",
            "--service",
            "author",
            "--json",
        ],
        None,
    );
    let recs = json_lines(&output);
    let types: Vec<&str> = recs.iter().map(|r| r["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["session_started", "group_created", "source_ended"]);
    assert!(recs[1]["sample"].as_str().unwrap().contains("keep me"));
}

#[test]
fn multiline_stack_stays_in_one_group_sample() {
    let fake = FakeAio::install();
    fs::write(
        &fake.logs,
        "\
26.08.2026 12:00:00.123 n *ERROR* [t] com.example.Foo boom\n\
java.lang.RuntimeException: boom\n\
\tat com.example.Foo.bar(Foo.java:42)\n\
26.08.2026 12:00:00.124 n *ERROR* [t] com.example.Foo boom\n\
java.lang.RuntimeException: boom\n\
\tat com.example.Foo.bar(Foo.java:42)\n\
26.08.2026 12:00:00.125 n *WARN* [t] ignored\n\
\tat com.example.Bar.skip(Bar.java:1)\n",
    )
    .unwrap();
    let output = run_with_fake(
        &fake,
        &[
            "--program-id",
            "p1",
            "--environment-id",
            "e1",
            "--service",
            "author",
            "--json",
        ],
        None,
    );
    let recs = json_lines(&output);
    let types: Vec<&str> = recs.iter().map(|r| r["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "session_started",
            "group_created",
            "group_updated",
            "source_ended"
        ]
    );
    let sample = recs[1]["sample"].as_str().unwrap();
    assert!(sample.contains("RuntimeException"), "{sample}");
    assert!(sample.contains("com.example.Foo.bar"), "{sample}");
    assert!(!sample.contains("ignored"), "{sample}");
    assert_eq!(recs[2]["count"], 2);
}

#[test]
fn json_redacts_secrets_and_raw_sample_keeps_sample_bodies() {
    let fake = FakeAio::install();
    fs::write(
        &fake.logs,
        "\
26.08.2026 12:00:00.123 n *ERROR* [192.0.2.10 [99] GET /content/site/us/en.html?foo=bar HTTP/1.1] com.example.Foo contact ops@example.com
java.lang.RuntimeException: boom
\tat com.example.Foo.bar(Foo.java:42)
",
    )
    .unwrap();
    let output = run_with_fake(
        &fake,
        &[
            "--program-id",
            "p1",
            "--environment-id",
            "e1",
            "--service",
            "author",
            "--json",
        ],
        None,
    );
    let recs = json_lines(&output);
    let created = recs
        .iter()
        .find(|r| r["type"] == "group_created")
        .expect("group_created");
    let sample = created["sample"].as_str().unwrap();
    assert!(!sample.contains("ops@example.com"), "{sample}");
    assert!(!sample.contains("192.0.2.10"), "{sample}");
    assert!(sample.contains("[REDACTED:email]"), "{sample}");
    assert!(sample.contains("[REDACTED:ip]"), "{sample}");
    assert!(sample.contains("/content/site/us/en.html"), "{sample}");
    assert!(sample.contains("com.example.Foo.bar"), "{sample}");
    assert_eq!(created["request_context"]["client_ip"], "[REDACTED:ip]");
    assert_eq!(
        created["request_context"]["path"],
        "/content/site/us/en.html?foo=[REDACTED:query]"
    );
    assert_eq!(created["terminal_exception"], "java.lang.RuntimeException");
    assert_eq!(created["terminal_frame"], "com.example.Foo.bar");
    assert_eq!(created["timestamp"], "2026-08-26T12:00:00.123Z");
    assert_eq!(created["sample_truncated"], false);
    assert!(created["first_seen"].as_str().unwrap().ends_with('Z'));

    let raw = run_with_fake(
        &fake,
        &[
            "--program-id",
            "p1",
            "--environment-id",
            "e1",
            "--service",
            "author",
            "--json",
            "--raw-sample",
        ],
        None,
    );
    let raw_recs = json_lines(&raw);
    let raw_created = raw_recs
        .iter()
        .find(|r| r["type"] == "group_created")
        .expect("group_created");
    let raw_sample = raw_created["sample"].as_str().unwrap();
    assert!(raw_sample.contains("ops@example.com"), "{raw_sample}");
    assert!(raw_sample.contains("192.0.2.10"), "{raw_sample}");
    assert_eq!(raw_created["request_context"]["client_ip"], "[REDACTED:ip]");
    assert_eq!(
        raw_created["request_context"]["path"],
        "/content/site/us/en.html?foo=[REDACTED:query]"
    );
    assert_eq!(
        raw_created["terminal_exception"],
        "java.lang.RuntimeException"
    );
    assert_eq!(raw_created["terminal_frame"], "com.example.Foo.bar");
    assert_eq!(raw_created["timestamp"], "2026-08-26T12:00:00.123Z");
}

#[test]
fn source_ended_stderr_is_always_redacted() {
    let fake = FakeAio::install();
    fs::write(
        &fake.logs,
        "26.08.2026 12:00:00.123 n *ERROR* [t] com.example.Foo boom\n",
    )
    .unwrap();
    let mut cmd = aemlog();
    cmd.args([
        "--program-id",
        "p1",
        "--environment-id",
        "e1",
        "--service",
        "author",
        "--json",
        "--raw-sample",
    ])
    .env_clear()
    .env("PATH", fake.path())
    .env("AEMLOG_FAKE_AIO_RECORD", &fake.record)
    .env("AEMLOG_FAKE_AIO_STDIN_RECORD", &fake.stdin_record)
    .env("AEMLOG_FAKE_AIO_LOGS", &fake.logs)
    .env(
        "AEMLOG_FAKE_AIO_STDERR",
        "token=supersecret ops@example.com",
    )
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let output = cmd.output().expect("run aemlog");
    let recs = json_lines(&output);
    let ended = recs
        .iter()
        .find(|r| r["type"] == "source_ended")
        .expect("source_ended");
    let stderr_text = ended["stderr"].as_str().unwrap();
    assert!(!stderr_text.contains("supersecret"), "{stderr_text}");
    assert!(!stderr_text.contains("ops@example.com"), "{stderr_text}");
    assert!(stderr_text.contains("[REDACTED:token]"), "{stderr_text}");
    assert!(stderr_text.contains("[REDACTED:email]"), "{stderr_text}");
    assert_eq!(ended["stderr_truncated"], false);
}

#[test]
fn broken_stdout_exits_1() {
    let fake = FakeAio::install();
    fs::write(
        &fake.logs,
        "26.08.2026 12:00:00.123 n *ERROR* [t] com.example.Foo boom\n",
    )
    .unwrap();
    let mut child = aemlog()
        .args([
            "--program-id",
            "p1",
            "--environment-id",
            "e1",
            "--service",
            "author",
            "--json",
        ])
        .env_clear()
        .env("PATH", fake.path())
        .env("AEMLOG_FAKE_AIO_RECORD", &fake.record)
        .env("AEMLOG_FAKE_AIO_STDIN_RECORD", &fake.stdin_record)
        .env("AEMLOG_FAKE_AIO_LOGS", &fake.logs)
        .env("AEMLOG_FAKE_AIO_HOLD", "1")
        .env("AEMLOG_FAKE_AIO_PID", &fake.pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn aemlog");
    drop(child.stdout.take());
    let output = child.wait_with_output().expect("wait aemlog");
    assert_eq!(output.status.code(), Some(1), "stderr={}", stderr(&output));
    if let Ok(pid) = fs::read_to_string(&fake.pid_file) {
        if let Ok(pid) = pid.trim().parse::<i32>() {
            assert!(!pid_alive(pid), "aio orphaned after broken stdout");
        }
    }
}

#[test]
fn parser_diagnostics_redact_unless_raw_sample() {
    let fake = FakeAio::install();
    fs::write(
        &fake.logs,
        "garbage ops@example.com leftover\n\
26.08.2026 12:00:00.123 n *ERROR* [t] com.example.Foo boom\n",
    )
    .unwrap();
    let output = run_with_fake(
        &fake,
        &[
            "--program-id",
            "p1",
            "--environment-id",
            "e1",
            "--service",
            "author",
            "--json",
        ],
        None,
    );
    let err = stderr(&output);
    assert!(err.contains("parser diagnostic"), "{err}");
    assert!(!err.contains("ops@example.com"), "{err}");
    assert!(err.contains("[REDACTED:email]"), "{err}");

    let raw = run_with_fake(
        &fake,
        &[
            "--program-id",
            "p1",
            "--environment-id",
            "e1",
            "--service",
            "author",
            "--json",
            "--raw-sample",
        ],
        None,
    );
    let raw_err = stderr(&raw);
    assert!(raw_err.contains("parser diagnostic"), "{raw_err}");
    assert!(raw_err.contains("ops@example.com"), "{raw_err}");
    let recs = json_lines(&output);
    let parser = recs
        .iter()
        .find(|r| r["type"] == "parser_error")
        .expect("parser_error");
    assert_eq!(parser["reason"], "unframed_prefix");
    assert!(!parser["sample"]
        .as_str()
        .unwrap()
        .contains("ops@example.com"));
    assert!(parser["sample"]
        .as_str()
        .unwrap()
        .contains("[REDACTED:email]"));
}

#[test]
fn ctrl_c_terminates_group_and_exits_0() {
    let fake = FakeAio::install();
    let mut child = spawn_held_fake(&fake, false);
    read_until_session_started(&mut child);
    let analyzer_pid = child.id();
    let analyzer_pgid = process_pgid(analyzer_pid);
    let aio_pid = wait_pid_file(&fake.pid_file);
    let aio_pgid = wait_pid_file(&fake.pgid_file);
    let descendant = wait_pid_file(&fake.descendant);
    assert_ne!(aio_pgid, analyzer_pgid, "aio must not share analyzer pgid");
    assert_eq!(aio_pgid, aio_pid, "aio should lead its process group");
    assert!(pid_alive(aio_pid));
    assert!(pid_alive(descendant));

    interrupt(analyzer_pid);
    let output = child.wait_with_output().expect("wait aemlog");
    assert_eq!(output.status.code(), Some(0), "stderr={}", stderr(&output));
    let recs = json_lines(&output);
    assert_eq!(recs.last().unwrap()["type"], "source_ended");
    assert!(!pid_alive(aio_pid), "aio orphaned after ctrl-c");
    assert!(!pid_alive(descendant), "descendant orphaned after ctrl-c");
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
}

#[test]
fn ctrl_c_sigkills_term_resistant_descendant() {
    let fake = FakeAio::install();
    let mut child = spawn_held_fake(&fake, true);
    read_until_session_started(&mut child);
    let analyzer_pid = child.id();
    let analyzer_pgid = process_pgid(analyzer_pid);
    let aio_pid = wait_pid_file(&fake.pid_file);
    let aio_pgid = wait_pid_file(&fake.pgid_file);
    let descendant = wait_pid_file(&fake.descendant);
    assert_ne!(aio_pgid, analyzer_pgid);
    assert!(pid_alive(descendant));

    let started = Instant::now();
    interrupt(analyzer_pid);
    let output = child.wait_with_output().expect("wait aemlog");
    let elapsed = started.elapsed();
    assert_eq!(output.status.code(), Some(0), "stderr={}", stderr(&output));
    assert!(elapsed >= Duration::from_secs(2), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(6), "{elapsed:?}");
    assert!(!pid_alive(aio_pid), "aio orphaned after forced kill");
    assert!(!pid_alive(descendant), "term-resistant descendant orphaned");
}

#[cfg(unix)]
#[test]
fn pty_volume_updates_and_q_restores_terminal() {
    use std::os::fd::AsRawFd;
    use std::os::unix::io::FromRawFd;
    use std::os::unix::process::CommandExt;

    let fake = FakeAio::install();
    fs::write(
        &fake.logs,
        "\
26.08.2026 12:00:00.123 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo Failed to start bundle
26.08.2026 12:00:00.456 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo Failed to start bundle
26.08.2026 12:00:01.001 author-0 *ERROR* [FelixDispatchQueue] com.example.Baz other error
",
    )
    .expect("logs");

    let mut master = posix_openpt().expect("posix_openpt");
    unlockpt(master.as_raw_fd()).expect("unlockpt");
    let slave_path = ptsname(master.as_raw_fd()).expect("ptsname");
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&slave_path)
        .expect("open slave");
    set_window(master.as_raw_fd(), 120, 40);
    let slave_fd = slave.as_raw_fd();
    let stdin_fd = unsafe { libc::dup(slave_fd) };
    let stdout_fd = unsafe { libc::dup(slave_fd) };
    assert!(stdin_fd >= 0 && stdout_fd >= 0, "dup slave");

    let mut cmd = aemlog();
    cmd.args([
        "--program-id",
        "p1",
        "--environment-id",
        "e1",
        "--service",
        "author",
    ])
    .env_clear()
    .env("PATH", fake.path())
    .env("HOME", &fake.dir)
    .env("TERM", "xterm")
    .env("AEMLOG_FAKE_AIO_RECORD", &fake.record)
    .env("AEMLOG_FAKE_AIO_LOGS", &fake.logs)
    .env("AEMLOG_FAKE_AIO_HOLD", "1")
    .stdin(unsafe { Stdio::from_raw_fd(stdin_fd) })
    .stdout(unsafe { Stdio::from_raw_fd(stdout_fd) })
    .stderr(Stdio::piped());
    let mut child = unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0);
            Ok(())
        })
        .spawn()
        .expect("spawn aemlog on pty")
    };
    drop(slave);

    let screen = wait_for_screen(&mut master, |text| text.contains("Failed to start bundle"));
    assert!(screen.contains("Volume"), "{screen}");
    assert!(
        screen.contains("COUNT") || screen.contains("Failed"),
        "{screen}"
    );
    assert!(
        !screen.contains("Connected"),
        "process state leaked Connected\n{screen}"
    );

    write_all(&mut master, b"j");
    write_all(&mut master, b"q");
    let leftover = wait_exit_draining(&mut child, &mut master, Duration::from_secs(5));
    assert!(
        leftover.contains("[?1049l") || leftover.contains("1049l"),
        "alternate screen not left\n{leftover:?}"
    );
}

#[cfg(unix)]
fn posix_openpt() -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::grantpt(fd) } != 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn unlockpt(fd: i32) -> std::io::Result<()> {
    if unsafe { libc::unlockpt(fd) } != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn ptsname(fd: i32) -> std::io::Result<String> {
    let ptr = unsafe { libc::ptsname(fd) };
    if ptr.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let cstr = unsafe { std::ffi::CStr::from_ptr(ptr) };
    Ok(cstr.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn set_window(fd: i32, cols: u16, rows: u16) {
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &mut size);
    }
}

#[cfg(unix)]
fn write_all(file: &mut std::fs::File, bytes: &[u8]) {
    file.write_all(bytes).expect("write pty");
    let _ = file.flush();
}

#[cfg(unix)]
fn drain(file: &mut std::fs::File) -> String {
    use std::os::fd::AsRawFd;
    let mut buf = [0u8; 8192];
    let mut out = Vec::new();
    set_nonblocking(file.as_raw_fd());
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(unix)]
fn wait_for_screen(file: &mut std::fs::File, pred: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut acc = String::new();
    while Instant::now() < deadline {
        acc.push_str(&drain(file));
        let visible = strip_ansi(&acc);
        if pred(&visible) || pred(&acc) {
            return visible;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("pty screen never matched; last={acc:?}");
}

#[cfg(unix)]
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                for next in chars.by_ref() {
                    if next == '\u{7}' {
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

#[cfg(unix)]
fn set_nonblocking(fd: i32) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

#[cfg(unix)]
fn wait_exit_draining(
    child: &mut std::process::Child,
    master: &mut std::fs::File,
    limit: Duration,
) -> String {
    let deadline = Instant::now() + limit;
    let mut leftover = String::new();
    loop {
        leftover.push_str(&drain(master));
        match child.try_wait() {
            Ok(Some(status)) => {
                leftover.push_str(&drain(master));
                assert_eq!(status.code(), Some(0), "aemlog status {status}");
                return leftover;
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                leftover.push_str(&drain(master));
                panic!("aemlog did not exit after q; leftover={leftover:?}");
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(err) => panic!("wait child: {err}"),
        }
    }
}
