use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

#[cfg(test)]
use super::cli::Timezone;
use super::cli::{Level, Request};
use super::evidence::{self, NodeSet, SampleMeta, SampleStore};
use super::frame::{self, Frame, Framer};
use super::rate::{rank_sorted, RateParams, RateSnapshot, RateState, SortColumn, View};
use super::redact::{RedactedRequestContext, Redactor};
use super::source;
use super::template::{BucketKey, TemplateStore};
use super::time::{self, TimeInterpreter};
use super::tuning::{Tuning, MAX_GROUPS};
use super::Error;

struct Group {
    id: u64,
    count: u64,
    first_seen: DateTime<Utc>,
    latest_effective: DateTime<Utc>,
    nodes: NodeSet,
    sample: SampleMeta,
    muted: bool,
    bucket: BucketKey,
    index: usize,
    rate: RateState,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct GroupAggregate {
    pub id: u64,
    pub count: u64,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub template: Vec<String>,
    pub nodes: Vec<String>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub node_count: u64,
    #[cfg_attr(not(test), allow(dead_code))]
    pub nodes_capped: bool,
    pub sample: String,
    pub sample_available: bool,
    pub sample_truncated: bool,
    pub sample_original_bytes: usize,
    pub sample_original_lines: usize,
    pub muted: bool,
    pub fast: f64,
    pub baseline: f64,
    pub level: Level,
    pub logger: String,
    pub terminal_exception: Option<String>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub is_overflow: bool,
    #[cfg_attr(not(test), allow(dead_code))]
    pub capacity_global: u64,
    #[cfg_attr(not(test), allow(dead_code))]
    pub capacity_template_bucket: u64,
}

impl GroupAggregate {
    pub(super) fn rate_snapshot(&self) -> RateSnapshot {
        RateSnapshot {
            id: self.id,
            count: self.count,
            first_seen: self.first_seen,
            last_seen: self.last_seen,
            muted: self.muted,
            fast: self.fast,
            baseline: self.baseline,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CapacityReason {
    Global,
    TemplateBucket,
}

#[cfg_attr(not(test), allow(dead_code))]
struct Overflow {
    id: u64,
    level: Level,
    count: u64,
    first_seen: DateTime<Utc>,
    latest_effective: DateTime<Utc>,
    nodes: NodeSet,
    global: u64,
    template_bucket: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProcessState {
    Starting,
    AioRunning,
    Ended,
}

impl ProcessState {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::AioRunning => "AIO running / awaiting logs",
            Self::Ended => "Ended",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OverflowState {
    None,
    Events(u64),
}

impl OverflowState {
    pub(super) fn label(self) -> String {
        match self {
            Self::None => "none".into(),
            Self::Events(count) => format!("{count}"),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Snapshot {
    pub program_id: String,
    pub environment_id: String,
    pub service: String,
    pub process: ProcessState,
    pub started_at: Instant,
    pub selected_events: u64,
    pub diagnostics: u64,
    pub overflow: OverflowState,
    pub groups: Vec<GroupAggregate>,
    pub now: DateTime<Utc>,
    pub rate_params: RateParams,
    pub generation: u64,
}

impl Snapshot {
    pub(super) fn group_count(&self) -> usize {
        self.groups.len()
    }

    pub(super) fn view_rows(&self, view: View, sort: SortColumn) -> Vec<&GroupAggregate> {
        let snaps: Vec<RateSnapshot> = self
            .groups
            .iter()
            .map(GroupAggregate::rate_snapshot)
            .collect();
        let ranked = rank_sorted(view, sort, &snaps, self.now, &self.rate_params);
        ranked
            .iter()
            .filter_map(|ranked| self.groups.iter().find(|group| group.id == ranked.id))
            .collect()
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum SessionCommand {
    ToggleMute(u64),
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
enum DomainChange {
    Created {
        group_id: u64,
        count: u64,
    },
    Updated {
        group_id: u64,
        count: u64,
    },
    Merged {
        removed_id: u64,
        survivor: GroupAggregate,
    },
}

struct Analyzer {
    session_id: String,
    levels: Vec<Level>,
    groups: HashMap<String, Group>,
    overflows: HashMap<Level, Overflow>,
    removed: HashSet<u64>,
    next_group_id: u64,
    max_groups: usize,
    sample_max_bytes: usize,
    samples: SampleStore,
    times: TimeInterpreter,
    redactor: Redactor,
    raw_sample: bool,
    templates: TemplateStore,
    rate_params: RateParams,
    selected_events: u64,
    diagnostics: u64,
    emit_diagnostics: bool,
    dirty: HashSet<u64>,
    last_record_at: HashMap<u64, Instant>,
}

#[derive(Serialize)]
struct SourceMeta<'a> {
    program_id: &'a str,
    environment_id: &'a str,
    service: &'a str,
    log: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ims_context: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(tag = "type")]
#[allow(clippy::large_enum_variant)]
enum Record<'a> {
    #[serde(rename = "session_started")]
    SessionStarted {
        version: u32,
        session_id: &'a str,
        emitted_at: String,
        source: SourceMeta<'a>,
        levels: Vec<&'a str>,
    },
    #[serde(rename = "group_created")]
    GroupCreated {
        version: u32,
        session_id: &'a str,
        emitted_at: String,
        group_id: u64,
        count: u64,
        first_seen: String,
        last_seen: String,
        template: String,
        nodes: Vec<String>,
        muted: bool,
        level: &'a str,
        sample: String,
        sample_truncated: bool,
        fast_rate: f64,
        baseline_rate: f64,
        timestamp: &'a str,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        time_fallback: bool,
        node: String,
        thread: String,
        logger: String,
        message: String,
        request_context: Option<RedactedRequestContext>,
        terminal_exception: Option<String>,
        terminal_frame: Option<String>,
        source_offsets: frame::SourceOffsets,
    },
    #[serde(rename = "group_merged")]
    GroupMerged {
        version: u32,
        session_id: &'a str,
        emitted_at: String,
        group_id: u64,
        removed_id: u64,
        count: u64,
        first_seen: String,
        last_seen: String,
        template: String,
        nodes: Vec<String>,
        muted: bool,
        level: &'a str,
        sample: String,
        sample_truncated: bool,
        fast_rate: f64,
        baseline_rate: f64,
    },
    #[serde(rename = "group_updated")]
    GroupUpdated {
        version: u32,
        session_id: &'a str,
        emitted_at: String,
        group_id: u64,
        count: u64,
        first_seen: String,
        last_seen: String,
        template: String,
        nodes: Vec<String>,
        muted: bool,
        level: &'a str,
        fast_rate: f64,
        baseline_rate: f64,
    },
    #[serde(rename = "parser_error")]
    ParserError {
        version: u32,
        session_id: &'a str,
        emitted_at: String,
        reason: &'a str,
        count: u64,
        sample: String,
        sample_truncated: bool,
        line: u64,
        offset: u64,
    },
    #[serde(rename = "source_ended")]
    SourceEnded {
        version: u32,
        session_id: &'a str,
        emitted_at: String,
        status: Option<i32>,
        stderr: String,
        stderr_truncated: bool,
    },
}

const STDERR_TAIL: usize = 64 * 1024;
const PIPE_QUEUE: usize = 16;
const UPDATE_COALESCE: Duration = Duration::from_secs(1);

struct StderrTail {
    bytes: Vec<u8>,
    truncated: bool,
}

impl StderrTail {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        if chunk.len() > STDERR_TAIL {
            self.bytes.clear();
            self.bytes
                .extend_from_slice(&chunk[chunk.len() - STDERR_TAIL..]);
            self.truncated = true;
            return;
        }
        if chunk.len() == STDERR_TAIL {
            self.truncated |= !self.bytes.is_empty();
            self.bytes.clear();
            self.bytes.extend_from_slice(chunk);
            return;
        }
        let overflow = self
            .bytes
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(STDERR_TAIL);
        if overflow > 0 {
            self.bytes.drain(..overflow);
            self.truncated = true;
        }
        self.bytes.extend_from_slice(chunk);
    }

    fn snapshot(&self) -> (String, bool) {
        (
            String::from_utf8_lossy(&self.bytes).into_owned(),
            self.truncated,
        )
    }
}

static USER_STOP: AtomicBool = AtomicBool::new(false);

const STOP_POLL: Duration = Duration::from_millis(50);

extern "C" fn on_sigint(_: libc::c_int) {
    USER_STOP.store(true, Ordering::SeqCst);
}

fn install_user_stop_handler() {
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

pub(super) fn run(request: &Request) -> Result<(), Error> {
    install_user_stop_handler();
    let mut source = source::Source::spawn(request)?;
    let stdout = source
        .take_stdout()
        .ok_or_else(|| Error::Io("aio stdout was not piped".into()))?;
    let stderr = source
        .take_stderr()
        .ok_or_else(|| Error::Io("aio stderr was not piped".into()))?;
    let tail = Arc::new(Mutex::new(StderrTail::new()));
    let drain_tail = Arc::clone(&tail);
    let drain = std::thread::spawn(move || {
        let mut stderr = stderr;
        let mut buf = [0u8; 8192];
        loop {
            match stderr.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => drain_tail.lock().expect("stderr tail").push(&buf[..n]),
            }
        }
    });

    let (tx, rx) = mpsc::sync_channel::<Option<Vec<u8>>>(PIPE_QUEUE);
    let reader = std::thread::spawn(move || {
        let mut stdout = stdout;
        let mut buf = [0u8; 8192];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(None);
                    break;
                }
                Ok(n) => {
                    if tx.send(Some(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = tx.send(None);
                    break;
                }
            }
        }
    });

    let mut analyzer = Analyzer::with_tuning(
        request.levels.clone(),
        TimeInterpreter::new(request.timezone),
        Redactor::new(request.tuning.extra_patterns.clone()),
        request.raw_sample,
        &request.tuning,
    )
    .with_rate_params(RateParams::from_tuning(&request.tuning));
    let mut framer = Framer::with_limits(
        request.tuning.event_max_bytes,
        request.tuning.event_max_lines,
        request.tuning.sample_max_bytes,
    );
    let mut out = std::io::stdout().lock();
    analyzer.emit_session_started(request, &mut out)?;

    let mut user_stop = false;
    let mut broken_stdout = false;
    loop {
        if USER_STOP.load(Ordering::SeqCst) {
            user_stop = true;
            break;
        }
        match rx.recv_timeout(STOP_POLL) {
            Ok(Some(chunk)) => {
                if let Err(err) =
                    accept_frames(&mut analyzer, framer.push(&chunk, Instant::now()), &mut out)
                {
                    if is_broken_pipe(&err) {
                        broken_stdout = true;
                        break;
                    }
                    return Err(err);
                }
            }
            Ok(None) => {
                if let Err(err) = accept_frames(&mut analyzer, framer.finish(), &mut out) {
                    if is_broken_pipe(&err) {
                        broken_stdout = true;
                        break;
                    }
                    return Err(err);
                }
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Err(err) =
                    accept_frames(&mut analyzer, framer.poll_idle(Instant::now()), &mut out)
                {
                    if is_broken_pipe(&err) {
                        broken_stdout = true;
                        break;
                    }
                    return Err(err);
                }
                if let Err(err) = analyzer.flush_due_updates(Instant::now(), &mut out) {
                    if is_broken_pipe(&err) {
                        broken_stdout = true;
                        break;
                    }
                    return Err(err);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Err(err) = accept_frames(&mut analyzer, framer.finish(), &mut out) {
                    if is_broken_pipe(&err) {
                        broken_stdout = true;
                        break;
                    }
                    return Err(err);
                }
                break;
            }
        }
    }

    if USER_STOP.load(Ordering::SeqCst) {
        user_stop = true;
    }

    let status = if user_stop || broken_stdout {
        source.shutdown()?
    } else {
        source.wait()?
    };
    let _ = reader.join();
    let _ = drain.join();
    let (stderr_raw, stderr_truncated) = tail.lock().expect("stderr tail").snapshot();
    let stderr_text = analyzer.redactor.redact(&stderr_raw);
    if !broken_stdout {
        analyzer.flush_pending_updates(Instant::now(), &mut out)?;
        analyzer.emit_source_ended(status, stderr_text, stderr_truncated, &mut out)?;
    }
    if broken_stdout {
        Err(Error::Io("broken stdout".into()))
    } else if user_stop {
        Ok(())
    } else {
        Err(Error::UnexpectedEnd(
            status
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".into()),
        ))
    }
}

pub(super) struct LiveSession {
    snapshots: std::sync::Arc<std::sync::Mutex<Snapshot>>,
    commands: mpsc::Sender<SessionCommand>,
    stop: std::sync::Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<Result<(), Error>>>,
}

impl LiveSession {
    pub(super) fn start(request: Request) -> Result<Self, Error> {
        let started_at = Instant::now();
        let mut source = source::Source::spawn(&request)?;
        let stdout = source
            .take_stdout()
            .ok_or_else(|| Error::Io("aio stdout was not piped".into()))?;
        let stderr = source
            .take_stderr()
            .ok_or_else(|| Error::Io("aio stderr was not piped".into()))?;
        let snapshots = std::sync::Arc::new(std::sync::Mutex::new(Snapshot {
            program_id: request.program_id.clone(),
            environment_id: request.environment_id.clone(),
            service: request.service.as_str().to_owned(),
            process: ProcessState::Starting,
            started_at,
            selected_events: 0,
            diagnostics: 0,
            overflow: OverflowState::None,
            groups: Vec::new(),
            now: Utc::now(),
            rate_params: RateParams::from_tuning(&request.tuning),
            generation: 0,
        }));
        let (commands, command_rx) = mpsc::channel();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let published = snapshots.clone();
        let stop_flag = stop.clone();
        let worker = std::thread::Builder::new()
            .name("aemlog-ingest".into())
            .spawn(move || {
                run_live(
                    request, source, stdout, stderr, published, command_rx, stop_flag, started_at,
                )
            })
            .map_err(|err| Error::Io(err.to_string()))?;
        Ok(Self {
            snapshots,
            commands,
            stop,
            worker: Some(worker),
        })
    }

    pub(super) fn snapshot(&self) -> Snapshot {
        self.snapshots
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    pub(super) fn toggle_mute(&self, group_id: u64) {
        let _ = self.commands.send(SessionCommand::ToggleMute(group_id));
    }

    pub(super) fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub(super) fn finished(&self) -> bool {
        self.worker
            .as_ref()
            .map(std::thread::JoinHandle::is_finished)
            .unwrap_or(true)
    }

    pub(super) fn join(mut self) -> Result<(), Error> {
        self.request_stop();
        match self.worker.take() {
            Some(worker) => worker
                .join()
                .unwrap_or_else(|_| Err(Error::Io("ingest thread panicked".into()))),
            None => Ok(()),
        }
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn publish_snapshot(
    snapshots: &std::sync::Arc<std::sync::Mutex<Snapshot>>,
    mut snapshot: Snapshot,
) {
    let mut guard = snapshots.lock().unwrap_or_else(|err| err.into_inner());
    snapshot.generation = guard.generation.saturating_add(1);
    *guard = snapshot;
}

#[allow(clippy::too_many_arguments)]
fn run_live(
    request: Request,
    mut source: source::Source,
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
    snapshots: std::sync::Arc<std::sync::Mutex<Snapshot>>,
    commands: mpsc::Receiver<SessionCommand>,
    stop: std::sync::Arc<AtomicBool>,
    started_at: Instant,
) -> Result<(), Error> {
    let drain = std::thread::spawn(move || {
        let mut stderr = stderr;
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
    });

    let (tx, rx) = mpsc::channel::<Option<Vec<u8>>>();
    let reader = std::thread::spawn(move || {
        let mut stdout = stdout;
        let mut buf = [0u8; 8192];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(None);
                    break;
                }
                Ok(n) => {
                    if tx.send(Some(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = tx.send(None);
                    break;
                }
            }
        }
    });

    let mut analyzer = Analyzer::with_tuning(
        request.levels.clone(),
        TimeInterpreter::new(request.timezone),
        Redactor::new(request.tuning.extra_patterns.clone()),
        request.raw_sample,
        &request.tuning,
    )
    .with_rate_params(RateParams::from_tuning(&request.tuning));
    let mut framer = Framer::with_limits(
        request.tuning.event_max_bytes,
        request.tuning.event_max_lines,
        request.tuning.sample_max_bytes,
    );
    let mut sink = std::io::sink();
    analyzer.emit_diagnostics = false;
    let mut process = ProcessState::AioRunning;
    publish_snapshot(&snapshots, analyzer.snapshot(&request, process, started_at));

    let mut user_stop = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            user_stop = true;
            break;
        }
        let mut dirty = drain_commands(&mut analyzer, &commands);
        match rx.recv_timeout(STOP_POLL) {
            Ok(Some(chunk)) => {
                accept_frames_with(
                    &mut analyzer,
                    framer.push(&chunk, Instant::now()),
                    &mut sink,
                    false,
                )?;
                dirty = true;
            }
            Ok(None) => {
                accept_frames_with(&mut analyzer, framer.finish(), &mut sink, false)?;
                dirty |= drain_commands(&mut analyzer, &commands);
                if dirty {
                    publish_snapshot(&snapshots, analyzer.snapshot(&request, process, started_at));
                }
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let frames = framer.poll_idle(Instant::now());
                if !frames.is_empty() {
                    accept_frames_with(&mut analyzer, frames, &mut sink, false)?;
                    dirty = true;
                }
                if source.try_wait()?.is_some() {
                    if dirty {
                        publish_snapshot(
                            &snapshots,
                            analyzer.snapshot(&request, process, started_at),
                        );
                    }
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                accept_frames_with(&mut analyzer, framer.finish(), &mut sink, false)?;
                dirty |= drain_commands(&mut analyzer, &commands);
                if dirty {
                    publish_snapshot(&snapshots, analyzer.snapshot(&request, process, started_at));
                }
                break;
            }
        }
        if dirty {
            publish_snapshot(&snapshots, analyzer.snapshot(&request, process, started_at));
        }
    }

    if stop.load(Ordering::SeqCst) {
        user_stop = true;
    }
    let status = if user_stop {
        source.shutdown()?
    } else {
        source.wait()?
    };
    let _ = reader.join();
    let _ = drain.join();
    process = ProcessState::Ended;
    publish_snapshot(&snapshots, analyzer.snapshot(&request, process, started_at));
    if user_stop {
        Ok(())
    } else {
        Err(Error::UnexpectedEnd(
            status
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".into()),
        ))
    }
}

fn drain_commands(analyzer: &mut Analyzer, commands: &mpsc::Receiver<SessionCommand>) -> bool {
    let mut changed = false;
    while let Ok(command) = commands.try_recv() {
        match command {
            SessionCommand::ToggleMute(group_id) => {
                changed |= analyzer.toggle_mute(group_id);
            }
        }
    }
    changed
}

fn accept_frames(
    analyzer: &mut Analyzer,
    frames: Vec<Frame>,
    out: &mut impl Write,
) -> Result<(), Error> {
    accept_frames_with(analyzer, frames, out, true)
}

fn accept_frames_with(
    analyzer: &mut Analyzer,
    frames: Vec<Frame>,
    out: &mut impl Write,
    emit_diagnostics: bool,
) -> Result<(), Error> {
    for item in frames {
        match item {
            Frame::Event(event) => analyzer.ingest(&event, Utc::now(), Instant::now(), out)?,
            Frame::Diagnostic(diag) => {
                analyzer.diagnostics = analyzer.diagnostics.saturating_add(diag.count.max(1));
                if emit_diagnostics {
                    eprintln!(
                        "parser diagnostic: {} count={} line={} offset={} sample={}",
                        diag.reason.as_str(),
                        diag.count,
                        diag.line,
                        diag.offset,
                        analyzer
                            .redactor
                            .redact_sample(&diag.sample, analyzer.raw_sample)
                            .trim_end_matches(['\r', '\n']),
                    );
                    analyzer.emit_parser_error(&diag, out)?;
                }
            }
        }
    }
    Ok(())
}

impl Analyzer {
    #[cfg(test)]
    fn new(levels: Vec<Level>) -> Self {
        let tuning = Tuning {
            sample_max_bytes: u64::MAX,
            ..Tuning::default()
        };
        Self::with_tuning(
            levels,
            TimeInterpreter::new(Timezone::Utc),
            Redactor::default(),
            false,
            &tuning,
        )
    }

    fn with_tuning(
        levels: Vec<Level>,
        times: TimeInterpreter,
        redactor: Redactor,
        raw_sample: bool,
        tuning: &Tuning,
    ) -> Self {
        Self {
            session_id: Uuid::new_v4().to_string(),
            levels,
            groups: HashMap::new(),
            overflows: HashMap::new(),
            removed: HashSet::new(),
            next_group_id: 1,
            max_groups: (tuning.max_groups as usize).min(MAX_GROUPS as usize),
            sample_max_bytes: usize::try_from(tuning.sample_max_bytes)
                .unwrap_or(usize::MAX)
                .max(1),
            samples: SampleStore::new(
                usize::try_from(tuning.sample_budget_bytes)
                    .unwrap_or(usize::MAX)
                    .max(1),
            ),
            times,
            redactor,
            raw_sample,
            templates: TemplateStore::new(tuning.similarity, tuning.bucket_cap),
            rate_params: RateParams::default(),
            selected_events: 0,
            diagnostics: 0,
            emit_diagnostics: true,
            dirty: HashSet::new(),
            last_record_at: HashMap::new(),
        }
    }

    fn with_rate_params(mut self, rate_params: RateParams) -> Self {
        self.rate_params = rate_params;
        self
    }

    fn emit_session_started(&self, request: &Request, out: &mut impl Write) -> Result<(), Error> {
        let levels: Vec<&str> = self.levels.iter().map(|level| level.as_str()).collect();
        emit(
            out,
            &Record::SessionStarted {
                version: 1,
                session_id: &self.session_id,
                emitted_at: now_utc(),
                source: SourceMeta {
                    program_id: &request.program_id,
                    environment_id: &request.environment_id,
                    service: request.service.as_str(),
                    log: source::AEMERROR,
                    ims_context: request.ims_context.as_deref(),
                },
                levels,
            },
        )
    }

    fn ingest(
        &mut self,
        event: &str,
        arrived_at: DateTime<Utc>,
        now: Instant,
        out: &mut impl Write,
    ) -> Result<(), Error> {
        let Some(metadata) = frame::parse_metadata(event) else {
            return Ok(());
        };
        if !self.levels.contains(&metadata.level) {
            return Ok(());
        }
        self.selected_events += 1;
        let interpreted = self.times.interpret(metadata.timestamp, arrived_at);
        if let Some(fault) = interpreted.fallback {
            self.diagnostics = self.diagnostics.saturating_add(1);
            if self.emit_diagnostics {
                eprintln!(
                    "parser diagnostic: {} sample={}",
                    fault.as_str(),
                    self.redactor
                        .redact_sample(metadata.timestamp, self.raw_sample)
                );
            }
        }
        let outcome = self.templates.learn_allowing(
            metadata.level,
            metadata.logger,
            metadata.terminal_exception,
            metadata.terminal_frame,
            metadata.message,
            self.groups.len() < self.max_groups,
        );
        let node = self.redactor.redact(metadata.node);
        let Some(key) = outcome.group_key() else {
            let reason = match outcome {
                super::template::LearnOutcome::Capacity { .. } => CapacityReason::TemplateBucket,
                _ => CapacityReason::Global,
            };
            self.record_overflow(metadata.level, interpreted.instant, node, reason);
            return Ok(());
        };
        let changes = if let Some(group) = self.groups.get_mut(&key) {
            group.count += 1;
            if interpreted.instant < group.first_seen {
                group.first_seen = interpreted.instant;
            }
            group.latest_effective =
                time::clamp_effective(Some(group.latest_effective), interpreted.instant);
            group
                .rate
                .observe(group.latest_effective, &self.rate_params);
            group.nodes.insert(node);
            vec![DomainChange::Updated {
                group_id: group.id,
                count: group.count,
            }]
        } else if self.groups.len() >= self.max_groups {
            self.record_overflow(
                metadata.level,
                interpreted.instant,
                node,
                CapacityReason::Global,
            );
            Vec::new()
        } else {
            let group_id = self.next_group_id;
            self.next_group_id += 1;
            let captured = evidence::capture(
                &self.redactor.redact_sample(event, self.raw_sample),
                self.sample_max_bytes,
            );
            let sample = captured.text.clone();
            let sample_truncated = captured.meta.truncated;
            self.samples.insert(group_id, captured.text);
            let bucket = outcome.bucket().expect("learned bucket").clone();
            let index = outcome.index().expect("learned index");
            let rate = RateState::first(interpreted.instant, &self.rate_params);
            self.groups.insert(
                key.clone(),
                Group {
                    id: group_id,
                    count: 1,
                    first_seen: interpreted.instant,
                    latest_effective: interpreted.instant,
                    nodes: NodeSet::singleton(node.clone()),
                    sample: captured.meta,
                    muted: false,
                    bucket,
                    index,
                    rate,
                },
            );
            let timestamp = time::rfc3339_millis(interpreted.instant);
            let group = self.groups.get(&key).expect("just inserted");
            let (fast_rate, baseline_rate) = rounded_rates(group, &self.rate_params);
            let template = self.template_text(&group.bucket, group.index);
            emit(
                out,
                &Record::GroupCreated {
                    version: 1,
                    session_id: &self.session_id,
                    emitted_at: now_utc(),
                    group_id,
                    count: 1,
                    first_seen: time::rfc3339_millis(interpreted.instant),
                    last_seen: time::rfc3339_millis(interpreted.instant),
                    template,
                    nodes: vec![node.clone()],
                    muted: false,
                    level: metadata.level.as_str(),
                    sample,
                    sample_truncated,
                    fast_rate,
                    baseline_rate,
                    timestamp: &timestamp,
                    time_fallback: interpreted.fallback.is_some(),
                    node,
                    thread: self.redactor.redact(metadata.thread),
                    logger: self.redactor.redact(metadata.logger),
                    message: self.redactor.redact(metadata.message),
                    request_context: metadata
                        .request_context
                        .as_ref()
                        .map(|ctx| self.redactor.request_context(ctx)),
                    terminal_exception: metadata
                        .terminal_exception
                        .map(|value| self.redactor.redact(value)),
                    terminal_frame: metadata
                        .terminal_frame
                        .map(|value| self.redactor.redact(value)),
                    source_offsets: metadata.offsets,
                },
            )?;
            self.last_record_at.insert(group_id, now);
            vec![DomainChange::Created { group_id, count: 1 }]
        };
        let mut emitted = changes;
        emitted.extend(self.apply_merges(&outcome));
        self.emit_domain_changes(&emitted, now, out)
    }

    fn record_overflow(
        &mut self,
        level: Level,
        instant: DateTime<Utc>,
        node: String,
        reason: CapacityReason,
    ) {
        if let Some(overflow) = self.overflows.get_mut(&level) {
            overflow.count += 1;
            if instant < overflow.first_seen {
                overflow.first_seen = instant;
            }
            overflow.latest_effective =
                time::clamp_effective(Some(overflow.latest_effective), instant);
            overflow.nodes.insert(node);
            match reason {
                CapacityReason::Global => overflow.global += 1,
                CapacityReason::TemplateBucket => overflow.template_bucket += 1,
            }
            return;
        }
        let id = self.next_group_id;
        self.next_group_id += 1;
        let (global, template_bucket) = match reason {
            CapacityReason::Global => (1, 0),
            CapacityReason::TemplateBucket => (0, 1),
        };
        self.overflows.insert(
            level,
            Overflow {
                id,
                level,
                count: 1,
                first_seen: instant,
                latest_effective: instant,
                nodes: NodeSet::singleton(node),
                global,
                template_bucket,
            },
        );
    }

    fn apply_merges(&mut self, outcome: &super::template::LearnOutcome) -> Vec<DomainChange> {
        let Some(bucket) = outcome.bucket() else {
            return Vec::new();
        };
        let mut changes = Vec::new();
        for merge in outcome.merges() {
            let survivor_key = bucket.group_key(merge.survivor);
            let removed_key = bucket.group_key(merge.removed);
            let Some(removed) = self.groups.remove(&removed_key) else {
                continue;
            };
            let Some(survivor) = self.groups.remove(&survivor_key) else {
                self.groups.insert(removed_key, removed);
                continue;
            };
            let (mut keep, drop) = if survivor.id <= removed.id {
                (survivor, removed)
            } else {
                (removed, survivor)
            };
            keep.count += drop.count;
            if drop.first_seen < keep.first_seen {
                keep.first_seen = drop.first_seen;
            }
            keep.latest_effective =
                time::clamp_effective(Some(keep.latest_effective), drop.latest_effective);
            keep.rate = keep.rate.merge(drop.rate, &self.rate_params);
            keep.nodes.merge(drop.nodes);
            if !self.samples.contains(keep.id) && self.samples.contains(drop.id) {
                keep.sample = drop.sample;
            }
            keep.muted |= drop.muted;
            keep.bucket = bucket.clone();
            keep.index = merge.survivor;
            let removed_id = drop.id;
            self.samples.merge(keep.id, removed_id);
            self.removed.insert(removed_id);
            self.dirty.remove(&removed_id);
            self.last_record_at.remove(&removed_id);
            let aggregate = self.aggregate(&keep);
            self.groups.insert(survivor_key, keep);
            changes.push(DomainChange::Merged {
                removed_id,
                survivor: aggregate,
            });
        }
        changes
    }

    fn aggregate(&self, group: &Group) -> GroupAggregate {
        self.aggregate_at(group, group.latest_effective)
    }

    fn aggregate_at(&self, group: &Group, now: DateTime<Utc>) -> GroupAggregate {
        let (fast, baseline) = group.rate.rates_at(now, &self.rate_params);
        GroupAggregate {
            id: group.id,
            count: group.count,
            first_seen: group.first_seen,
            last_seen: group.latest_effective,
            template: self
                .templates
                .template(&group.bucket, group.index)
                .unwrap_or(&[])
                .to_vec(),
            nodes: group.nodes.ids().cloned().collect(),
            node_count: group.nodes.count(),
            nodes_capped: group.nodes.capped(),
            sample: self.samples.get(group.id).unwrap_or("").to_owned(),
            sample_available: self.samples.contains(group.id),
            sample_truncated: group.sample.truncated,
            sample_original_bytes: group.sample.original_bytes,
            sample_original_lines: group.sample.original_lines,
            muted: group.muted,
            fast,
            baseline,
            level: group.bucket.level,
            logger: group.bucket.logger.clone(),
            terminal_exception: group.bucket.terminal_exception.clone(),
            is_overflow: false,
            capacity_global: 0,
            capacity_template_bucket: 0,
        }
    }

    #[cfg(test)]
    fn overflow_aggregate(overflow: &Overflow) -> GroupAggregate {
        GroupAggregate {
            id: overflow.id,
            count: overflow.count,
            first_seen: overflow.first_seen,
            last_seen: overflow.latest_effective,
            template: Vec::new(),
            nodes: overflow.nodes.ids().cloned().collect(),
            node_count: overflow.nodes.count(),
            nodes_capped: overflow.nodes.capped(),
            sample: String::new(),
            sample_available: false,
            sample_truncated: false,
            sample_original_bytes: 0,
            sample_original_lines: 0,
            muted: false,
            fast: 0.0,
            baseline: 0.0,
            level: overflow.level,
            logger: String::new(),
            terminal_exception: None,
            is_overflow: true,
            capacity_global: overflow.global,
            capacity_template_bucket: overflow.template_bucket,
        }
    }

    fn snapshot(&self, request: &Request, process: ProcessState, started_at: Instant) -> Snapshot {
        let now = self.snapshot_now();
        Snapshot {
            program_id: request.program_id.clone(),
            environment_id: request.environment_id.clone(),
            service: request.service.as_str().to_owned(),
            process,
            started_at,
            selected_events: self.selected_events,
            diagnostics: self.diagnostics,
            overflow: {
                let count: u64 = self.overflows.values().map(|overflow| overflow.count).sum();
                if count == 0 {
                    OverflowState::None
                } else {
                    OverflowState::Events(count)
                }
            },
            groups: self.visible_groups_at(now),
            now,
            rate_params: self.rate_params,
            generation: 0,
        }
    }

    fn snapshot_now(&self) -> DateTime<Utc> {
        self.groups
            .values()
            .filter(|group| !self.removed.contains(&group.id))
            .map(|group| group.latest_effective)
            .max()
            .unwrap_or_else(Utc::now)
    }

    fn visible_groups_at(&self, now: DateTime<Utc>) -> Vec<GroupAggregate> {
        let mut groups: Vec<GroupAggregate> = self
            .groups
            .values()
            .filter(|group| !self.removed.contains(&group.id))
            .map(|group| self.aggregate_at(group, now))
            .collect();
        groups.sort_by_key(|group| group.id);
        groups
    }

    #[cfg(test)]
    fn snapshot_groups(&self) -> Vec<GroupAggregate> {
        let mut groups: Vec<GroupAggregate> = self
            .visible_groups_at(self.snapshot_now())
            .into_iter()
            .chain(self.overflows.values().map(Self::overflow_aggregate))
            .collect();
        groups.sort_by(|a, b| {
            a.is_overflow
                .cmp(&b.is_overflow)
                .then_with(|| a.id.cmp(&b.id))
        });
        groups
    }

    fn emit_domain_changes(
        &mut self,
        changes: &[DomainChange],
        now: Instant,
        out: &mut impl Write,
    ) -> Result<(), Error> {
        let merged = changes
            .iter()
            .any(|change| matches!(change, DomainChange::Merged { .. }));
        for change in changes {
            match change {
                DomainChange::Created { .. } => {}
                DomainChange::Updated { group_id, .. } => {
                    if merged || self.removed.contains(group_id) {
                        continue;
                    }
                    self.note_update(*group_id, now, out)?;
                }
                DomainChange::Merged {
                    removed_id,
                    survivor,
                } => {
                    let Some(group) = self.group_by_id(survivor.id) else {
                        continue;
                    };
                    let (fast_rate, baseline_rate) = rounded_rates(group, &self.rate_params);
                    emit(
                        out,
                        &Record::GroupMerged {
                            version: 1,
                            session_id: &self.session_id,
                            emitted_at: now_utc(),
                            group_id: survivor.id,
                            removed_id: *removed_id,
                            count: survivor.count,
                            first_seen: time::rfc3339_millis(survivor.first_seen),
                            last_seen: time::rfc3339_millis(survivor.last_seen),
                            template: survivor.template.join(" "),
                            nodes: survivor.nodes.clone(),
                            muted: survivor.muted,
                            level: survivor.level.as_str(),
                            sample: survivor.sample.clone(),
                            sample_truncated: survivor.sample_truncated,
                            fast_rate,
                            baseline_rate,
                        },
                    )?;
                    self.dirty.remove(&survivor.id);
                    self.last_record_at.insert(survivor.id, now);
                }
            }
        }
        Ok(())
    }

    fn note_update(
        &mut self,
        group_id: u64,
        now: Instant,
        out: &mut impl Write,
    ) -> Result<(), Error> {
        if self.can_emit_update(group_id, now) {
            self.emit_group_updated(group_id, now, out)
        } else {
            self.dirty.insert(group_id);
            Ok(())
        }
    }

    fn can_emit_update(&self, group_id: u64, now: Instant) -> bool {
        match self.last_record_at.get(&group_id) {
            None => true,
            Some(last) => now.duration_since(*last) >= UPDATE_COALESCE,
        }
    }

    fn emit_group_updated(
        &mut self,
        group_id: u64,
        now: Instant,
        out: &mut impl Write,
    ) -> Result<(), Error> {
        let Some(group) = self.group_by_id(group_id) else {
            self.dirty.remove(&group_id);
            return Ok(());
        };
        let (fast_rate, baseline_rate) = rounded_rates(group, &self.rate_params);
        let count = group.count;
        let first_seen = time::rfc3339_millis(group.first_seen);
        let last_seen = time::rfc3339_millis(group.latest_effective);
        let template = self.template_text(&group.bucket, group.index);
        let nodes: Vec<String> = group.nodes.ids().cloned().collect();
        let muted = group.muted;
        let level = group.bucket.level.as_str();
        emit(
            out,
            &Record::GroupUpdated {
                version: 1,
                session_id: &self.session_id,
                emitted_at: now_utc(),
                group_id,
                count,
                first_seen,
                last_seen,
                template,
                nodes,
                muted,
                level,
                fast_rate,
                baseline_rate,
            },
        )?;
        self.dirty.remove(&group_id);
        self.last_record_at.insert(group_id, now);
        Ok(())
    }

    fn flush_due_updates(&mut self, now: Instant, out: &mut impl Write) -> Result<(), Error> {
        let mut due: Vec<u64> = self
            .dirty
            .iter()
            .copied()
            .filter(|id| self.can_emit_update(*id, now))
            .collect();
        due.sort_unstable();
        for group_id in due {
            self.emit_group_updated(group_id, now, out)?;
        }
        Ok(())
    }

    fn flush_pending_updates(&mut self, now: Instant, out: &mut impl Write) -> Result<(), Error> {
        let mut pending: Vec<u64> = self.dirty.iter().copied().collect();
        pending.sort_unstable();
        for group_id in pending {
            self.emit_group_updated(group_id, now, out)?;
        }
        Ok(())
    }

    fn emit_parser_error(
        &self,
        diag: &frame::Diagnostic,
        out: &mut impl Write,
    ) -> Result<(), Error> {
        let redacted = self.redactor.redact_sample(&diag.sample, self.raw_sample);
        let sample_truncated = redacted.len() > self.sample_max_bytes;
        emit(
            out,
            &Record::ParserError {
                version: 1,
                session_id: &self.session_id,
                emitted_at: now_utc(),
                reason: diag.reason.as_str(),
                count: diag.count,
                sample: frame::bound_sample(&redacted, self.sample_max_bytes),
                sample_truncated,
                line: diag.line,
                offset: diag.offset,
            },
        )
    }

    fn group_by_id(&self, group_id: u64) -> Option<&Group> {
        self.groups
            .values()
            .find(|group| group.id == group_id && !self.removed.contains(&group.id))
    }

    fn template_text(&self, bucket: &BucketKey, index: usize) -> String {
        self.templates
            .template(bucket, index)
            .unwrap_or(&[])
            .join(" ")
    }

    #[cfg(test)]
    fn rate_snapshots(&self, now: DateTime<Utc>) -> Vec<RateSnapshot> {
        self.groups
            .values()
            .filter(|group| !self.removed.contains(&group.id))
            .map(|group| {
                let (fast, baseline) = group.rate.rates_at(now, &self.rate_params);
                RateSnapshot {
                    id: group.id,
                    count: group.count,
                    first_seen: group.first_seen,
                    last_seen: group.latest_effective,
                    muted: group.muted,
                    fast,
                    baseline,
                }
            })
            .collect()
    }

    #[cfg(test)]
    fn ranked(&self, view: View, now: DateTime<Utc>) -> Vec<RateSnapshot> {
        super::rate::rank(view, &self.rate_snapshots(now), now, &self.rate_params)
    }

    #[cfg(test)]
    fn normal_group_count(&self) -> usize {
        self.groups
            .values()
            .filter(|group| !self.removed.contains(&group.id))
            .count()
    }

    fn toggle_mute(&mut self, group_id: u64) -> bool {
        if self.removed.contains(&group_id) {
            return false;
        }
        for group in self.groups.values_mut() {
            if group.id == group_id {
                group.muted = !group.muted;
                return true;
            }
        }
        false
    }

    #[cfg(test)]
    fn mute(&mut self, group_id: u64) {
        if self.removed.contains(&group_id) {
            return;
        }
        for group in self.groups.values_mut() {
            if group.id == group_id {
                group.muted = true;
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn view_detail(&mut self, group_id: u64) -> Option<GroupAggregate> {
        if self.removed.contains(&group_id) {
            return None;
        }
        if !self.groups.values().any(|group| group.id == group_id) {
            return None;
        }
        self.samples.touch(group_id);
        self.groups
            .values()
            .find(|group| group.id == group_id)
            .map(|group| self.aggregate(group))
    }

    #[cfg(test)]
    fn sample_bytes_used(&self) -> usize {
        self.samples.used()
    }

    #[cfg(test)]
    fn sample_budget(&self) -> usize {
        self.samples.budget()
    }

    fn emit_source_ended(
        &self,
        status: Option<i32>,
        stderr: String,
        stderr_truncated: bool,
        out: &mut impl Write,
    ) -> Result<(), Error> {
        emit(
            out,
            &Record::SourceEnded {
                version: 1,
                session_id: &self.session_id,
                emitted_at: now_utc(),
                status,
                stderr,
                stderr_truncated,
            },
        )
    }
}

fn emit(out: &mut impl Write, record: &Record<'_>) -> Result<(), Error> {
    let mut line = serde_json::to_vec(record).map_err(|err| Error::Io(err.to_string()))?;
    line.push(b'\n');
    out.write_all(&line)
        .and_then(|_| out.flush())
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::BrokenPipe {
                Error::Io("broken stdout".into())
            } else {
                Error::Io(err.to_string())
            }
        })
}

fn now_utc() -> String {
    time::rfc3339_millis(Utc::now())
}

fn is_broken_pipe(err: &Error) -> bool {
    matches!(
        err,
        Error::Io(message)
            if message.contains("Broken pipe")
                || message.contains("broken stdout")
                || message.contains("os error 32")
    )
}

fn round_rate(value: f64) -> f64 {
    if !value.is_finite() {
        0.0
    } else {
        (value * 1000.0).round() / 1000.0
    }
}

fn rounded_rates(group: &Group, params: &RateParams) -> (f64, f64) {
    let (fast, baseline) = group.rate.rates_at(group.latest_effective, params);
    (round_rate(fast), round_rate(baseline))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::cli::Service;
    use chrono::TimeZone;

    fn arrival() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 26, 20, 0, 0).unwrap()
    }

    const ERROR_A: &str = "26.08.2026 12:00:00.123 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo Failed to start bundle";
    const ERROR_A_REPEAT: &str = "26.08.2026 12:00:01.000 author-1 *ERROR* [FelixDispatchQueue] com.example.Foo Failed to start bundle";
    const ERROR_B: &str =
        "26.08.2026 12:00:02.001 author-0 *ERROR* [FelixDispatchQueue] com.example.Baz other error";
    const WARN: &str =
        "26.08.2026 12:00:00.789 author-0 *WARN* [FelixDispatchQueue] com.example.Bar ignored warn";
    const INFO: &str =
        "26.08.2026 12:00:00.790 author-0 *INFO* [FelixDispatchQueue] com.example.Bar chatter";

    fn request() -> Request {
        Request {
            program_id: "p1".into(),
            environment_id: "e1".into(),
            service: Service::Author,
            levels: vec![Level::Error],
            ims_context: None,
            config: None,
            timezone: Timezone::Utc,
            json: true,
            raw_sample: false,
            tuning: Tuning::default(),
        }
    }

    fn records(levels: Vec<Level>, lines: &[&str]) -> Vec<serde_json::Value> {
        records_with(levels, lines, Redactor::default(), false)
    }

    fn records_with(
        levels: Vec<Level>,
        lines: &[&str],
        redactor: Redactor,
        raw_sample: bool,
    ) -> Vec<serde_json::Value> {
        let tuning = Tuning {
            sample_max_bytes: u64::MAX,
            ..Tuning::default()
        };
        let mut analyzer = Analyzer::with_tuning(
            levels,
            TimeInterpreter::new(Timezone::Utc),
            redactor,
            raw_sample,
            &tuning,
        );
        let mut buf = Vec::new();
        analyzer.emit_session_started(&request(), &mut buf).unwrap();
        for line in lines {
            analyzer
                .ingest(line, arrival(), Instant::now(), &mut buf)
                .unwrap();
        }
        analyzer
            .flush_pending_updates(Instant::now(), &mut buf)
            .unwrap();
        analyzer
            .emit_source_ended(Some(0), String::new(), false, &mut buf)
            .unwrap();
        parse_records(&buf)
    }

    fn parse_records(buf: &[u8]) -> Vec<serde_json::Value> {
        String::from_utf8(buf.to_vec())
            .unwrap()
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).expect(line))
            .collect()
    }

    #[test]
    fn framed_multiline_stack_is_one_exact_group() {
        let event = concat!(
            "26.08.2026 12:00:00.123 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo boom\n",
            "java.lang.RuntimeException: boom\n",
            "\tat com.example.Foo.bar(Foo.java:42)\n",
        );
        let repeat = concat!(
            "26.08.2026 12:00:01.000 author-1 *ERROR* [FelixDispatchQueue] com.example.Foo boom\n",
            "java.lang.RuntimeException: boom\n",
            "\tat com.example.Foo.bar(Foo.java:42)\n",
        );
        let recs = records(vec![Level::Error], &[event, repeat]);
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
        assert_eq!(recs[1]["count"], 1);
        assert_eq!(recs[1]["sample"], event);
        assert_eq!(recs[1]["sample_truncated"], false);
        assert_eq!(recs[2]["count"], 2);
        assert!(recs[2].get("sample").is_none());
        assert!(recs[1]["fast_rate"].as_f64().unwrap().is_finite());
        assert!(recs[2]["baseline_rate"].as_f64().unwrap().is_finite());
    }

    #[test]
    fn session_started_is_first_and_identifies_source() {
        let recs = records(vec![Level::Error], &[]);
        assert_eq!(recs[0]["type"], "session_started");
        assert_eq!(recs[0]["version"], 1);
        assert_eq!(recs[0]["source"]["program_id"], "p1");
        assert_eq!(recs[0]["source"]["environment_id"], "e1");
        assert_eq!(recs[0]["source"]["service"], "author");
        assert_eq!(recs[0]["source"]["log"], "aemerror");
        assert_eq!(recs[0]["levels"], serde_json::json!(["ERROR"]));
        assert!(recs[0]["emitted_at"].as_str().unwrap().ends_with('Z'));
        assert_eq!(recs.last().unwrap()["type"], "source_ended");
    }

    #[test]
    fn selected_single_line_creates_exact_group_and_repetition_increments() {
        let recs = records(vec![Level::Error], &[ERROR_A, ERROR_A_REPEAT, ERROR_B]);
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
        assert_eq!(recs[1]["group_id"], 1);
        assert_eq!(recs[1]["count"], 1);
        assert_eq!(recs[1]["sample"], ERROR_A);
        assert_eq!(recs[2]["group_id"], 2);
        assert_eq!(recs[2]["count"], 1);
        assert_eq!(recs[3]["group_id"], 1);
        assert_eq!(recs[3]["count"], 2);
        assert!(recs[3].get("sample").is_none());
        assert_eq!(recs[1]["session_id"], recs[3]["session_id"]);
    }

    #[test]
    fn non_selected_levels_never_create_groups() {
        let recs = records(vec![Level::Error], &[WARN, INFO, "garbage", ERROR_A]);
        let types: Vec<&str> = recs.iter().map(|r| r["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["session_started", "group_created", "source_ended"]);
        assert_eq!(recs[1]["sample"], ERROR_A);
    }

    #[test]
    fn uuid_v4_session_id() {
        let recs = records(vec![Level::Error], &[]);
        let id = recs[0]["session_id"].as_str().unwrap();
        assert!(is_uuid_v4(id), "{id}");
    }

    #[test]
    fn parser_diagnostics_do_not_create_or_rank_groups() {
        use crate::app::frame::{Diagnostic, DiagnosticReason};

        let mut analyzer = Analyzer::new(vec![Level::Error]);
        let mut buf = Vec::new();
        analyzer.emit_session_started(&request(), &mut buf).unwrap();
        accept_frames(
            &mut analyzer,
            vec![
                Frame::Diagnostic(Diagnostic {
                    reason: DiagnosticReason::UnframedPrefix,
                    count: 7,
                    sample: "garbage".into(),
                    line: 1,
                    offset: 0,
                }),
                Frame::Diagnostic(Diagnostic {
                    reason: DiagnosticReason::EventByteLimit,
                    count: 40,
                    sample: "xxxx".into(),
                    line: 2,
                    offset: 10,
                }),
                Frame::Event(ERROR_A.into()),
                Frame::Diagnostic(Diagnostic {
                    reason: DiagnosticReason::InvalidUtf8,
                    count: 1,
                    sample: "\u{FFFD}".into(),
                    line: 3,
                    offset: 20,
                }),
            ],
            &mut buf,
        )
        .unwrap();
        analyzer
            .emit_source_ended(Some(0), String::new(), false, &mut buf)
            .unwrap();
        let recs = parse_records(&buf);
        let types: Vec<&str> = recs.iter().map(|r| r["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            [
                "session_started",
                "parser_error",
                "parser_error",
                "group_created",
                "parser_error",
                "source_ended"
            ]
        );
        assert_eq!(recs[1]["reason"], "unframed_prefix");
        assert_eq!(recs[1]["count"], 7);
        assert_eq!(recs[1]["sample"], "garbage");
        assert_eq!(recs[2]["reason"], "event_byte_limit");
        assert_eq!(recs[3]["sample"], ERROR_A);
        assert_eq!(recs[3]["count"], 1);
        assert_eq!(recs[4]["reason"], "invalid_utf8");
        assert_eq!(analyzer.groups.len(), 1);
        assert_eq!(analyzer.diagnostics, 48);
        assert_eq!(analyzer.selected_events, 1);
    }

    #[test]
    fn snapshot_sorts_volume_by_count_then_id_and_tracks_overflow() {
        let mut analyzer = Analyzer::with_tuning(
            vec![Level::Error],
            TimeInterpreter::new(Timezone::Utc),
            Redactor::default(),
            false,
            &Tuning {
                sample_max_bytes: u64::MAX,
                bucket_cap: 1,
                ..Tuning::default()
            },
        );
        let mut buf = Vec::new();
        analyzer
            .ingest(
                "26.08.2026 12:00:00.000 author-0 *ERROR* [t] com.example.Foo alpha beta gamma delta epsilon",
                arrival(),
                Instant::now(),
                &mut buf,
            )
            .unwrap();
        analyzer
            .ingest(
                "26.08.2026 12:00:01.000 author-1 *ERROR* [t] com.example.Foo zzzzz yyyyy xxxxx wwwww vvvvv",
                arrival(),
                Instant::now(),
                &mut buf,
            )
            .unwrap();
        analyzer
            .ingest(
                "26.08.2026 12:00:02.000 author-0 *ERROR* [t] com.example.Foo alpha beta gamma delta epsilon",
                arrival(),
                Instant::now(),
                &mut buf,
            )
            .unwrap();
        let snap = analyzer.snapshot(&request(), ProcessState::AioRunning, Instant::now());
        assert_eq!(snap.selected_events, 3);
        assert_eq!(snap.overflow, OverflowState::Events(1));
        assert_eq!(snap.group_count(), 1);
        assert_eq!(snap.process.label(), "AIO running / awaiting logs");
        let ids: Vec<u64> = snap
            .view_rows(View::Volume, View::Volume.default_sort())
            .into_iter()
            .map(|g| g.id)
            .collect();
        assert_eq!(ids, [1]);
        assert_eq!(snap.groups[0].count, 2);
        assert_eq!(snap.groups[0].level, Level::Error);
        assert_eq!(snap.groups[0].logger, "com.example.Foo");
    }

    #[test]
    fn metadata_is_emitted_and_raw_sample_is_bounded() {
        let event = concat!(
            "26.08.2026 12:00:00.123 author-0 *ERROR* [worker-1] com.example.Foo ",
            "message with a deliberately long continuation\n",
            "stack continuation\n",
        );
        let tuning = Tuning {
            sample_max_bytes: 24,
            ..Tuning::default()
        };
        let mut analyzer = Analyzer::with_tuning(
            vec![Level::Error],
            TimeInterpreter::new(Timezone::Utc),
            Redactor::default(),
            false,
            &tuning,
        );
        let mut buf = Vec::new();
        analyzer
            .ingest(event, arrival(), Instant::now(), &mut buf)
            .unwrap();
        let record = parse_records(&buf).pop().expect("group record");
        assert_eq!(record["timestamp"], "2026-08-26T12:00:00.123Z");
        assert!(record.get("time_fallback").is_none());
        assert_eq!(record["node"], "author-0");
        assert_eq!(record["level"], "ERROR");
        assert_eq!(record["thread"], "worker-1");
        assert_eq!(record["logger"], "com.example.Foo");
        assert_eq!(
            record["message"],
            "message with a deliberately long continuation"
        );
        assert!(record["request_context"].is_null());
        assert!(record["terminal_exception"].is_null());
        assert!(record["terminal_frame"].is_null());
        assert!(record["sample"].as_str().unwrap().len() <= 24);
        assert_eq!(record["sample_truncated"], true);
        assert!(record["source_offsets"]["logger"]["end"].as_u64().unwrap() > 0);
    }

    #[test]
    fn grouping_uses_pre_redaction_identity() {
        let a = "26.08.2026 12:00:00.123 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo user ops@example.com failed";
        let b = "26.08.2026 12:00:01.000 author-1 *ERROR* [FelixDispatchQueue] com.example.Foo user admin@example.net failed";
        let recs = records(vec![Level::Error], &[a, b]);
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
        assert_eq!(recs[1]["group_id"], 1);
        assert_eq!(recs[2]["group_id"], 1);
        assert_eq!(recs[2]["count"], 2);
        assert_eq!(recs[1]["first_seen"], "2026-08-26T12:00:00.123Z");
        assert_eq!(recs[2]["last_seen"], "2026-08-26T12:00:01.000Z");
        let sample = recs[1]["sample"].as_str().unwrap();
        assert!(!sample.contains("ops@example.com"), "{sample}");
        assert!(sample.contains("[REDACTED:email]"), "{sample}");
        assert_eq!(recs[1]["message"], "user [REDACTED:email] failed");
    }

    #[test]
    fn learned_templates_group_paths_and_keep_frames_separate() {
        let us = "26.08.2026 12:00:00.123 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo Resource not found /content/site/us/en.html";
        let de = "26.08.2026 12:00:01.000 author-1 *ERROR* [FelixDispatchQueue] com.example.Foo Resource not found /content/site/de/de.html";
        let other_frame = concat!(
            "26.08.2026 12:00:02.000 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo Resource not found /content/site/fr/fr.html\n",
            "java.lang.RuntimeException: missing\n",
            "\tat com.example.Foo.bar(Foo.java:42)\n",
        );
        let recs = records(vec![Level::Error], &[us, de, other_frame]);
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
        assert_eq!(recs[1]["group_id"], 1);
        assert_eq!(recs[2]["group_id"], 2);
        assert_eq!(recs[2]["terminal_exception"], "java.lang.RuntimeException");
        assert_eq!(recs[2]["terminal_frame"], "com.example.Foo.bar");
        assert_eq!(recs[3]["group_id"], 1);
        assert_eq!(recs[3]["count"], 2);
    }

    #[test]
    fn raw_sample_keeps_sample_raw_and_always_redacts_request_context() {
        let event = concat!(
            "26.08.2026 12:00:01.456 author-0 *ERROR* ",
            "[192.0.2.10 [1724666401456] GET /content/site/us/en.html?foo=bar HTTP/1.1] ",
            "com.example.core.filters.ErrorFilter contact ops@example.com\n",
            "java.lang.IllegalStateException: resource resolver closed\n",
            "\tat com.example.core.filters.ErrorFilter.doFilter(ErrorFilter.java:64)\n",
        );
        let recs = records_with(vec![Level::Error], &[event], Redactor::default(), true);
        let sample = recs[1]["sample"].as_str().unwrap();
        assert_eq!(sample, event);
        assert!(sample.contains("192.0.2.10"), "{sample}");
        assert!(sample.contains("ops@example.com"), "{sample}");
        assert_eq!(recs[1]["request_context"]["client_ip"], "[REDACTED:ip]");
        assert_eq!(
            recs[1]["request_context"]["path"],
            "/content/site/us/en.html?foo=[REDACTED:query]"
        );
        assert_eq!(recs[1]["request_context"]["request_id"], "1724666401456");
        assert!(!recs[1]["thread"].as_str().unwrap().contains("192.0.2.10"));
        assert_eq!(recs[1]["message"], "contact [REDACTED:email]");
        assert!(recs[1]["logger"].as_str().unwrap().contains("ErrorFilter"));
        assert_eq!(
            recs[1]["terminal_exception"],
            "java.lang.IllegalStateException"
        );
        assert_eq!(
            recs[1]["terminal_frame"],
            "com.example.core.filters.ErrorFilter.doFilter"
        );
        assert_eq!(recs[1]["timestamp"], "2026-08-26T12:00:01.456Z");
    }

    #[test]
    fn extra_patterns_redact_samples_after_builtins() {
        let event = "26.08.2026 12:00:00.123 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo secret-99 leftover";
        let recs = records_with(
            vec![Level::Error],
            &[event],
            Redactor::new(vec![regex::Regex::new("secret-[0-9]+").unwrap()]),
            false,
        );
        let sample = recs[1]["sample"].as_str().unwrap();
        assert!(!sample.contains("secret-99"), "{sample}");
        assert!(sample.contains("[REDACTED]"), "{sample}");
        assert!(sample.contains("leftover"), "{sample}");
        assert!(sample.contains("com.example.Foo"), "{sample}");
    }

    #[test]
    fn sample_bound_applies_after_redaction() {
        let prefix = "26.08.2026 12:00:00.123 author-0 *ERROR* [t] com.example.Foo ";
        let secret = "ops@example.com";
        let event = format!("{prefix}{secret} trailing");
        let max = prefix.len() + 5;
        let tuning = Tuning {
            sample_max_bytes: max as u64,
            ..Tuning::default()
        };
        let mut analyzer = Analyzer::with_tuning(
            vec![Level::Error],
            TimeInterpreter::new(Timezone::Utc),
            Redactor::default(),
            false,
            &tuning,
        );
        let mut buf = Vec::new();
        analyzer
            .ingest(&event, arrival(), Instant::now(), &mut buf)
            .unwrap();
        let record = parse_records(&buf).pop().expect("group record");
        let sample = record["sample"].as_str().unwrap();
        assert!(sample.len() <= max, "{sample}");
        assert!(!sample.contains("ops@"), "{sample}");
        assert!(!sample.contains(secret), "{sample}");
        assert!(!sample.contains("example.com"), "{sample}");
    }

    #[test]
    fn terminal_identity_is_emitted_on_group_created() {
        let event = concat!(
            "26.08.2026 12:00:00.123 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo boom\n",
            "java.lang.RuntimeException: boom\n",
            "\tat com.example.Foo.bar(Foo.java:42)\n",
        );
        let recs = records(vec![Level::Error], &[event]);
        assert_eq!(recs[1]["type"], "group_created");
        assert_eq!(recs[1]["terminal_exception"], "java.lang.RuntimeException");
        assert_eq!(recs[1]["terminal_frame"], "com.example.Foo.bar");
        assert_eq!(recs[1]["sample"], event);
    }

    #[test]
    fn iana_zone_emits_utc_rfc3339_timestamp() {
        let tuning = Tuning {
            sample_max_bytes: u64::MAX,
            ..Tuning::default()
        };
        let mut analyzer = Analyzer::with_tuning(
            vec![Level::Error],
            TimeInterpreter::new(Timezone::Iana("America/New_York".parse().expect("zone"))),
            Redactor::default(),
            false,
            &tuning,
        );
        let mut buf = Vec::new();
        analyzer
            .ingest(ERROR_A, arrival(), Instant::now(), &mut buf)
            .unwrap();
        let record = parse_records(&buf).pop().expect("group record");
        assert_eq!(record["timestamp"], "2026-08-26T16:00:00.123Z");
        assert!(record.get("time_fallback").is_none());
    }

    #[test]
    fn dst_faults_mark_arrival_fallback() {
        let tuning = Tuning {
            sample_max_bytes: u64::MAX,
            ..Tuning::default()
        };
        let mut analyzer = Analyzer::with_tuning(
            vec![Level::Error],
            TimeInterpreter::new(Timezone::Iana("America/New_York".parse().expect("zone"))),
            Redactor::default(),
            false,
            &tuning,
        );
        let gap =
            "08.03.2026 02:30:00.000 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo spring";
        let fold =
            "01.11.2026 01:30:00.000 author-0 *ERROR* [FelixDispatchQueue] com.example.Bar fall";
        let mut buf = Vec::new();
        analyzer
            .ingest(gap, arrival(), Instant::now(), &mut buf)
            .unwrap();
        analyzer
            .ingest(fold, arrival(), Instant::now(), &mut buf)
            .unwrap();
        let recs = parse_records(&buf);
        assert_eq!(recs[0]["timestamp"], "2026-08-26T20:00:00.000Z");
        assert_eq!(recs[0]["time_fallback"], true);
        assert_eq!(recs[1]["timestamp"], "2026-08-26T20:00:00.000Z");
        assert_eq!(recs[1]["time_fallback"], true);
    }

    #[test]
    fn out_of_order_nodes_cannot_move_group_clock_backward() {
        let mut analyzer = Analyzer::new(vec![Level::Error]);
        let later = "26.08.2026 12:00:02.000 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo Failed to start bundle";
        let earlier = "26.08.2026 12:00:01.000 author-1 *ERROR* [FelixDispatchQueue] com.example.Foo Failed to start bundle";
        let mut buf = Vec::new();
        analyzer
            .ingest(later, arrival(), Instant::now(), &mut buf)
            .unwrap();
        analyzer
            .ingest(earlier, arrival(), Instant::now(), &mut buf)
            .unwrap();
        let first = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 2).unwrap();
        assert_eq!(
            analyzer.groups.values().next().unwrap().latest_effective,
            first
        );
    }

    fn event(ts: &str, node: &str, message: &str) -> String {
        format!("{ts} {node} *ERROR* [FelixDispatchQueue] com.example.Foo {message}")
    }

    fn stacked(ts: &str, node: &str, message: &str, exception: &str, frame: &str) -> String {
        format!(
            "{ts} {node} *ERROR* [FelixDispatchQueue] com.example.Foo {message}\n{exception}: missing\n\tat {frame}(Foo.java:42)\n"
        )
    }

    #[test]
    fn path_and_package_variants_merge_while_frames_stay_split() {
        let us = event(
            "26.08.2026 12:00:00.123",
            "author-0",
            "Resource not found /content/site/us/en.html",
        );
        let de = event(
            "26.08.2026 12:00:01.000",
            "author-1",
            "Resource not found /content/site/de/de.html",
        );
        let v1 = event(
            "26.08.2026 12:00:02.000",
            "author-0",
            "Failed to start bundle com.example.core 1.2.3",
        );
        let v2 = event(
            "26.08.2026 12:00:03.000",
            "author-1",
            "Failed to start bundle com.example.core 1.2.4",
        );
        let framed = stacked(
            "26.08.2026 12:00:04.000",
            "author-0",
            "Resource not found /content/site/fr/fr.html",
            "java.lang.RuntimeException",
            "com.example.Foo.bar",
        );
        let recs = records(vec![Level::Error], &[&us, &de, &v1, &v2, &framed]);
        let types: Vec<&str> = recs.iter().map(|r| r["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            [
                "session_started",
                "group_created",
                "group_created",
                "group_created",
                "group_updated",
                "group_updated",
                "source_ended"
            ]
        );
        assert_eq!(recs[1]["group_id"], 1);
        assert_eq!(recs[2]["group_id"], 2);
        assert_eq!(recs[3]["group_id"], 3);
        assert_eq!(recs[3]["terminal_frame"], "com.example.Foo.bar");
        let updates: Vec<_> = recs
            .iter()
            .filter(|r| r["type"] == "group_updated")
            .collect();
        assert_eq!(updates[0]["group_id"], 1);
        assert_eq!(updates[0]["count"], 2);
        assert_eq!(updates[1]["group_id"], 2);
        assert_eq!(updates[1]["count"], 2);
    }

    fn merge_bridge_lines() -> Vec<String> {
        let first = "alpha beta gamma delta epsilon";
        let second = "alpha other unique novel epsilon";
        let mut lines = vec![
            event("26.08.2026 12:00:00.000", "author-0", first),
            event("26.08.2026 12:00:01.000", "author-1", second),
        ];
        for (src, position, replacement, ts, node) in [
            (first, 1, "BETA", "26.08.2026 12:00:02.000", "author-0"),
            (first, 2, "GAMMA", "26.08.2026 12:00:03.000", "author-0"),
            (second, 1, "OTHER", "26.08.2026 12:00:04.000", "author-1"),
            (second, 2, "UNIQUE", "26.08.2026 12:00:05.000", "author-2"),
        ] {
            let mut tokens: Vec<&str> = src.split_whitespace().collect();
            tokens[position] = replacement;
            lines.push(event(ts, node, &tokens.join(" ")));
        }
        lines
    }

    #[test]
    fn compatible_groups_merge_oldest_id_and_preserve_aggregates() {
        let lines = merge_bridge_lines();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut analyzer = Analyzer::new(vec![Level::Error]);
        let mut buf = Vec::new();
        analyzer.emit_session_started(&request(), &mut buf).unwrap();
        analyzer
            .ingest(refs[0], arrival(), Instant::now(), &mut buf)
            .unwrap();
        analyzer.mute(1);
        for line in &refs[1..] {
            analyzer
                .ingest(line, arrival(), Instant::now(), &mut buf)
                .unwrap();
        }
        analyzer
            .flush_pending_updates(Instant::now(), &mut buf)
            .unwrap();
        analyzer
            .emit_source_ended(Some(0), String::new(), false, &mut buf)
            .unwrap();
        let recs = parse_records(&buf);
        let merged = recs
            .iter()
            .find(|r| r["type"] == "group_merged")
            .expect("merge");
        assert_eq!(merged["group_id"], 1);
        assert_eq!(merged["removed_id"], 2);
        assert_eq!(merged["count"], 6);
        assert_eq!(merged["first_seen"], "2026-08-26T12:00:00.000Z");
        assert_eq!(merged["last_seen"], "2026-08-26T12:00:05.000Z");
        assert_eq!(merged["template"], "alpha <*> <*> <*> epsilon");
        assert_eq!(
            merged["nodes"],
            serde_json::json!(["author-0", "author-1", "author-2"])
        );
        assert_eq!(merged["muted"], true);
        assert_eq!(merged["sample"], refs[0]);
        let snap = analyzer.snapshot_groups();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, 1);
        assert_eq!(snap[0].count, 6);
        assert!(snap[0].muted);
        assert!(!analyzer.removed.contains(&1));
        assert!(analyzer.removed.contains(&2));
        let live = analyzer.snapshot(&request(), ProcessState::AioRunning, Instant::now());
        assert!(live
            .view_rows(View::Volume, View::Volume.default_sort())
            .is_empty());
        assert_eq!(
            live.view_rows(View::Muted, View::Muted.default_sort())
                .iter()
                .map(|g| g.id)
                .collect::<Vec<_>>(),
            [1]
        );
    }

    #[test]
    fn removed_group_ids_do_not_update_or_reappear() {
        let lines = merge_bridge_lines();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut analyzer = Analyzer::new(vec![Level::Error]);
        let mut buf = Vec::new();
        for line in &refs {
            analyzer
                .ingest(line, arrival(), Instant::now(), &mut buf)
                .unwrap();
        }
        let after_merge = event(
            "26.08.2026 12:00:05.000",
            "author-3",
            "alpha other unique novel epsilon",
        );
        analyzer
            .ingest(&after_merge, arrival(), Instant::now(), &mut buf)
            .unwrap();
        analyzer
            .flush_pending_updates(Instant::now(), &mut buf)
            .unwrap();
        let recs = parse_records(&buf);
        let merge_at = recs
            .iter()
            .position(|r| r["type"] == "group_merged")
            .expect("merge");
        assert!(recs[merge_at + 1..]
            .iter()
            .all(|r| r.get("group_id") != Some(&serde_json::json!(2))));
        let last = recs.last().unwrap();
        assert_eq!(last["type"], "group_updated");
        assert_eq!(last["group_id"], 1);
        assert_eq!(last["count"], 7);
        let ids: Vec<u64> = analyzer
            .snapshot_groups()
            .into_iter()
            .map(|g| g.id)
            .collect();
        assert_eq!(ids, [1]);
    }

    fn shuffle<T>(items: &mut [T], seed: u64) {
        let mut state = seed;
        for i in (1..items.len()).rev() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            items.swap(i, (state as usize) % (i + 1));
        }
    }

    fn fingerprint(groups: &[GroupAggregate]) -> Vec<(u64, String, Vec<String>, bool)> {
        let mut rows: Vec<(u64, String, Vec<String>, bool)> = groups
            .iter()
            .map(|group| {
                (
                    group.count,
                    group.template.join(" "),
                    group.nodes.clone(),
                    group.muted,
                )
            })
            .collect();
        rows.sort();
        rows
    }

    #[test]
    fn insertion_order_does_not_change_final_groups_or_totals() {
        let corpus = vec![
            event(
                "26.08.2026 12:00:00.000",
                "author-0",
                "Resource not found /content/site/us/en.html",
            ),
            event(
                "26.08.2026 12:00:01.000",
                "author-1",
                "Resource not found /content/site/de/de.html",
            ),
            event(
                "26.08.2026 12:00:02.000",
                "author-0",
                "Failed to start bundle com.example.core 1.2.3",
            ),
            event(
                "26.08.2026 12:00:03.000",
                "author-1",
                "Failed to start bundle com.example.core 1.2.4",
            ),
            stacked(
                "26.08.2026 12:00:04.000",
                "author-0",
                "Resource not found /content/site/fr/fr.html",
                "java.lang.RuntimeException",
                "com.example.Foo.bar",
            ),
            stacked(
                "26.08.2026 12:00:05.000",
                "author-1",
                "Resource not found /content/site/es/es.html",
                "java.lang.RuntimeException",
                "com.example.Foo.bar",
            ),
            stacked(
                "26.08.2026 12:00:06.000",
                "author-0",
                "Resource not found /content/site/it/it.html",
                "java.lang.IllegalStateException",
                "com.example.Foo.baz",
            ),
        ];
        let mut analyzer = Analyzer::new(vec![Level::Error]);
        let mut buf = Vec::new();
        for line in &corpus {
            analyzer
                .ingest(line, arrival(), Instant::now(), &mut buf)
                .unwrap();
        }
        let expected = fingerprint(&analyzer.snapshot_groups());
        let expected_total: u64 = analyzer.snapshot_groups().iter().map(|g| g.count).sum();
        for seed in 1..48 {
            let mut ordered = corpus.clone();
            shuffle(&mut ordered, seed);
            let mut other = Analyzer::new(vec![Level::Error]);
            let mut out = Vec::new();
            for line in &ordered {
                other
                    .ingest(line, arrival(), Instant::now(), &mut out)
                    .unwrap();
            }
            assert_eq!(
                fingerprint(&other.snapshot_groups()),
                expected,
                "seed {seed}"
            );
            let total: u64 = other.snapshot_groups().iter().map(|g| g.count).sum();
            assert_eq!(total, expected_total, "seed {seed}");
        }
    }

    #[test]
    fn rates_leave_counts_and_timestamps_exact() {
        let mut analyzer = Analyzer::new(vec![Level::Error]);
        let mut buf = Vec::new();
        analyzer
            .ingest(
                &event(
                    "26.08.2026 12:00:00.000",
                    "author-0",
                    "Failed to start bundle",
                ),
                arrival(),
                Instant::now(),
                &mut buf,
            )
            .unwrap();
        analyzer
            .ingest(
                &event(
                    "26.08.2026 12:00:10.000",
                    "author-1",
                    "Failed to start bundle",
                ),
                arrival(),
                Instant::now(),
                &mut buf,
            )
            .unwrap();
        let snap = analyzer.snapshot_groups();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].count, 2);
        assert_eq!(
            snap[0].first_seen,
            Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap()
        );
        assert_eq!(
            snap[0].last_seen,
            Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 10).unwrap()
        );
        let now = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 10).unwrap();
        let rates = analyzer.rate_snapshots(now);
        assert_eq!(rates[0].count, 2);
        assert_eq!(rates[0].first_seen, snap[0].first_seen);
        assert_eq!(rates[0].last_seen, snap[0].last_seen);
        assert!(rates[0].fast > 0.0);
    }

    #[test]
    fn fallback_arrival_time_feeds_rate_clock() {
        let tuning = Tuning {
            sample_max_bytes: u64::MAX,
            ..Tuning::default()
        };
        let mut analyzer = Analyzer::with_tuning(
            vec![Level::Error],
            TimeInterpreter::new(Timezone::Iana("America/New_York".parse().expect("zone"))),
            Redactor::default(),
            false,
            &tuning,
        );
        let gap =
            "08.03.2026 02:30:00.000 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo spring";
        let mut buf = Vec::new();
        analyzer
            .ingest(gap, arrival(), Instant::now(), &mut buf)
            .unwrap();
        let group = analyzer.groups.values().next().unwrap();
        assert_eq!(group.latest_effective, arrival());
        assert_eq!(group.rate.updated_at(), arrival());
    }

    #[test]
    fn out_of_order_nodes_do_not_regress_rate_clock() {
        let mut analyzer = Analyzer::new(vec![Level::Error]);
        let later = event(
            "26.08.2026 12:00:10.000",
            "author-0",
            "Failed to start bundle",
        );
        let earlier = event(
            "26.08.2026 12:00:01.000",
            "author-1",
            "Failed to start bundle",
        );
        let mut buf = Vec::new();
        analyzer
            .ingest(&later, arrival(), Instant::now(), &mut buf)
            .unwrap();
        analyzer
            .ingest(&earlier, arrival(), Instant::now(), &mut buf)
            .unwrap();
        let group = analyzer.groups.values().next().unwrap();
        let last = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 10).unwrap();
        assert_eq!(group.latest_effective, last);
        assert_eq!(group.rate.updated_at(), last);
        assert_eq!(group.count, 2);
        assert_eq!(
            group.first_seen,
            Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 1).unwrap()
        );
    }

