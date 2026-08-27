use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use super::cli::Request;
use super::frame::Framer;
use super::live::{accept_frames, Analyzer, ProcessState, PIPE_QUEUE};
use super::tui::{render, App};
use super::tuning::Tuning;
use super::Error;

pub const TARGET_EVENTS_PER_SEC: f64 = 100_000.0;
pub const TARGET_INPUT_LATENCY: Duration = Duration::from_millis(100);
pub const DAY_RSS_LIMIT: u64 = 128 * 1024 * 1024;
pub const ADVERSARIAL_RSS_LIMIT: u64 = 512 * 1024 * 1024;
pub const GUARD_MIN_EVENTS_PER_SEC: f64 = 1_000.0;
pub const SUSTAIN_SECS: u64 = 60;
pub const MIXED_GUARD_EVENTS: u64 = 27;
pub const MIXED_GUARD_SELECTED: u64 = 26;

#[derive(Clone, Debug, PartialEq)]
pub struct Metrics {
    pub events: u64,
    pub bytes: u64,
    pub elapsed: Duration,
    pub peak_rss: u64,
    pub allocations: Option<u64>,
    pub groups: usize,
    pub overflow: u64,
    pub checksum: u64,
    pub selected_events: u64,
}

impl Metrics {
    pub fn events_per_sec(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            0.0
        } else {
            self.events as f64 / secs
        }
    }

    pub fn bytes_per_sec(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            0.0
        } else {
            self.bytes as f64 / secs
        }
    }

    pub fn report(&self, name: &str) {
        let alloc = match self.allocations {
            Some(n) => n.to_string(),
            None => "n/a".into(),
        };
        println!(
            "{name}: events/s={:.0} bytes/s={:.0} peak_rss={} allocations={alloc} groups={} overflow={} checksum={} selected={} elapsed_ms={}",
            self.events_per_sec(),
            self.bytes_per_sec(),
            self.peak_rss,
            self.groups,
            self.overflow,
            self.checksum,
            self.selected_events,
            self.elapsed.as_millis()
        );
    }
}

pub fn peak_rss_bytes() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc != 0 {
        return 0;
    }
    let usage = unsafe { usage.assume_init() };
    let rss = usage.ru_maxrss as u64;
    if cfg!(target_os = "macos") {
        rss
    } else {
        rss.saturating_mul(1024)
    }
}

pub fn line_event(ts: &str, node: &str, logger: &str, message: &str) -> String {
    format!("{ts} {node} *ERROR* [FelixDispatchQueue] {logger} {message}")
}

pub fn warn_event(ts: &str, node: &str, logger: &str, message: &str) -> String {
    format!("{ts} {node} *WARN* [FelixDispatchQueue] {logger} {message}")
}

pub fn stack_event(ts: &str, node: &str, lines: usize) -> String {
    let mut out = String::with_capacity(lines.saturating_mul(48));
    out.push_str(ts);
    out.push(' ');
    out.push_str(node);
    out.push_str(
        " *ERROR* [sling-default-1] com.example.core.servlets.SearchServlet Uncaught exception\n",
    );
    out.push_str("java.lang.RuntimeException: search failed\n");
    let extra = lines.saturating_sub(2);
    for i in 0..extra {
        out.push_str("\tat com.example.Foo.frame");
        out.push_str(&i.to_string());
        out.push_str("(Foo.java:");
        out.push_str(&i.to_string());
        out.push_str(")\n");
    }
    out
}

fn stamp(i: usize) -> String {
    format!(
        "26.08.2026 12:{:02}:{:02}.{:03}",
        (i / 60) % 60,
        i % 60,
        i % 1000
    )
}

pub fn mixed_guard_bytes() -> Vec<u8> {
    let mut buf = Vec::new();
    for i in 0..12 {
        let line = line_event(
            &stamp(i),
            if i % 3 == 0 { "author-0" } else { "author-1" },
            "com.example.Foo",
            "Failed to start bundle",
        );
        push_event(&mut buf, &line);
    }
    for i in 0..5 {
        let line = line_event(
            &stamp(20 + i),
            "author-0",
            "com.example.Foo",
            &format!(
                "Resource not found /content/site/{}/en.html",
                ["us", "de", "fr", "it", "es"][i]
            ),
        );
        push_event(&mut buf, &line);
    }
    for msg in [
        "alpha beta gamma delta epsilon",
        "alpha other unique novel epsilon",
        "alpha BETA gamma delta epsilon",
        "alpha beta GAMMA delta epsilon",
        "alpha OTHER unique novel epsilon",
        "alpha other UNIQUE novel epsilon",
    ] {
        let line = line_event(
            "26.08.2026 12:01:00.000",
            "author-0",
            "com.example.Merge",
            msg,
        );
        push_event(&mut buf, &line);
    }
    push_event(
        &mut buf,
        &stack_event("26.08.2026 12:02:00.000", "author-2", 8),
    );
    push_event(
        &mut buf,
        &stack_event("26.08.2026 12:02:01.000", "author-2", 8),
    );
    push_event(
        &mut buf,
        &warn_event(
            "26.08.2026 12:03:00.000",
            "author-0",
            "com.example.Bar",
            "ignored warn",
        ),
    );
    buf.extend_from_slice(
        b"26.08.2026 12:03:01.000 author-0 *INFO* [FelixDispatchQueue] com.example.Bar chatter\n",
    );
    buf
}

