use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn aemlog() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aemlog"))
}

struct Isolate {
    root: PathBuf,
    home: PathBuf,
    cwd: PathBuf,
}

impl Isolate {
    fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("aemlog-cli-{}-{stamp}-{n}", std::process::id()));
        let home = root.join("home");
        let cwd = root.join("cwd");
        fs::create_dir_all(&home).expect("home");
        fs::create_dir_all(&cwd).expect("cwd");
        Self { root, home, cwd }
    }

    fn run(&self, args: &[&str]) -> Output {
        aemlog()
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .current_dir(&self.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run aemlog")
    }
}

impl Drop for Isolate {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn run(args: &[&str]) -> Output {
    Isolate::new().run(args)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn help_documents_the_cli_contract() {
    let output = run(&["--help"]);
    assert_eq!(output.status.code(), Some(0), "stderr={}", stderr(&output));
    let help = stdout(&output);
    for needle in [
        "--program-id",
        "--environment-id",
        "--service",
        "--level",
        "--ims-context",
        "--config",
        "--timezone",
        "--json",
        "--raw-sample",
        "author",
        "publish",
        "ERROR",
        "case-insensitive",
        "Repeatable",
        "utc",
        "TTY",
        "status 2",
        "status 1",
        "~/aemlog.toml",
        "never merged",
    ] {
        assert!(help.contains(needle), "help missing {needle:?}\n{help}");
    }
}

#[test]
fn accepted_json_invocation_without_aio_exits_1() {
    let output = run(&[
        "--program-id",
        "p1",
        "--environment-id",
        "e1",
        "--service",
        "author",
        "--level",
        "error",
        "--level",
        "WARN",
        "--ims-context",
        "ctx",
        "--timezone",
        "UTC",
        "--json",
        "--raw-sample",
    ]);
    assert_eq!(output.status.code(), Some(1), "stderr={}", stderr(&output));
    assert!(stderr(&output).contains("aio"), "{}", stderr(&output));
}

#[test]
fn redirected_stdout_without_json_exits_2_with_json_hint() {
    let output = run(&[
        "--program-id",
        "p1",
        "--environment-id",
        "e1",
        "--service",
        "publish",
    ]);
    assert_eq!(output.status.code(), Some(2), "stderr={}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("--json"), "missing --json hint\n{err}");
    assert!(err.contains("TTY") || err.contains("terminal"), "{err}");
}

#[test]
fn missing_required_args_exit_2() {
    let output = run(&["--json"]);
    assert_eq!(output.status.code(), Some(2), "stderr={}", stderr(&output));
}

#[test]
fn invalid_service_exits_2() {
    let output = run(&[
        "--program-id",
        "p1",
        "--environment-id",
        "e1",
        "--service",
        "preview",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(2), "stderr={}", stderr(&output));
    let err = stderr(&output);
    assert!(err.to_lowercase().contains("author"), "{err}");
    assert!(err.to_lowercase().contains("publish"), "{err}");
}

#[test]
fn invalid_level_exits_2() {
    let output = run(&[
        "--program-id",
        "p1",
        "--environment-id",
        "e1",
        "--service",
        "author",
        "--level",
        "loud",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(2), "stderr={}", stderr(&output));
}

#[test]
fn invalid_timezone_exits_2_with_rejected_value() {
    let output = run(&[
        "--program-id",
        "p1",
        "--environment-id",
        "e1",
        "--service",
        "author",
        "--timezone",
        "Not/AZone",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(2), "stderr={}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("Not/AZone"), "{err}");
}

#[test]
fn empty_program_id_exits_2() {
    let output = run(&[
        "--program-id",
        " ",
        "--environment-id",
        "e1",
        "--service",
        "author",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(2), "stderr={}", stderr(&output));
    assert!(
        stderr(&output).contains("program ID"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn raw_sample_without_json_exits_2() {
    let output = run(&[
        "--program-id",
        "p1",
        "--environment-id",
        "e1",
        "--service",
        "author",
        "--raw-sample",
    ]);
    assert_eq!(output.status.code(), Some(2), "stderr={}", stderr(&output));
}

#[test]
fn missing_explicit_config_exits_2_without_home_fallback() {
    let isolate = Isolate::new();
    fs::write(isolate.home.join("aemlog.toml"), "timezone = \"local\"\n").expect("home config");
    let missing = isolate.root.join("missing.toml");
    let output = isolate.run(&[
        "--program-id",
        "p1",
        "--environment-id",
        "e1",
        "--service",
        "author",
        "--config",
        missing.to_str().expect("utf8 path"),
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(2), "stderr={}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("not found"), "{err}");
    assert!(err.contains("missing.toml"), "{err}");
}

#[test]
fn invalid_explicit_config_exits_2() {
    let isolate = Isolate::new();
    let config = isolate.cwd.join("bad.toml");
    fs::write(&config, "[[[not toml").expect("bad config");
    let output = isolate.run(&[
        "--program-id",
        "p1",
        "--environment-id",
        "e1",
        "--service",
        "author",
        "--config",
        config.to_str().expect("utf8 path"),
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(2), "stderr={}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("invalid config"), "{err}");
}

#[test]
fn valid_explicit_config_exits_0() {
    let isolate = Isolate::new();
    let config = isolate.cwd.join("ok.toml");
    fs::write(&config, "timezone = \"utc\"\n").expect("ok config");
    let output = isolate.run(&[
        "--program-id",
        "p1",
        "--environment-id",
        "e1",
        "--service",
        "author",
        "--config",
        config.to_str().expect("utf8 path"),
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(0), "stderr={}", stderr(&output));
}
