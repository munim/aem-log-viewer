use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

#[cfg(test)]
use super::cli::Timezone;
use super::cli::{Level, Request};
use super::frame::{self, Frame, Framer};
use super::rate::{RateParams, RateState};
#[cfg(test)]
use super::rate::{RateSnapshot, View};
use super::redact::{RedactedRequestContext, Redactor};
use super::source;
use super::template::{BucketKey, TemplateStore};
use super::time::{self, TimeInterpreter};
#[cfg(test)]
use super::tuning::{DEFAULT_BUCKET_CAP, DEFAULT_SIMILARITY};
use super::Error;

struct Group {
    id: u64,
    count: u64,
    first_seen: DateTime<Utc>,
    latest_effective: DateTime<Utc>,
    nodes: BTreeSet<String>,
    sample: String,
    muted: bool,
    bucket: BucketKey,
    index: usize,
    rate: RateState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GroupAggregate {
    id: u64,
    count: u64,
    first_seen: DateTime<Utc>,
    last_seen: DateTime<Utc>,
    template: Vec<String>,
    nodes: Vec<String>,
    sample: String,
    muted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
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
    removed: HashSet<u64>,
    next_group_id: u64,
    sample_max_bytes: usize,
    times: TimeInterpreter,
    redactor: Redactor,
    raw_sample: bool,
    templates: TemplateStore,
    rate_params: RateParams,
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
        sample: String,
        timestamp: &'a str,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        time_fallback: bool,
        node: String,
        level: &'a str,
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
        sample: String,
    },
    #[serde(rename = "group_updated")]
    GroupUpdated {
        version: u32,
        session_id: &'a str,
        emitted_at: String,
        group_id: u64,
        count: u64,
    },
    #[serde(rename = "source_ended")]
    SourceEnded {
        version: u32,
        session_id: &'a str,
        emitted_at: String,
        status: Option<i32>,
    },
}

static USER_STOP: AtomicBool = AtomicBool::new(false);

const STOP_POLL: Duration = Duration::from_millis(50);

extern "C" fn on_sigint(_: libc::c_int) {
    USER_STOP.store(true, Ordering::SeqCst);
}