fn push_event(buf: &mut Vec<u8>, event: &str) {
    buf.extend_from_slice(event.as_bytes());
    if !event.ends_with('\n') {
        buf.push(b'\n');
    }
}

fn ingest_bytes(bytes: &[u8], tuning: &Tuning, out: &mut impl Write) -> Result<Metrics, Error> {
    let mut analyzer = Analyzer::for_harness(tuning);
    let mut framer = Framer::with_limits(
        tuning.event_max_bytes,
        tuning.event_max_lines,
        tuning.sample_max_bytes,
    );
    let start = Instant::now();
    let mut events = 0u64;
    let now = Instant::now();
    for chunk in bytes.chunks(64 * 1024) {
        let frames = framer.push(chunk, now);
        events += frames
            .iter()
            .filter(|frame| matches!(frame, super::frame::Frame::Event(_)))
            .count() as u64;
        accept_frames(&mut analyzer, frames, out)?;
    }
    let rest = framer.finish();
    events += rest
        .iter()
        .filter(|frame| matches!(frame, super::frame::Frame::Event(_)))
        .count() as u64;
    accept_frames(&mut analyzer, rest, out)?;
    analyzer.flush_pending_updates(Instant::now(), out)?;
    Ok(metrics_from(
        analyzer,
        events,
        bytes.len() as u64,
        start.elapsed(),
    ))
}

fn metrics_from(analyzer: Analyzer, events: u64, bytes: u64, elapsed: Duration) -> Metrics {
    Metrics {
        events,
        bytes,
        elapsed,
        peak_rss: peak_rss_bytes(),
        allocations: None,
        groups: analyzer.normal_group_count(),
        overflow: analyzer.overflow_count(),
        checksum: analyzer.count_checksum(),
        selected_events: analyzer.selected_events(),
    }
}

pub fn mixed_guard_metrics() -> Result<Metrics, Error> {
    ingest_bytes(&mixed_guard_bytes(), &Tuning::default(), &mut io::sink())
}

pub fn assert_conservation(metrics: &Metrics) -> Result<(), String> {
    if metrics.selected_events < metrics.overflow {
        return Err(format!(
            "overflow {} exceeds selected {}",
            metrics.overflow, metrics.selected_events
        ));
    }
    Ok(())
}

pub fn run_guard() -> Result<(), String> {
    let first = mixed_guard_metrics().map_err(|err| err.to_string())?;
    let second = mixed_guard_metrics().map_err(|err| err.to_string())?;
    assert_conservation(&first)?;
    if first.checksum != second.checksum {
        return Err(format!(
            "checksum not deterministic: {} vs {}",
            first.checksum, second.checksum
        ));
    }
    if first.events != MIXED_GUARD_EVENTS {
        return Err(format!(
            "expected {MIXED_GUARD_EVENTS} framed events, got {}",
            first.events
        ));
    }
    if first.selected_events != MIXED_GUARD_SELECTED {
        return Err(format!(
            "expected {MIXED_GUARD_SELECTED} selected events, got {}",
            first.selected_events
        ));
    }
    if first.overflow != 0 {
        return Err(format!("mixed guard overflowed: {}", first.overflow));
    }
    if first.groups == 0 {
        return Err("mixed guard created no groups".into());
    }
    let hot = replay_mixed(2_000)?;
    if hot.events_per_sec() < GUARD_MIN_EVENTS_PER_SEC {
        return Err(format!(
            "guard throughput {:.0} events/s below {}",
            hot.events_per_sec(),
            GUARD_MIN_EVENTS_PER_SEC
        ));
    }
    overflow_guard()?;
    coalesce_guard()?;
    backpressure_guard()?;
    latency_guard()?;
    Ok(())
}