    #[test]
    fn merge_combines_rate_state_without_changing_exact_counts() {
        let lines = merge_bridge_lines();
        let mut analyzer = Analyzer::new(vec![Level::Error]);
        let mut buf = Vec::new();
        for line in &lines {
            analyzer
                .ingest(line, arrival(), Instant::now(), &mut buf)
                .unwrap();
        }
        let snap = analyzer.snapshot_groups();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].count, 6);
        assert_eq!(
            snap[0].first_seen,
            Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap()
        );
        assert_eq!(
            snap[0].last_seen,
            Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 5).unwrap()
        );
        let now = snap[0].last_seen;
        let rates = analyzer.rate_snapshots(now);
        assert_eq!(rates[0].count, 6);
        assert!(rates[0].fast > 0.0);
        assert_eq!(
            analyzer.groups.values().next().unwrap().rate.updated_at(),
            now
        );
    }

    #[test]
    fn ranked_views_use_snapshot_clock() {
        let mut analyzer = Analyzer::new(vec![Level::Error]);
        let mut buf = Vec::new();
        analyzer
            .ingest(
                &event("26.08.2026 12:00:00.000", "author-0", "alpha error"),
                arrival(),
                Instant::now(),
                &mut buf,
            )
            .unwrap();
        analyzer
            .ingest(
                &event("26.08.2026 12:00:50.000", "author-0", "beta error"),
                arrival(),
                Instant::now(),
                &mut buf,
            )
            .unwrap();
        analyzer.mute(1);
        let now = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 50).unwrap();
        assert_eq!(
            analyzer
                .ranked(View::New, now)
                .iter()
                .map(|g| g.id)
                .collect::<Vec<_>>(),
            [2]
        );
        assert!(analyzer
            .ranked(
                View::Increasing,
                Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 50).unwrap()
            )
            .is_empty());
        assert_eq!(analyzer.ranked(View::Muted, now)[0].id, 1);
        assert_eq!(
            analyzer
                .ranked(View::Volume, now)
                .iter()
                .map(|g| g.id)
                .collect::<Vec<_>>(),
            [2]
        );
        let snap = analyzer.snapshot(&request(), ProcessState::AioRunning, Instant::now());
        assert_eq!(
            snap.view_rows(View::Volume, View::Volume.default_sort())
                .iter()
                .map(|g| g.id)
                .collect::<Vec<_>>(),
            [2]
        );
        assert_eq!(
            snap.view_rows(View::Muted, View::Muted.default_sort())[0].id,
            1
        );
    }

    #[test]
    fn muted_groups_keep_ingesting_and_toggle_restores() {
        let mut analyzer = Analyzer::new(vec![Level::Error]);
        let mut buf = Vec::new();
        analyzer
            .ingest(
                &event("26.08.2026 12:00:00.000", "author-0", "alpha error"),
                arrival(),
                Instant::now(),
                &mut buf,
            )
            .unwrap();
        analyzer.mute(1);
        analyzer
            .ingest(
                &event("26.08.2026 12:00:01.000", "author-0", "alpha error"),
                arrival(),
                Instant::now(),
                &mut buf,
            )
            .unwrap();
        let snap = analyzer.snapshot(&request(), ProcessState::AioRunning, Instant::now());
        assert_eq!(snap.groups[0].count, 2);
        assert!(snap.groups[0].muted);
        assert!(snap
            .view_rows(View::Volume, View::Volume.default_sort())
            .is_empty());
        assert_eq!(
            snap.view_rows(View::Muted, View::Muted.default_sort())[0].count,
            2
        );
        assert!(analyzer.toggle_mute(1));
        let restored = analyzer.snapshot(&request(), ProcessState::AioRunning, Instant::now());
        assert!(!restored.groups[0].muted);
        assert_eq!(
            restored.view_rows(View::Volume, View::Volume.default_sort())[0].id,
            1
        );
        assert!(restored
            .view_rows(View::Muted, View::Muted.default_sort())
            .is_empty());
    }

    fn analyzer_with(
        levels: Vec<Level>,
        similarity: f64,
        bucket_cap: u32,
        max_groups: u32,
    ) -> Analyzer {
        let tuning = Tuning {
            sample_max_bytes: u64::MAX,
            similarity,
            bucket_cap,
            max_groups,
            ..Tuning::default()
        };
        Analyzer::with_tuning(
            levels,
            TimeInterpreter::new(Timezone::Utc),
            Redactor::default(),
            false,
            &tuning,
        )
    }

    fn ingest_lines(analyzer: &mut Analyzer, lines: &[String]) {
        let mut buf = Vec::new();
        for line in lines {
            analyzer
                .ingest(line, arrival(), Instant::now(), &mut buf)
                .unwrap();
        }
    }

    fn unique_line(level: &str, i: usize) -> String {
        format!(
            "26.08.2026 12:{min:02}:{sec:02}.{ms:03} author-{node} *{level}* [FelixDispatchQueue] com.example.Logger{i} unique-token-{i}",
            min = (i / 60) % 60,
            sec = i % 60,
            ms = i % 1000,
            node = i % 3,
        )
    }

    fn same_bucket_line(i: usize) -> String {
        format!(
            "26.08.2026 12:00:00.{i:03} author-0 *ERROR* [FelixDispatchQueue] com.example.Foo unique-token-{i}"
        )
    }

    fn overflows(groups: &[GroupAggregate]) -> Vec<&GroupAggregate> {
        groups.iter().filter(|group| group.is_overflow).collect()
    }

    fn normals(groups: &[GroupAggregate]) -> Vec<&GroupAggregate> {
        groups.iter().filter(|group| !group.is_overflow).collect()
    }

    #[test]
    fn group_capacity_clamps_to_hard_ceiling() {
        let analyzer = analyzer_with(vec![Level::Error], 1.0, 1, MAX_GROUPS.saturating_add(1));
        assert_eq!(analyzer.max_groups, MAX_GROUPS as usize);
    }

    #[test]
    fn capacity_zero_rejects_all_new_groups() {
        let mut analyzer = analyzer_with(vec![Level::Error], 1.0, 8, 0);
        ingest_lines(
            &mut analyzer,
            &[unique_line("ERROR", 0), unique_line("ERROR", 1)],
        );
        assert_eq!(analyzer.normal_group_count(), 0);
        let snap = analyzer.snapshot_groups();
        let overflow = overflows(&snap);
        assert_eq!(overflow.len(), 1);
        assert_eq!(overflow[0].level, Level::Error);
        assert_eq!(overflow[0].count, 2);
        assert_eq!(overflow[0].capacity_global, 2);
        assert_eq!(overflow[0].capacity_template_bucket, 0);
        assert_eq!(overflow[0].id, 1);
    }

    #[test]
    fn exact_capacity_boundary_creates_no_overflow() {
        let mut analyzer = analyzer_with(vec![Level::Error], 1.0, 8, 2);
        ingest_lines(
            &mut analyzer,
            &[unique_line("ERROR", 0), unique_line("ERROR", 1)],
        );
        let snap = analyzer.snapshot_groups();
        assert_eq!(analyzer.normal_group_count(), 2);
        assert!(overflows(&snap).is_empty());
        assert_eq!(
            normals(&snap).iter().map(|g| g.id).collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn first_overflow_uses_one_visible_level_group() {
        let mut analyzer = analyzer_with(vec![Level::Error], 1.0, 8, 2);
        ingest_lines(
            &mut analyzer,
            &[
                unique_line("ERROR", 0),
                unique_line("ERROR", 1),
                unique_line("ERROR", 2),
            ],
        );
        let snap = analyzer.snapshot_groups();
        assert_eq!(analyzer.normal_group_count(), 2);
        let overflow = overflows(&snap);
        assert_eq!(overflow.len(), 1);
        assert_eq!(overflow[0].id, 3);
        assert_eq!(overflow[0].count, 1);
        assert_eq!(overflow[0].capacity_global, 1);
        assert_eq!(overflow[0].capacity_template_bucket, 0);
        assert_eq!(
            snap.iter()
                .map(|g| (g.id, g.is_overflow))
                .collect::<Vec<_>>(),
            [(1, false), (2, false), (3, true)]
        );
    }

    #[test]
    fn sustained_unique_attack_stays_in_one_overflow() {
        let mut analyzer = analyzer_with(vec![Level::Error], 1.0, 8, 3);
        let lines: Vec<String> = (0..20).map(|i| unique_line("ERROR", i)).collect();
        ingest_lines(&mut analyzer, &lines);
        let snap = analyzer.snapshot_groups();
        assert_eq!(analyzer.normal_group_count(), 3);
        let overflow = overflows(&snap);
        assert_eq!(overflow.len(), 1);
        assert_eq!(overflow[0].count, 17);
        assert_eq!(overflow[0].capacity_global, 17);
        assert_eq!(overflow[0].capacity_template_bucket, 0);
        assert_eq!(overflow[0].nodes.len(), 3);
        assert_eq!(
            time::rfc3339_millis(overflow[0].first_seen),
            "2026-08-26T12:00:03.003Z"
        );
        assert_eq!(
            time::rfc3339_millis(overflow[0].last_seen),
            "2026-08-26T12:00:19.019Z"
        );
    }

    #[test]
    fn known_groups_keep_counting_after_saturation() {
        let mut analyzer = analyzer_with(vec![Level::Error], 1.0, 8, 2);
        ingest_lines(
            &mut analyzer,
            &[
                unique_line("ERROR", 0),
                unique_line("ERROR", 1),
                unique_line("ERROR", 2),
                unique_line("ERROR", 0),
            ],
        );
        let snap = analyzer.snapshot_groups();
        let normal = normals(&snap);
        assert_eq!(normal[0].id, 1);
        assert_eq!(normal[0].count, 2);
        assert_eq!(normal[1].count, 1);
        let overflow = overflows(&snap);
        assert_eq!(overflow.len(), 1);
        assert_eq!(overflow[0].count, 1);
        assert_eq!(overflow[0].capacity_global, 1);
    }

    #[test]
    fn error_and_warn_overflows_stay_distinct() {
        let mut analyzer = analyzer_with(vec![Level::Error, Level::Warn], 1.0, 8, 2);
        ingest_lines(
            &mut analyzer,
            &[
                unique_line("ERROR", 0),
                unique_line("WARN", 1),
                unique_line("ERROR", 2),
                unique_line("WARN", 3),
                unique_line("ERROR", 4),
            ],
        );
        let snap = analyzer.snapshot_groups();
        assert_eq!(analyzer.normal_group_count(), 2);
        let overflow = overflows(&snap);
        assert_eq!(overflow.len(), 2);
        assert_eq!(overflow[0].level, Level::Error);
        assert_eq!(overflow[0].count, 2);
        assert_eq!(overflow[0].capacity_global, 2);
        assert_eq!(overflow[1].level, Level::Warn);
        assert_eq!(overflow[1].count, 1);
        assert_eq!(overflow[1].capacity_global, 1);
        assert!(snap.iter().all(|group| group.level != Level::Fatal));
    }

    #[test]
    fn template_bucket_exhaustion_uses_template_reason() {
        let mut analyzer = analyzer_with(vec![Level::Error], 1.0, 2, 100);
        let lines: Vec<String> = (0..5).map(same_bucket_line).collect();
        ingest_lines(&mut analyzer, &lines);
        let snap = analyzer.snapshot_groups();
        assert_eq!(analyzer.normal_group_count(), 2);
        let overflow = overflows(&snap);
        assert_eq!(overflow.len(), 1);
        assert_eq!(overflow[0].count, 3);
        assert_eq!(overflow[0].capacity_template_bucket, 3);
        assert_eq!(overflow[0].capacity_global, 0);
    }

    fn analyzer_samples(sample_max_bytes: u64, sample_budget_bytes: u64) -> Analyzer {
        let tuning = Tuning {
            sample_max_bytes,
            sample_budget_bytes,
            ..Tuning::default()
        };
        Analyzer::with_tuning(
            vec![Level::Error],
            TimeInterpreter::new(Timezone::Utc),
            Redactor::default(),
            false,
            &tuning,
        )
    }

    #[test]
    fn hot_group_copies_first_sample_once() {
        let first = stacked(
            "26.08.2026 12:00:00.000",
            "author-0",
            "hot boom",
            "java.lang.RuntimeException",
            "com.example.Foo.bar",
        );
        let again = stacked(
            "26.08.2026 12:00:01.000",
            "author-1",
            "hot boom",
            "java.lang.RuntimeException",
            "com.example.Foo.bar",
        );
        let mut analyzer = analyzer_samples(u64::MAX, u64::MAX);
        let mut buf = Vec::new();
        analyzer
            .ingest(&first, arrival(), Instant::now(), &mut buf)
            .unwrap();
        let after_first = analyzer.sample_bytes_used();
        analyzer
            .ingest(&again, arrival(), Instant::now(), &mut buf)
            .unwrap();
        analyzer
            .ingest(&again, arrival(), Instant::now(), &mut buf)
            .unwrap();
        let snap = analyzer.snapshot_groups();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].count, 3);
        assert_eq!(snap[0].sample, first);
        assert!(snap[0].sample_available);
        assert!(!snap[0].sample_truncated);
        assert_eq!(analyzer.sample_bytes_used(), after_first);
    }

    #[test]
    fn many_sampled_groups_stay_within_budget() {
        let mut analyzer = analyzer_samples(32, 512);
        let lines: Vec<String> = (0..8).map(|i| unique_line("ERROR", i)).collect();
        ingest_lines(&mut analyzer, &lines);
        let snap = analyzer.snapshot_groups();
        assert_eq!(snap.len(), 8);
        assert!(snap.iter().all(|group| group.sample_available));
        assert!(analyzer.sample_bytes_used() <= analyzer.sample_budget());
        assert!(analyzer.sample_bytes_used() > 0);
    }

    #[test]
    fn truncated_sample_keeps_head_and_tail() {
        let mut event = String::from(
            "26.08.2026 12:00:00.123 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo first message\n",
        );
        event.push_str(&"stack-body\n".repeat(40));
        event.push_str("java.lang.RuntimeException: terminal\n");
        event.push_str("\tat com.example.Foo.bar(Foo.java:42)\n");
        let mut analyzer = analyzer_samples(200, 200);
        let mut buf = Vec::new();
        analyzer
            .ingest(&event, arrival(), Instant::now(), &mut buf)
            .unwrap();
        let snap = analyzer.snapshot_groups();
        assert_eq!(snap.len(), 1);
        assert!(snap[0].sample_truncated);
        assert_eq!(snap[0].sample_original_bytes, event.len());
        assert_eq!(snap[0].sample_original_lines, 43);
        assert!(
            snap[0].sample.contains("first message"),
            "{}",
            snap[0].sample
        );
        assert!(
            snap[0].sample.contains("Foo.bar(Foo.java:42)"),
            "{}",
            snap[0].sample
        );
        assert!(snap[0].sample.len() <= 200);
        assert_eq!(snap[0].count, 1);
    }

    #[test]
    fn lru_eviction_drops_sample_not_group_state() {
        let mut analyzer = analyzer_samples(32, 80);
        ingest_lines(
            &mut analyzer,
            &[
                unique_line("ERROR", 0),
                unique_line("ERROR", 1),
                unique_line("ERROR", 2),
            ],
        );
        let snap = analyzer.snapshot_groups();
        assert_eq!(snap.len(), 3);
        assert!(snap.iter().any(|group| !group.sample_available));
        assert!(snap.iter().all(|group| group.count == 1));
        assert!(analyzer.sample_bytes_used() <= analyzer.sample_budget());
        let evicted = snap
            .iter()
            .find(|group| !group.sample_available)
            .expect("evicted");
        assert!(evicted.sample.is_empty());
        assert_eq!(evicted.id, 1);
        assert!(!evicted.muted);
        assert_eq!(evicted.nodes, ["author-0"]);
    }

    #[test]
    fn viewing_detail_refreshes_sample_recency() {
        let mut analyzer = analyzer_samples(32, 80);
        ingest_lines(
            &mut analyzer,
            &[unique_line("ERROR", 0), unique_line("ERROR", 1)],
        );
        let before = analyzer.snapshot_groups();
        assert!(before.iter().all(|group| group.sample_available));
        let viewed = analyzer.view_detail(1).expect("group 1");
        assert_eq!(viewed.count, before[0].count);
        assert_eq!(viewed.id, 1);
        ingest_lines(&mut analyzer, &[unique_line("ERROR", 2)]);
        let snap = analyzer.snapshot_groups();
        let one = snap.iter().find(|group| group.id == 1).unwrap();
        let two = snap.iter().find(|group| group.id == 2).unwrap();
        assert!(one.sample_available);
        assert!(!two.sample_available);
        assert_eq!(one.count, 1);
        assert_eq!(two.count, 1);
    }

    #[test]
    fn merge_after_eviction_keeps_oldest_available_sample() {
        let lines = merge_bridge_lines();
        let max_len = lines.iter().map(String::len).max().unwrap() as u64;
        let hold_one = lines[0].len().max(lines[1].len()) as u64;
        let mut analyzer = analyzer_samples(max_len, hold_one);
        let mut buf = Vec::new();
        analyzer
            .ingest(&lines[0], arrival(), Instant::now(), &mut buf)
            .unwrap();
        analyzer
            .ingest(&lines[1], arrival(), Instant::now(), &mut buf)
            .unwrap();
        let mid = analyzer.snapshot_groups();
        assert_eq!(mid.len(), 2);
        let older = mid.iter().find(|group| group.id == 1).unwrap();
        let newer = mid.iter().find(|group| group.id == 2).unwrap();
        assert!(!older.sample_available);
        assert!(newer.sample_available);
        for line in &lines[2..] {
            analyzer
                .ingest(line, arrival(), Instant::now(), &mut buf)
                .unwrap();
        }
        let snap = analyzer.snapshot_groups();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, 1);
        assert_eq!(snap[0].count, 6);
        assert!(snap[0].sample_available);
        assert_eq!(snap[0].sample, lines[1]);
        assert!(analyzer.sample_bytes_used() <= analyzer.sample_budget());
    }

    #[test]
    fn node_ids_stay_exact_through_cap() {
        let mut analyzer = Analyzer::new(vec![Level::Error]);
        let mut buf = Vec::new();
        for i in 0..(evidence::MAX_NODE_IDS + 4) {
            let line = event(
                "26.08.2026 12:00:00.000",
                &format!("author-{i}"),
                "Failed to start bundle",
            );
            analyzer
                .ingest(&line, arrival(), Instant::now(), &mut buf)
                .unwrap();
        }
        let snap = analyzer.snapshot_groups();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].count, (evidence::MAX_NODE_IDS + 4) as u64);
        assert_eq!(snap[0].nodes.len(), evidence::MAX_NODE_IDS);
        assert!(snap[0].nodes_capped);
        assert_eq!(snap[0].node_count, evidence::MAX_NODE_IDS as u64 + 1);
        assert!(snap[0].nodes.iter().any(|id| id == "author-0"));
        assert!(snap[0]
            .nodes
            .iter()
            .any(|id| id == &format!("author-{}", evidence::MAX_NODE_IDS - 1)));
        assert!(!snap[0]
            .nodes
            .iter()
            .any(|id| id == &format!("author-{}", evidence::MAX_NODE_IDS)));
    }

    #[test]
    fn updates_coalesce_to_one_per_second_without_losing_final_count() {
        let mut analyzer = Analyzer::new(vec![Level::Error]);
        let mut buf = Vec::new();
        let t0 = Instant::now();
        analyzer.ingest(ERROR_A, arrival(), t0, &mut buf).unwrap();
        analyzer
            .ingest(
                ERROR_A_REPEAT,
                arrival(),
                t0 + Duration::from_millis(10),
                &mut buf,
            )
            .unwrap();
        analyzer
            .ingest(ERROR_A, arrival(), t0 + Duration::from_millis(20), &mut buf)
            .unwrap();
        assert!(parse_records(&buf)
            .iter()
            .all(|record| record["type"] != "group_updated"));
        analyzer
            .flush_due_updates(t0 + Duration::from_millis(500), &mut buf)
            .unwrap();
        assert!(parse_records(&buf)
            .iter()
            .all(|record| record["type"] != "group_updated"));
        analyzer
            .flush_due_updates(t0 + Duration::from_secs(1), &mut buf)
            .unwrap();
        let updates: Vec<_> = parse_records(&buf)
            .into_iter()
            .filter(|record| record["type"] == "group_updated")
            .collect();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["count"], 3);
        assert_eq!(updates[0]["group_id"], 1);
        assert!(updates[0].get("sample").is_none());
        assert!(updates[0]["fast_rate"].as_f64().unwrap().is_finite());
    }

    #[test]
    fn source_end_flushes_coalesced_final_count() {
        let mut analyzer = Analyzer::new(vec![Level::Error]);
        let mut buf = Vec::new();
        let t0 = Instant::now();
        analyzer.ingest(ERROR_A, arrival(), t0, &mut buf).unwrap();
        analyzer
            .ingest(
                ERROR_A_REPEAT,
                arrival(),
                t0 + Duration::from_millis(1),
                &mut buf,
            )
            .unwrap();
        analyzer
            .flush_pending_updates(t0 + Duration::from_millis(2), &mut buf)
            .unwrap();
        let updates: Vec<_> = parse_records(&buf)
            .into_iter()
            .filter(|record| record["type"] == "group_updated")
            .collect();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["count"], 2);
    }

    #[test]
    fn stderr_tail_keeps_last_64kib_and_marks_truncation() {
        let mut tail = StderrTail::new();
        tail.push(&[b'a'; STDERR_TAIL]);
        let (text, truncated) = tail.snapshot();
        assert_eq!(text.len(), STDERR_TAIL);
        assert!(!truncated);
        tail.push(b"xyz");
        let (text, truncated) = tail.snapshot();
        assert_eq!(text.len(), STDERR_TAIL);
        assert!(truncated);
        assert!(text.ends_with("xyz"));
        let mut huge = StderrTail::new();
        huge.push(&[b'b'; STDERR_TAIL + 8]);
        let (text, truncated) = huge.snapshot();
        assert_eq!(text.len(), STDERR_TAIL);
        assert!(truncated);
        assert!(text.chars().all(|ch| ch == 'b'));
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
}
