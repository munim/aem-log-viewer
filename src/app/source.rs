use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use super::cli::Request;
use super::Error;

pub(super) const AIO_PROGRAM: &str = "aio";
pub(super) const AEMERROR: &str = "aemerror";
pub(super) const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

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
    apply_process_group(&mut cmd);
    cmd
}

fn apply_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

fn self_pgid() -> i32 {
    unsafe { libc::getpgrp() }
}

/// Signal a dedicated child process group. Never targets the analyzer group.
pub(super) fn signal_process_group(pgid: i32, signal: i32) -> Result<(), String> {
    if pgid <= 0 {
        return Err("refusing to signal an invalid process group".into());
    }
    if pgid == self_pgid() {
        return Err("refusing to signal the analyzer process group".into());
    }
    let rc = unsafe { libc::kill(-pgid, signal) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err.to_string())
}

fn child_pgid(pid: i32) -> i32 {
    let _ = unsafe { libc::setpgid(pid, pid) };
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid > 0 {
        pgid
    } else {
        pid
    }
}

/// Owns the AIO child and its dedicated Unix process group.
pub(super) struct Source {
    child: Child,
    pid: i32,
    pgid: i32,
    reaped: bool,
}

impl Source {
    pub(super) fn spawn(request: &Request) -> Result<Self, Error> {
        Self::spawn_command(command(request))
    }

    fn spawn_command(mut cmd: Command) -> Result<Self, Error> {
        apply_process_group(&mut cmd);
        let child = cmd.spawn().map_err(|err| Error::Spawn(err.to_string()))?;
        let pid = child.id() as i32;
        Ok(Self {
            child,
            pid,
            pgid: child_pgid(pid),
            reaped: false,
        })
    }

    pub(super) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub(super) fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    #[cfg(test)]
    pub(super) fn pgid(&self) -> i32 {
        self.pgid
    }

    #[cfg(test)]
    pub(super) fn pid(&self) -> i32 {
        self.pid
    }

    fn signal_tree(&self, signal: i32) -> Result<(), String> {
        if self.pgid > 0 && self.pgid != self_pgid() {
            signal_process_group(self.pgid, signal)
        } else {
            let rc = unsafe { libc::kill(self.pid, signal) };
            if rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error().to_string())
            }
        }
    }

    fn take_status(&mut self, status: ExitStatus) -> Option<i32> {
        self.reaped = true;
        status.code()
    }

    pub(super) fn wait(&mut self) -> Result<Option<i32>, Error> {
        if self.reaped {
            return Ok(None);
        }
        self.child
            .wait()
            .map(|status| self.take_status(status))
            .map_err(|err| Error::Io(err.to_string()))
    }

    pub(super) fn try_wait(&mut self) -> Result<Option<Option<i32>>, Error> {
        if self.reaped {
            return Ok(Some(None));
        }
        match self.child.try_wait() {
            Ok(Some(status)) => Ok(Some(self.take_status(status))),
            Ok(None) => Ok(None),
            Err(err) => Err(Error::Io(err.to_string())),
        }
    }

    /// SIGTERM the group, drain-wait up to two seconds, SIGKILL survivors, reap.
    pub(super) fn shutdown(&mut self) -> Result<Option<i32>, Error> {
        if self.reaped {
            return Ok(None);
        }
        let _ = self.signal_tree(libc::SIGTERM);
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Ok(self.take_status(status)),
                Ok(None) if Instant::now() >= deadline => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(err) => return Err(Error::Io(err.to_string())),
            }
        }
        let _ = self.signal_tree(libc::SIGKILL);
        self.child
            .wait()
            .map(|status| self.take_status(status))
            .map_err(|err| Error::Io(err.to_string()))
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        let _ = self.signal_tree(libc::SIGKILL);
        if let Ok(Some(status)) = self.child.try_wait() {
            self.reaped = true;
            let _ = status;
            return;
        }
        if let Ok(status) = self.child.wait() {
            self.reaped = true;
            let _ = status;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

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

    fn pid_alive(pid: i32) -> bool {
        if pid <= 0 {
            return false;
        }
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0
    }

    fn spawn_script(script: &str) -> Source {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Source::spawn_command(cmd).expect("spawn test child")
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
        let ims = "ctx`id`;echo owned";
        let args = tail_log_args(&request(program, environment, Some(ims)));
        assert_eq!(args[2], environment);
        assert_eq!(args[6], program);
        assert_eq!(args[8], ims);
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
    fn child_starts_in_dedicated_process_group() {
        let source = spawn_script("sleep 30");
        assert_eq!(source.pgid(), source.pid());
        assert_ne!(source.pgid(), self_pgid());
        assert!(pid_alive(source.pid()));
        drop(source);
    }

    #[test]
    fn drop_kills_direct_child_and_descendant() {
        let dir = std::env::temp_dir().join(format!(
            "aemlog-source-drop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let desc = dir.join("desc.pid");
        let script = format!(
            r#"
            (sleep 60) &
            echo $! > "{desc}"
            sleep 60
            "#,
            desc = desc.display()
        );
        let source = spawn_script(&script);
        let parent = source.pid();
        let descendant = wait_pid_file(&desc);
        assert_ne!(parent, descendant);
        assert!(pid_alive(parent));
        assert!(pid_alive(descendant));
        drop(source);
        assert!(!pid_alive(parent), "direct child survived drop");
        assert!(!pid_alive(descendant), "descendant survived drop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_sends_sigterm_and_reaps_when_child_exits() {
        let dir = std::env::temp_dir().join(format!(
            "aemlog-source-term-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let ready = dir.join("ready");
        let script = format!(
            r#"
            trap 'exit 0' TERM
            echo $$ > "{ready}"
            while true; do sleep 1; done
            "#,
            ready = ready.display()
        );
        let mut source = spawn_script(&script);
        let _ = wait_pid_file(&ready);
        let started = Instant::now();
        let status = source.shutdown().expect("shutdown");
        assert!(started.elapsed() < SHUTDOWN_GRACE);
        assert_eq!(status, Some(0));
        assert!(!pid_alive(source.pid()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_sigkills_term_resistant_group() {
        let dir = std::env::temp_dir().join(format!(
            "aemlog-source-kill-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let desc = dir.join("desc.pid");
        let script = format!(
            r#"
            ( trap '' TERM
              echo $$ > "{desc}"
              while true; do sleep 1; done
            ) &
            trap '' TERM
            while true; do sleep 1; done
            "#,
            desc = desc.display()
        );
        let mut source = spawn_script(&script);
        let parent = source.pid();
        let descendant = wait_pid_file(&desc);
        let started = Instant::now();
        let _status = source.shutdown().expect("shutdown");
        let elapsed = started.elapsed();
        assert!(elapsed >= SHUTDOWN_GRACE, "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        assert!(!pid_alive(parent), "direct child survived SIGKILL");
        assert!(!pid_alive(descendant), "descendant survived SIGKILL");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn signal_process_group_refuses_analyzer_group() {
        let own = self_pgid();
        let err = signal_process_group(own, libc::SIGUSR1).expect_err("own group");
        assert!(err.contains("analyzer process group"), "{err}");
        assert!(pid_alive(std::process::id() as i32));
        let err = signal_process_group(0, libc::SIGTERM).expect_err("zero");
        assert!(err.contains("invalid"), "{err}");
    }

    fn wait_pid_file(path: &std::path::Path) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
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
}