fn replay_mixed(times: usize) -> Result<Metrics, String> {
    let unit = mixed_guard_bytes();
    let mut buf = Vec::with_capacity(unit.len() * times);
    for _ in 0..times {
        buf.extend_from_slice(&unit);
    }
    ingest_bytes(&buf, &Tuning::default(), &mut io::sink()).map_err(|err| err.to_string())
}

fn overflow_guard() -> Result<(), String> {
    let tuning = Tuning {
        max_groups: 8,
        similarity: 1.0,
        bucket_cap: 8,
        ..Tuning::default()
    };
    let mut buf = Vec::new();
    for i in 0..20 {
        let line = line_event(
            &stamp(i),
            &format!("author-{}", i % 3),
            &format!("com.example.Logger{i}"),
            &format!("unique-token-{i}"),
        );
        push_event(&mut buf, &line);
    }
    for i in 0..4 {
        let line = line_event(
            &stamp(100 + i),
            "author-0",
            "com.example.Logger0",
            "unique-token-0",
        );
        push_event(&mut buf, &line);
    }
    let metrics = ingest_bytes(&buf, &tuning, &mut io::sink()).map_err(|err| err.to_string())?;
    if metrics.groups != 8 {
        return Err(format!("expected 8 normal groups, got {}", metrics.groups));
    }
    if metrics.overflow != 12 {
        return Err(format!("expected overflow 12, got {}", metrics.overflow));
    }
    if metrics.selected_events != 24 {
        return Err(format!(
            "known counts dropped: selected {}",
            metrics.selected_events
        ));
    }
    if metrics.selected_events != metrics.overflow + 12 {
        return Err("known group counts do not plus overflow equal selected".into());
    }
    Ok(())
}

struct CountingSink {
    writes: usize,
}

impl Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn coalesce_guard() -> Result<(), String> {
    let mut sink = CountingSink { writes: 0 };
    let mut buf = Vec::new();
    for i in 0..40 {
        let line = line_event(
            &stamp(i),
            "author-0",
            "com.example.Foo",
            "Failed to start bundle",
        );
        push_event(&mut buf, &line);
    }
    let metrics =
        ingest_bytes(&buf, &Tuning::default(), &mut sink).map_err(|err| err.to_string())?;
    if metrics.selected_events != 40 {
        return Err(format!(
            "coalesce dropped counts {}",
            metrics.selected_events
        ));
    }
    if sink.writes >= 40 {
        return Err(format!(
            "output not coalesced: {} writes for 40 events",
            sink.writes
        ));
    }
    Ok(())
}

struct SlowSink {
    delay: Duration,
}

impl Write for SlowSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        std::thread::sleep(self.delay);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn backpressure_guard() -> Result<(), String> {
    use std::sync::Arc;

    let peak = Arc::new(AtomicUsize::new(0));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::sync_channel::<Option<Vec<u8>>>(PIPE_QUEUE);
    let producer = std::thread::spawn({
        let in_flight = Arc::clone(&in_flight);
        let peak = Arc::clone(&peak);
        move || {
            for i in 0..64 {
                let line = line_event(
                    &stamp(i),
                    "author-0",
                    "com.example.Foo",
                    "Failed to start bundle",
                );
                let mut chunk = Vec::new();
                push_event(&mut chunk, &line);
                if tx.send(Some(chunk)).is_err() {
                    break;
                }
                let next = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(next, Ordering::SeqCst);
            }
            let _ = tx.send(None);
        }
    });
    let mut analyzer = Analyzer::for_harness(&Tuning::default());
    let mut framer = Framer::with_limits(
        Tuning::default().event_max_bytes,
        Tuning::default().event_max_lines,
        Tuning::default().sample_max_bytes,
    );
    let mut slow = SlowSink {
        delay: Duration::from_millis(1),
    };
    while let Ok(Some(chunk)) = rx.recv() {
        let now = Instant::now();
        accept_frames(&mut analyzer, framer.push(&chunk, now), &mut slow)
            .map_err(|err| err.to_string())?;
        in_flight.fetch_sub(1, Ordering::SeqCst);
    }
    accept_frames(&mut analyzer, framer.finish(), &mut slow).map_err(|err| err.to_string())?;
    producer
        .join()
        .map_err(|_| "producer panicked".to_owned())?;
    let peak = peak.load(Ordering::SeqCst);
    if peak > PIPE_QUEUE + 1 {
        return Err(format!(
            "unbounded queue growth: peak {peak} over PIPE_QUEUE {PIPE_QUEUE}"
        ));
    }
    if analyzer.selected_events() != 64 {
        return Err(format!(
            "slow output dropped known counts {}",
            analyzer.selected_events()
        ));
    }
    Ok(())
}

