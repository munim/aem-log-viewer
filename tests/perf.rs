use std::process::{Command, Stdio};

fn aemlog() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aemlog"))
}

#[test]
fn production_cli_has_no_offline_input() {
    let output = aemlog()
        .arg("--help")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("help");
    assert_eq!(output.status.code(), Some(0));
    let help = String::from_utf8_lossy(&output.stdout);
    for needle in ["--file", "--input", "offline", "replay"] {
        assert!(
            !help.contains(needle),
            "production CLI grew offline input {needle:?}\n{help}"
        );
    }
}
