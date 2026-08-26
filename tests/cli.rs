use std::process::{Command, Output, Stdio};

fn aemlog() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aemlog"))
}

fn run(args: &[&str]) -> Output {
    aemlog()
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run aemlog")
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
    ] {
        assert!(help.contains(needle), "help missing {needle:?}\n{help}");
    }
}

#[test]
fn accepted_json_invocation_exits_0_without_aio() {
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
        "--config",
        "missing.toml",
        "--timezone",
        "UTC",
        "--json",
        "--raw-sample",
    ]);
    assert_eq!(output.status.code(), Some(0), "stderr={}", stderr(&output));
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
