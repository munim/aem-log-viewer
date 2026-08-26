use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::Child;

use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use uuid::Uuid;

use super::cli::{Level, Request};
use super::source;
use super::Error;

struct Group {
    id: u64,
    count: u64,
}

struct Analyzer {
    session_id: String,
    levels: Vec<Level>,
    groups: HashMap<String, Group>,
    next_group_id: u64,
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
        sample: &'a str,
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

pub(super) fn run(request: &Request) -> Result<(), Error> {
    let mut child = spawn(request)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Io("aio stdout was not piped".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::Io("aio stderr was not piped".into()))?;
    let drain = std::thread::spawn(move || {
        let mut stderr = stderr;
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
    });

    let mut analyzer = Analyzer::new(request.levels.clone());
    let mut out = std::io::stdout().lock();
    analyzer.emit_session_started(request, &mut out)?;

    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(|err| Error::Io(err.to_string()))?;
        analyzer.ingest(&line, &mut out)?;
    }

    let status = wait_status(&mut child)?;
    let _ = drain.join();
    analyzer.emit_source_ended(status, &mut out)?;
    Err(Error::UnexpectedEnd(
        status
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".into()),
    ))
}

fn spawn(request: &Request) -> Result<Child, Error> {
    source::command(request)
        .spawn()
        .map_err(|err| Error::Spawn(err.to_string()))
}

fn wait_status(child: &mut Child) -> Result<Option<i32>, Error> {
    child
        .wait()
        .map(|status| status.code())
        .map_err(|err| Error::Io(err.to_string()))
}

impl Analyzer {
    fn new(levels: Vec<Level>) -> Self {
        Self {
            session_id: Uuid::new_v4().to_string(),
            levels,
            groups: HashMap::new(),
            next_group_id: 1,
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

    fn ingest(&mut self, line: &str, out: &mut impl Write) -> Result<(), Error> {
        let Some((level, key)) = parse_aem_header(line) else {
            return Ok(());
        };
        if !self.levels.contains(&level) {
            return Ok(());
        }
        if let Some(group) = self.groups.get_mut(key) {
            group.count += 1;
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
        let group_id = self.next_group_id;
        self.next_group_id += 1;
        self.groups.insert(
            key.to_owned(),
            Group {
                id: group_id,
                count: 1,
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
                sample: line,
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
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Recognize a single-line AEM header: `dd.MM.yyyy HH:mm:ss.SSS <node> *LEVEL* <message>`.
/// The grouping key is the exact `*LEVEL* …` suffix (timestamp and node excluded).
fn parse_aem_header(line: &str) -> Option<(Level, &str)> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let ts = line.get(..23)?;
    if !is_aem_timestamp(ts) {
        return None;
    }
    if line.get(23..24)? != " " {
        return None;
    }
    let rest = line.get(24..)?;
    let (node, rest) = rest.split_once(' ')?;
    if node.is_empty() {
        return None;
    }
    if !rest.starts_with('*') {
        return None;
    }
    let (level_token, after_level) = rest[1..].split_once('*')?;
    let level = Level::from_aem(level_token)?;
    let _message = after_level.strip_prefix(' ')?;
    Some((level, rest))
}

fn is_aem_timestamp(ts: &str) -> bool {
    let b = ts.as_bytes();
    if b.len() != 23 {
        return false;
    }
    let d = |i: usize| b[i].is_ascii_digit();
    d(0) && d(1)
        && b[2] == b'.'
        && d(3)
        && d(4)
        && b[5] == b'.'
        && d(6)
        && d(7)
        && d(8)
        && d(9)
        && b[10] == b' '
        && d(11)
        && d(12)
        && b[13] == b':'
        && d(14)
        && d(15)
        && b[16] == b':'
        && d(17)
        && d(18)
        && b[19] == b'.'
        && d(20)
        && d(21)
        && d(22)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::cli::{Service, Timezone};
    use crate::app::tuning::Tuning;

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
            analyzer.ingest(line, &mut buf).unwrap();
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
    fn parse_requires_timestamp_node_and_level() {
        let (level, key) = parse_aem_header(ERROR_A).expect("header");
        assert_eq!(level, Level::Error);
        assert_eq!(
            key,
            "*ERROR* [FelixDispatchQueue] com.example.Foo Failed to start bundle"
        );
        assert!(parse_aem_header("not a log line").is_none());
        assert!(parse_aem_header("26.08.2026 12:00:00.123").is_none());
        assert!(parse_aem_header("26.08.2026 12:00:00.123 node *NOPE* x").is_none());
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
