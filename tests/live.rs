use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

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

exit "${AEMLOG_FAKE_AIO_EXIT:-0}"
"#;

struct FakeAio {
    dir: PathBuf,
    record: PathBuf,
    stdin_record: PathBuf,
    logs: PathBuf,
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
    assert_eq!(recs[2]["type"], "group_updated");
    assert_eq!(recs[2]["group_id"], 1);
    assert_eq!(recs[2]["count"], 2);
    assert_eq!(recs[3]["type"], "group_created");
    assert_eq!(recs[3]["group_id"], 2);
    let last = recs.last().unwrap();
    assert_eq!(last["type"], "source_ended");
    assert_eq!(last["status"], 0);
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