fn latency_guard() -> Result<(), String> {
    let mut groups = Vec::new();
    for i in 0..64 {
        let line = line_event(
            &stamp(i),
            "author-0",
            &format!("com.example.Logger{i}"),
            &format!("unique-token-{i}"),
        );
        groups.push(line);
    }
    let mut buf = Vec::new();
    for line in &groups {
        push_event(&mut buf, line);
    }
    let mut analyzer = Analyzer::for_harness(&Tuning::default());
    let mut framer = Framer::with_limits(
        Tuning::default().event_max_bytes,
        Tuning::default().event_max_lines,
        Tuning::default().sample_max_bytes,
    );
    accept_frames(
        &mut analyzer,
        framer.push(&buf, Instant::now()),
        &mut io::sink(),
    )
    .map_err(|err| err.to_string())?;
    accept_frames(&mut analyzer, framer.finish(), &mut io::sink())
        .map_err(|err| err.to_string())?;
    let snap = analyzer.snapshot(
        &Request::harness(),
        ProcessState::AioRunning,
        Instant::now(),
    );
    let mut app = App::new(snap);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).map_err(|err| err.to_string())?;
    let start = Instant::now();
    let _ = app.handle(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    terminal
        .draw(|frame| render(frame, &app))
        .map_err(|err| err.to_string())?;
    let elapsed = start.elapsed();
    if elapsed > TARGET_INPUT_LATENCY {
        return Err(format!(
            "input-to-visible-selection {}ms exceeds 100ms",
            elapsed.as_millis()
        ));
    }
    Ok(())
}

pub fn sustain_mixed(secs: u64) -> Result<Metrics, Error> {
    let tuning = Tuning::default();
    let mut analyzer = Analyzer::for_harness(&tuning);
    let mut framer = Framer::with_limits(
        tuning.event_max_bytes,
        tuning.event_max_lines,
        tuning.sample_max_bytes,
    );
    let batch = mixed_batch();
    let start = Instant::now();
    let deadline = start + Duration::from_secs(secs);
    let mut events = 0u64;
    let mut bytes = 0u64;
    let mut round = 0usize;
    while Instant::now() < deadline {
        let mut chunk = Vec::new();
        for event in &batch {
            let rewritten = rewrite_stamp(event, round);
            push_event(&mut chunk, &rewritten);
            round = round.wrapping_add(1);
        }
        bytes += chunk.len() as u64;
        let frames = framer.push(&chunk, Instant::now());
        events += frames
            .iter()
            .filter(|frame| matches!(frame, super::frame::Frame::Event(_)))
            .count() as u64;
        accept_frames(&mut analyzer, frames, &mut io::sink())?;
    }
    let rest = framer.finish();
    events += rest
        .iter()
        .filter(|frame| matches!(frame, super::frame::Frame::Event(_)))
        .count() as u64;
    accept_frames(&mut analyzer, rest, &mut io::sink())?;
    analyzer.flush_pending_updates(Instant::now(), &mut io::sink())?;
    Ok(metrics_from(analyzer, events, bytes, start.elapsed()))
}

fn mixed_batch() -> Vec<String> {
    vec![
        line_event(
            "26.08.2026 12:00:00.000",
            "author-0",
            "com.example.Foo",
            "Failed to start bundle",
        ),
        line_event(
            "26.08.2026 12:00:00.000",
            "author-1",
            "com.example.Foo",
            "Failed to start bundle",
        ),
        line_event(
            "26.08.2026 12:00:00.000",
            "author-0",
            "com.example.Foo",
            "Resource not found /content/site/us/en.html",
        ),
        line_event(
            "26.08.2026 12:00:00.000",
            "author-0",
            "com.example.Foo",
            "Resource not found /content/site/de/en.html",
        ),
        line_event(
            "26.08.2026 12:00:00.000",
            "author-0",
            "com.example.Merge",
            "alpha beta gamma delta epsilon",
        ),
        line_event(
            "26.08.2026 12:00:00.000",
            "author-0",
            "com.example.Merge",
            "alpha other unique novel epsilon",
        ),
        stack_event("26.08.2026 12:00:00.000", "author-2", 345),
        warn_event(
            "26.08.2026 12:00:00.000",
            "author-0",
            "com.example.Bar",
            "ignored warn",
        ),
    ]
}

fn rewrite_stamp(event: &str, i: usize) -> String {
    let rest = event.get(23..).unwrap_or(event);
    format!("{}{rest}", stamp(i))
}

