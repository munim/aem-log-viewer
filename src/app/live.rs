use std::collections::HashMap;
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
use super::source;
use super::time::{self, TimeInterpreter};
use super::Error;

struct Group {
    id: u64,
    count: u64,
    /// Monotonic source clock for later rate work. Never moves backward.
    #[allow(dead_code)]
    latest_effective: DateTime<Utc>,
}

struct Analyzer {
    session_id: String,
    levels: Vec<Level>,
    groups: HashMap<String, Group>,
    next_group_id: u64,
    sample_max_bytes: usize,
    times: TimeInterpreter,
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
        node: &'a str,
        level: &'a str,
        thread: &'a str,
        logger: &'a str,
        message: &'a str,
        request_context: Option<&'a frame::RequestContext<'a>>,
        terminal_exception: Option<&'a str>,
        terminal_frame: Option<&'a str>,
        source_offsets: frame::SourceOffsets,
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
    );
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
                    diag.sample.trim_end_matches(['\r', '\n']),
                );
            }
        }
    }
    Ok(())
}

impl Analyzer {
    #[cfg(test)]
    fn new(levels: Vec<Level>) -> Self {
        Self::with_sample_limit(levels, u64::MAX, TimeInterpreter::new(Timezone::Utc))
    }

    fn with_sample_limit(
        levels: Vec<Level>,
        sample_max_bytes: u64,
        times: TimeInterpreter,
    ) -> Self {
        Self {
            session_id: Uuid::new_v4().to_string(),
            levels,
            groups: HashMap::new(),
            next_group_id: 1,
            sample_max_bytes: usize::try_from(sample_max_bytes)
                .unwrap_or(usize::MAX)
                .max(1),
            times,
        }
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
                metadata.timestamp
            );
        }
        let key = metadata.grouping_key();
        if let Some(group) = self.groups.get_mut(&key) {
            group.count += 1;
            group.latest_effective =
                time::clamp_effective(Some(group.latest_effective), interpreted.instant);
            let group_id = group.id;
            let count = group.count;
            return emit(
                out,
                &Record::GroupUpdated {
                    version: 1,
                    session_id: &self.session_id,
                    emitted_at: now_utc(),
                    group_id,
                    count,
                },
            );
        }
        let timestamp = time::rfc3339_millis(interpreted.instant);
        let group_id = self.next_group_id;
        self.next_group_id += 1;
        self.groups.insert(
            key,
            Group {
                id: group_id,
                count: 1,
                latest_effective: interpreted.instant,
            },
        );
        emit(
            out,
            &Record::GroupCreated {
                version: 1,
                session_id: &self.session_id,
                emitted_at: now_utc(),
                group_id,
                count: 1,
                sample: frame::bound_sample(event, self.sample_max_bytes),
                timestamp: &timestamp,
                time_fallback: interpreted.fallback.is_some(),
                node: metadata.node,
                level: metadata.level.as_str(),
                thread: metadata.thread,
                logger: metadata.logger,
                message: metadata.message,
                request_context: metadata.request_context.as_ref(),
                terminal_exception: metadata.terminal_exception,
                terminal_frame: metadata.terminal_frame,
                source_offsets: metadata.offsets,
            },
        )
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
        let mut analyzer = Analyzer::new(levels);
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