fn install_user_stop_handler() {
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
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

    let mut analyzer = Analyzer::with_sample_limit(
        request.levels.clone(),
        request.tuning.sample_max_bytes,
        TimeInterpreter::new(request.timezone),
        Redactor::new(request.tuning.extra_patterns.clone()),
        request.raw_sample,
        request.tuning.similarity,
        request.tuning.bucket_cap,
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
    loop {
        if USER_STOP.load(Ordering::SeqCst) {
            user_stop = true;
            break;
        }
        match rx.recv_timeout(STOP_POLL) {
            Ok(Some(chunk)) => {
                accept_frames(&mut analyzer, framer.push(&chunk, Instant::now()), &mut out)?;
            }
            Ok(None) => {
                accept_frames(&mut analyzer, framer.finish(), &mut out)?;
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                accept_frames(&mut analyzer, framer.poll_idle(Instant::now()), &mut out)?;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                accept_frames(&mut analyzer, framer.finish(), &mut out)?;
                break;
            }
        }
    }

    if USER_STOP.load(Ordering::SeqCst) {
        user_stop = true;
    }

    // Drain continues on the helper threads while the group is signaled.
    let status = if user_stop {
        source.shutdown()?
    } else {
        source.wait()?
    };
    let _ = reader.join();
    let _ = drain.join();
    analyzer.emit_source_ended(status, &mut out)?;
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

fn accept_frames(
    analyzer: &mut Analyzer,
    frames: Vec<Frame>,
    out: &mut impl Write,
) -> Result<(), Error> {
    for item in frames {
        match item {
            Frame::Event(event) => analyzer.ingest(&event, Utc::now(), out)?,
            Frame::Diagnostic(diag) => {
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
            }
        }
    }
    Ok(())
}

impl Analyzer {
    #[cfg(test)]
    fn new(levels: Vec<Level>) -> Self {
        Self::with_sample_limit(
            levels,
            u64::MAX,
            TimeInterpreter::new(Timezone::Utc),
            Redactor::default(),
            false,
            DEFAULT_SIMILARITY,
            DEFAULT_BUCKET_CAP,
        )
    }

    fn with_sample_limit(
        levels: Vec<Level>,
        sample_max_bytes: u64,
        times: TimeInterpreter,
        redactor: Redactor,
        raw_sample: bool,
        similarity: f64,
        bucket_cap: u32,
    ) -> Self {
        Self {
            session_id: Uuid::new_v4().to_string(),
            levels,
            groups: HashMap::new(),
            removed: HashSet::new(),
            next_group_id: 1,
            sample_max_bytes: usize::try_from(sample_max_bytes)
                .unwrap_or(usize::MAX)
                .max(1),
            times,
            redactor,
            raw_sample,
            templates: TemplateStore::new(similarity, bucket_cap),
            rate_params: RateParams::default(),
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
        out: &mut impl Write,
    ) -> Result<(), Error> {
        let Some(metadata) = frame::parse_metadata(event) else {
            return Ok(());
        };
        if !self.levels.contains(&metadata.level) {
            return Ok(());
        }
        let interpreted = self.times.interpret(metadata.timestamp, arrived_at);
        if let Some(fault) = interpreted.fallback {
            eprintln!(
                "parser diagnostic: {} sample={}",
                fault.as_str(),
                self.redactor
                    .redact_sample(metadata.timestamp, self.raw_sample)
            );
        }
        let outcome = self.templates.learn(
            metadata.level,
            metadata.logger,
            metadata.terminal_exception,
            metadata.terminal_frame,
            metadata.message,
        );
        let Some(key) = outcome.group_key() else {
            return Ok(());
        };
        let node = self.redactor.redact(metadata.node);
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
        } else {
            let group_id = self.next_group_id;
            self.next_group_id += 1;
            let sample = frame::bound_sample(
                &self.redactor.redact_sample(event, self.raw_sample),
                self.sample_max_bytes,
            );
            let bucket = outcome.bucket().expect("learned bucket").clone();
            let index = outcome.index().expect("learned index");
            self.groups.insert(
                key.clone(),
                Group {
                    id: group_id,
                    count: 1,
                    first_seen: interpreted.instant,
                    latest_effective: interpreted.instant,
                    nodes: BTreeSet::from([node.clone()]),
                    sample: sample.clone(),
                    muted: false,
                    bucket,
                    index,
                    rate: RateState::first(interpreted.instant, &self.rate_params),
                },
            );
            let timestamp = time::rfc3339_millis(interpreted.instant);
            emit(
                out,
                &Record::GroupCreated {
                    version: 1,
                    session_id: &self.session_id,
                    emitted_at: now_utc(),
                    group_id,
                    count: 1,
                    sample,
                    timestamp: &timestamp,
                    time_fallback: interpreted.fallback.is_some(),
                    node,
                    level: metadata.level.as_str(),
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
            vec![DomainChange::Created { group_id, count: 1 }]
        };
        let mut emitted = changes;
        emitted.extend(self.apply_merges(&outcome));
        self.emit_domain_changes(&emitted, out)
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
            keep.nodes.extend(drop.nodes);
            keep.muted |= drop.muted;
            keep.bucket = bucket.clone();
            keep.index = merge.survivor;
            let removed_id = drop.id;
            self.removed.insert(removed_id);
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
            nodes: group.nodes.iter().cloned().collect(),
            sample: group.sample.clone(),
            muted: group.muted,
        }
    }

    fn emit_domain_changes(
        &self,
        changes: &[DomainChange],
        out: &mut impl Write,
    ) -> Result<(), Error> {
        let merged = changes
            .iter()
            .any(|change| matches!(change, DomainChange::Merged { .. }));
        for change in changes {
            match change {
                DomainChange::Created { .. } => {}
                DomainChange::Updated { group_id, count } => {
                    if merged || self.removed.contains(group_id) {
                        continue;
                    }
                    emit(
                        out,
                        &Record::GroupUpdated {
                            version: 1,
                            session_id: &self.session_id,
                            emitted_at: now_utc(),
                            group_id: *group_id,
                            count: *count,
                        },
                    )?;
                }
                DomainChange::Merged {
                    removed_id,
                    survivor,
                } => {
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
                            sample: survivor.sample.clone(),
                        },
                    )?;
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn snapshot_groups(&self) -> Vec<GroupAggregate> {
        let mut groups: Vec<GroupAggregate> = self
            .groups
            .values()
            .filter(|group| !self.removed.contains(&group.id))
            .map(|group| self.aggregate(group))
            .collect();
        groups.sort_by_key(|group| group.id);
        groups
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
    fn mute(&mut self, group_id: u64) {
        for group in self.groups.values_mut() {
            if group.id == group_id {
                group.muted = true;
            }
        }
    }

    fn emit_source_ended(&self, status: Option<i32>, out: &mut impl Write) -> Result<(), Error> {
        emit(
            out,
            &Record::SourceEnded {
                version: 1,
                session_id: &self.session_id,
                emitted_at: now_utc(),
                status,
            },
        )
    }
}

fn emit(out: &mut impl Write, record: &Record<'_>) -> Result<(), Error> {
    serde_json::to_writer(&mut *out, record).map_err(|err| Error::Io(err.to_string()))?;
    out.write_all(b"\n")
        .and_then(|_| out.flush())
        .map_err(|err| Error::Io(err.to_string()))
}

fn now_utc() -> String {
    time::rfc3339_millis(Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::cli::Service;
    use crate::app::tuning::Tuning;
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
        let mut analyzer = Analyzer::with_sample_limit(
            levels,
            u64::MAX,
            TimeInterpreter::new(Timezone::Utc),
            redactor,
            raw_sample,
            DEFAULT_SIMILARITY,
            DEFAULT_BUCKET_CAP,
        );
        let mut buf = Vec::new();
        analyzer.emit_session_started(&request(), &mut buf).unwrap();
        for line in lines {
            analyzer.ingest(line, arrival(), &mut buf).unwrap();
        }
        analyzer.emit_source_ended(Some(0), &mut buf).unwrap();
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
        assert_eq!(recs[2]["count"], 2);
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
                "group_updated",
                "group_created",
                "source_ended"
            ]
        );
        assert_eq!(recs[1]["group_id"], 1);
        assert_eq!(recs[1]["count"], 1);
        assert_eq!(recs[1]["sample"], ERROR_A);
        assert_eq!(recs[2]["group_id"], 1);
        assert_eq!(recs[2]["count"], 2);
        assert!(recs[2].get("sample").is_none());
        assert_eq!(recs[3]["group_id"], 2);
        assert_eq!(recs[3]["count"], 1);
        assert_eq!(recs[1]["session_id"], recs[2]["session_id"]);
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
        analyzer.emit_source_ended(Some(0), &mut buf).unwrap();
        let recs = parse_records(&buf);
        let types: Vec<&str> = recs.iter().map(|r| r["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["session_started", "group_created", "source_ended"]);
        assert_eq!(recs[1]["sample"], ERROR_A);
        assert_eq!(recs[1]["count"], 1);
        assert_eq!(analyzer.groups.len(), 1);
    }

    #[test]
    fn metadata_is_emitted_and_raw_sample_is_bounded() {
        let event = concat!(
            "26.08.2026 12:00:00.123 author-0 *ERROR* [worker-1] com.example.Foo ",
            "message with a deliberately long continuation\n",
            "stack continuation\n",
        );
        let mut analyzer = Analyzer::with_sample_limit(
            vec![Level::Error],
            24,
            TimeInterpreter::new(Timezone::Utc),
            Redactor::default(),
            false,
            DEFAULT_SIMILARITY,
            DEFAULT_BUCKET_CAP,
        );
        let mut buf = Vec::new();
        analyzer.ingest(event, arrival(), &mut buf).unwrap();
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
                "group_updated",
                "group_created",
                "source_ended"
            ]
        );
        assert_eq!(recs[1]["group_id"], 1);
        assert_eq!(recs[2]["group_id"], 1);
        assert_eq!(recs[3]["group_id"], 2);
        assert_eq!(recs[3]["terminal_exception"], "java.lang.RuntimeException");
        assert_eq!(recs[3]["terminal_frame"], "com.example.Foo.bar");
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
        let mut analyzer = Analyzer::with_sample_limit(
            vec![Level::Error],
            max as u64,
            TimeInterpreter::new(Timezone::Utc),
            Redactor::default(),
            false,
            DEFAULT_SIMILARITY,
            DEFAULT_BUCKET_CAP,
        );
        let mut buf = Vec::new();
        analyzer.ingest(&event, arrival(), &mut buf).unwrap();
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
        let mut analyzer = Analyzer::with_sample_limit(
            vec![Level::Error],
            u64::MAX,
            TimeInterpreter::new(Timezone::Iana("America/New_York".parse().expect("zone"))),
            Redactor::default(),
            false,
            DEFAULT_SIMILARITY,
            DEFAULT_BUCKET_CAP,
        );
        let mut buf = Vec::new();
        analyzer.ingest(ERROR_A, arrival(), &mut buf).unwrap();
        let record = parse_records(&buf).pop().expect("group record");
        assert_eq!(record["timestamp"], "2026-08-26T16:00:00.123Z");
        assert!(record.get("time_fallback").is_none());
    }

    #[test]
    fn dst_faults_mark_arrival_fallback() {
        let mut analyzer = Analyzer::with_sample_limit(
            vec![Level::Error],
            u64::MAX,
            TimeInterpreter::new(Timezone::Iana("America/New_York".parse().expect("zone"))),
            Redactor::default(),
            false,
            DEFAULT_SIMILARITY,
            DEFAULT_BUCKET_CAP,
        );
        let gap =
            "08.03.2026 02:30:00.000 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo spring";
        let fold =
            "01.11.2026 01:30:00.000 author-0 *ERROR* [FelixDispatchQueue] com.example.Bar fall";
        let mut buf = Vec::new();
        analyzer.ingest(gap, arrival(), &mut buf).unwrap();
        analyzer.ingest(fold, arrival(), &mut buf).unwrap();
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
        analyzer.ingest(later, arrival(), &mut buf).unwrap();
        analyzer.ingest(earlier, arrival(), &mut buf).unwrap();
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
                "group_updated",
                "group_created",
                "group_updated",
                "group_created",
                "source_ended"
            ]
        );
        assert_eq!(recs[1]["group_id"], 1);
        assert_eq!(recs[2]["group_id"], 1);
        assert_eq!(recs[3]["group_id"], 2);
        assert_eq!(recs[4]["group_id"], 2);
        assert_eq!(recs[5]["group_id"], 3);
        assert_eq!(recs[5]["terminal_frame"], "com.example.Foo.bar");
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
        analyzer.ingest(refs[0], arrival(), &mut buf).unwrap();
        analyzer.mute(1);
        for line in &refs[1..] {
            analyzer.ingest(line, arrival(), &mut buf).unwrap();
        }
        analyzer.emit_source_ended(Some(0), &mut buf).unwrap();
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
    }

    #[test]
    fn removed_group_ids_do_not_update_or_reappear() {
        let lines = merge_bridge_lines();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut analyzer = Analyzer::new(vec![Level::Error]);
        let mut buf = Vec::new();
        for line in &refs {
            analyzer.ingest(line, arrival(), &mut buf).unwrap();
        }
        let after_merge = event(
            "26.08.2026 12:00:05.000",
            "author-3",
            "alpha other unique novel epsilon",
        );
        analyzer.ingest(&after_merge, arrival(), &mut buf).unwrap();
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
            analyzer.ingest(line, arrival(), &mut buf).unwrap();
        }
        let expected = fingerprint(&analyzer.snapshot_groups());
        let expected_total: u64 = analyzer.snapshot_groups().iter().map(|g| g.count).sum();
        for seed in 1..48 {
            let mut ordered = corpus.clone();
            shuffle(&mut ordered, seed);
            let mut other = Analyzer::new(vec![Level::Error]);
            let mut out = Vec::new();
            for line in &ordered {
                other.ingest(line, arrival(), &mut out).unwrap();
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
        let mut analyzer = Analyzer::with_sample_limit(
            vec![Level::Error],
            u64::MAX,
            TimeInterpreter::new(Timezone::Iana("America/New_York".parse().expect("zone"))),
            Redactor::default(),
            false,
            DEFAULT_SIMILARITY,
            DEFAULT_BUCKET_CAP,
        );
        let gap =
            "08.03.2026 02:30:00.000 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo spring";
        let mut buf = Vec::new();
        analyzer.ingest(gap, arrival(), &mut buf).unwrap();
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
        analyzer.ingest(&later, arrival(), &mut buf).unwrap();
        analyzer.ingest(&earlier, arrival(), &mut buf).unwrap();
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
            analyzer.ingest(line, arrival(), &mut buf).unwrap();
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
                &mut buf,
            )
            .unwrap();
        analyzer
            .ingest(
                &event("26.08.2026 12:00:50.000", "author-0", "beta error"),
                arrival(),
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
            [2, 1]
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
            [1, 2]
        );
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