pub fn adversarial_unique(n: u32) -> Result<Metrics, Error> {
    let tuning = Tuning {
        similarity: 1.0,
        bucket_cap: 1_000,
        max_groups: n,
        ..Tuning::default()
    };
    let mut analyzer = Analyzer::for_harness(&tuning);
    let mut framer = Framer::with_limits(
        tuning.event_max_bytes,
        tuning.event_max_lines,
        tuning.sample_max_bytes,
    );
    let start = Instant::now();
    let extra = 32u32;
    let mut events = 0u64;
    let mut bytes = 0u64;
    let total = n.saturating_add(extra);
    for i in 0..total {
        let line = line_event(
            &stamp(i as usize),
            &format!("author-{}", i % 3),
            &format!("com.example.Logger{i}"),
            &format!("unique-token-{i}"),
        );
        let mut chunk = Vec::new();
        push_event(&mut chunk, &line);
        bytes += chunk.len() as u64;
        let frames = framer.push(&chunk, Instant::now());
        events += frames
            .iter()
            .filter(|frame| matches!(frame, super::frame::Frame::Event(_)))
            .count() as u64;
        accept_frames(&mut analyzer, frames, &mut io::sink())?;
    }
    let rest = framer.finish();
    events += rest
        .iter()
        .filter(|frame| matches!(frame, super::frame::Frame::Event(_)))
        .count() as u64;
    accept_frames(&mut analyzer, rest, &mut io::sink())?;
    Ok(metrics_from(analyzer, events, bytes, start.elapsed()))
}

pub fn ingest_path(path: &str) -> Result<Metrics, Error> {
    let bytes = std::fs::read(path).map_err(|err| Error::Io(err.to_string()))?;
    ingest_bytes(&bytes, &Tuning::default(), &mut io::sink())
}

pub fn blocking_evidence(name: &str, metrics: &Metrics, reason: &str) -> String {
    format!(
        "BLOCKED {name}: {reason}\n  events/s={:.0} bytes/s={:.0} peak_rss={} groups={} overflow={} checksum={} selected={}\n  Do not relax thresholds. Record this report as blocking evidence for issue #24.",
        metrics.events_per_sec(),
        metrics.bytes_per_sec(),
        metrics.peak_rss,
        metrics.groups,
        metrics.overflow,
        metrics.checksum,
        metrics.selected_events
    )
}

pub fn run_release() -> Result<(), String> {
    println!("aemlog perf harness (release internals; production CLI stays live-AIO only)");
    let mixed = mixed_guard_metrics().map_err(|err| err.to_string())?;
    mixed.report("mixed-guard");
    let sustain = sustain_mixed(SUSTAIN_SECS).map_err(|err| err.to_string())?;
    sustain.report("sustain-60s");
    if sustain.events_per_sec() < TARGET_EVENTS_PER_SEC {
        return Err(blocking_evidence(
            "sustain-60s",
            &sustain,
            "throughput below 100000 events/s",
        ));
    }
    if sustain.selected_events != sustain.events {
        return Err(blocking_evidence(
            "sustain-60s",
            &sustain,
            "known-group counts dropped",
        ));
    }
    latency_guard()?;
    let adversarial = adversarial_unique(100_000).map_err(|err| err.to_string())?;
    adversarial.report("adversarial-100k");
    if adversarial.groups != 100_000 {
        return Err(blocking_evidence(
            "adversarial-100k",
            &adversarial,
            "normal groups did not reach 100000",
        ));
    }
    if adversarial.overflow == 0 {
        return Err(blocking_evidence(
            "adversarial-100k",
            &adversarial,
            "later unique events did not route to overflow",
        ));
    }
    if adversarial.peak_rss > ADVERSARIAL_RSS_LIMIT {
        return Err(blocking_evidence(
            "adversarial-100k",
            &adversarial,
            "peak RSS above 512 MiB",
        ));
    }
    match std::env::var("AEMLOG_DAY_LOG") {
        Ok(path) if !path.is_empty() => {
            let day = ingest_path(&path).map_err(|err| err.to_string())?;
            day.report("full-day");
            if day.peak_rss > DAY_RSS_LIMIT {
                return Err(blocking_evidence(
                    "full-day",
                    &day,
                    "peak RSS above 128 MiB",
                ));
            }
            if let Ok(expected) = std::env::var("AEMLOG_DAY_EVENTS") {
                let expected: u64 = expected.parse().map_err(|err| format!("{err}"))?;
                if day.events != expected {
                    return Err(blocking_evidence(
                        "full-day",
                        &day,
                        &format!("event count {0} != expected {expected}", day.events),
                    ));
                }
            }
        }
        _ => println!("full-day: skipped (set AEMLOG_DAY_LOG to a private, uncommitted log)"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_guard_is_deterministic() {
        run_guard().expect("guard");
    }
}
