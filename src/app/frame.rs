use std::time::{Duration, Instant};

use super::cli::Level;

/// Flush a pending event after this much byte inactivity.
pub(super) const IDLE_FLUSH: Duration = Duration::from_secs(2);

/// A complete framed AEM event, or bytes that are not part of one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Frame {
    Event(String),
    Unframed(String),
}

/// Turns arbitrary byte chunks into complete AEM error-log events.
///
/// A valid event starts with `dd.MM.yyyy HH:mm:ss.SSS`, a node token, `*LEVEL*`,
/// and a balanced bracketed thread/request context. A later valid header, two
/// seconds without bytes, or EOF ends the pending event. Continuation text after
/// an idle flush is unframed rather than attached to the previous event.
pub(super) struct Framer {
    buf: Vec<u8>,
    pending: Option<String>,
    last_byte_at: Option<Instant>,
}

impl Framer {
    pub(super) fn new() -> Self {
        Self {
            buf: Vec::new(),
            pending: None,
            last_byte_at: None,
        }
    }

    pub(super) fn push(&mut self, bytes: &[u8], now: Instant) -> Vec<Frame> {
        if bytes.is_empty() {
            return Vec::new();
        }
        self.last_byte_at = Some(now);
        self.buf.extend_from_slice(bytes);
        self.drain_complete_lines()
    }

    pub(super) fn poll_idle(&mut self, now: Instant) -> Vec<Frame> {
        let Some(last) = self.last_byte_at else {
            return Vec::new();
        };
        if now.saturating_duration_since(last) < IDLE_FLUSH {
            return Vec::new();
        }
        self.flush_tail_and_pending()
    }

    pub(super) fn finish(&mut self) -> Vec<Frame> {
        self.flush_tail_and_pending()
    }

    fn drain_complete_lines(&mut self) -> Vec<Frame> {
        let mut out = Vec::new();
        while let Some(line) = take_line(&mut self.buf) {
            apply_line(&mut self.pending, &line, &mut out);
        }
        out
    }

    fn flush_tail_and_pending(&mut self) -> Vec<Frame> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let tail = String::from_utf8_lossy(&self.buf).into_owned();
            self.buf.clear();
            apply_line(&mut self.pending, &tail, &mut out);
        }
        if let Some(event) = self.pending.take() {
            out.push(Frame::Event(event));
        }
        out
    }
}

/// Level and grouping key (`*LEVEL*` through the end of the event, timestamp and
/// node excluded).
pub(super) fn parse_event(event: &str) -> Option<(Level, &str)> {
    let first = event
        .split_once('\n')
        .map(|(line, _)| line)
        .unwrap_or(event);
    let first = first.strip_suffix('\r').unwrap_or(first);
    let header = parse_header(first)?;
    let key_offset = first.len() - header.key.len();
    Some((header.level, event.get(key_offset..)?))
}

struct Header<'a> {
    level: Level,
    key: &'a str,
}

fn parse_header(line: &str) -> Option<Header<'_>> {
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
    let after_level = after_level.strip_prefix(' ')?;
    let _context = take_balanced_brackets(after_level)?;
    Some(Header { level, key: rest })
}

fn take_balanced_brackets(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.first().copied() != Some(b'[') {
        return None;
    }
    let mut depth = 0i32;
    for (i, &c) in bytes.iter().enumerate() {
        match c {
            b'[' => depth += 1,
            b']' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    return Some(&s[..=i]);
                }
            }
            _ => {}
        }
    }
    None
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

fn take_line(buf: &mut Vec<u8>) -> Option<String> {
    let i = buf.iter().position(|&b| b == b'\n')?;
    let line = String::from_utf8_lossy(&buf[..=i]).into_owned();
    buf.drain(..=i);
    Some(line)
}

fn apply_line(pending: &mut Option<String>, line: &str, out: &mut Vec<Frame>) {
    let content = line_content(line);
    if parse_header(content).is_some() {
        if let Some(prev) = pending.take() {
            out.push(Frame::Event(prev));
        }
        *pending = Some(line.to_owned());
    } else if let Some(event) = pending.as_mut() {
        event.push_str(line);
    } else if !content.is_empty() {
        out.push(Frame::Unframed(line.to_owned()));
    }
}

fn line_content(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORDINARY: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/frame/ordinary-thread.log"
    ));
    const NESTED_HTTP: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/frame/nested-http.log"
    ));
    const ONE_LINE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/frame/one-line.log"
    ));
    const LONG_STACK: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/frame/long-stack.log"
    ));
    const NESTED_CAUSES: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/frame/nested-causes.log"
    ));
    const NO_TRAILING: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/frame/no-trailing-newline.log"
    ));

    const HEADER_A: &str =
        "26.08.2026 12:00:00.123 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo boom";
    const HEADER_B: &str =
        "26.08.2026 12:00:01.000 author-0 *ERROR* [FelixDispatchQueue] com.example.Bar other";
    const NESTED_HEADER: &str = "26.08.2026 12:00:01.456 author-0 *ERROR* [192.0.2.10 [1724666401456] GET /content/site/us/en.html HTTP/1.1] com.example.core.filters.ErrorFilter Uncaught request exception";

    fn events_of(frames: &[Frame]) -> Vec<&str> {
        frames
            .iter()
            .filter_map(|frame| match frame {
                Frame::Event(event) => Some(event.as_str()),
                Frame::Unframed(_) => None,
            })
            .collect()
    }

    fn unframed_of(frames: &[Frame]) -> Vec<&str> {
        frames
            .iter()
            .filter_map(|frame| match frame {
                Frame::Unframed(text) => Some(text.as_str()),
                Frame::Event(_) => None,
            })
            .collect()
    }

    fn feed(chunks: &[&[u8]]) -> Vec<Frame> {
        let mut framer = Framer::new();
        let t0 = Instant::now();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(framer.push(chunk, t0));
        }
        out.extend(framer.finish());
        out
    }

    fn feed_bytes(bytes: &[u8]) -> Vec<Frame> {
        feed(&[bytes])
    }

    fn one_byte_chunks(bytes: &[u8]) -> Vec<&[u8]> {
        bytes.chunks(1).collect()
    }

    fn line_chunks(bytes: &[u8]) -> Vec<&[u8]> {
        let mut chunks = Vec::new();
        let mut start = 0;
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                chunks.push(&bytes[start..=i]);
                start = i + 1;
            }
        }
        if start < bytes.len() {
            chunks.push(&bytes[start..]);
        }
        chunks
    }

    fn mixed_stream() -> String {
        let mut out = String::new();
        for part in [ORDINARY, NESTED_HTTP, ONE_LINE, LONG_STACK, NESTED_CAUSES] {
            out.push_str(part);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        out.push_str(NO_TRAILING);
        out
    }

    #[test]
    fn header_requires_timestamp_node_level_and_balanced_context() {
        let (level, key) = parse_event(HEADER_A).expect("header");
        assert_eq!(level, Level::Error);
        assert_eq!(key, "*ERROR* [FelixDispatchQueue] com.example.Foo boom");
        assert!(parse_event("not a log line").is_none());
        assert!(parse_event("26.08.2026 12:00:00.123").is_none());
        assert!(parse_event("26.08.2026 12:00:00.123 node *NOPE* x").is_none());
        assert!(parse_event("26.08.2026 12:00:00.123 node *ERROR* no-brackets").is_none());
        assert!(parse_event("26.08.2026 12:00:00.123 node *ERROR* [unbalanced").is_none());
        assert!(parse_event(NESTED_HEADER).is_some());
    }

    #[test]
    fn nested_request_brackets_do_not_end_context_early() {
        let (level, key) = parse_event(NESTED_HEADER).expect("nested header");
        assert_eq!(level, Level::Error);
        assert!(
            key.starts_with(
                "*ERROR* [192.0.2.10 [1724666401456] GET /content/site/us/en.html HTTP/1.1]"
            ),
            "{key}"
        );
        assert!(
            key.contains("com.example.core.filters.ErrorFilter"),
            "{key}"
        );
        let naive = NESTED_HEADER.split_once(']').map(|(left, _)| left).unwrap();
        assert!(
            naive.ends_with("[192.0.2.10 [1724666401456"),
            "naive first-bracket split must stop at the epoch closer: {naive}"
        );
        assert!(parse_event(NESTED_HEADER).is_some());
    }

    #[test]
    fn continuation_blank_stack_caused_by_and_suppressed_stay_in_event() {
        let frames = feed_bytes(NESTED_CAUSES.as_bytes());
        let events = events_of(&frames);
        assert_eq!(events.len(), 1, "{frames:?}");
        let event = events[0];
        assert!(event.contains("Caused by: java.lang.IllegalStateException"));
        assert!(event.contains("Suppressed: java.io.IOException"));
        assert!(event.contains("Caused by: javax.jcr.RepositoryException"));
        assert!(event.contains("\n\n"));
        assert!(event.contains("\tat com.example.core.servlets.ExportServlet.doGet"));
        assert_eq!(event, NESTED_CAUSES);
    }

    #[test]
    fn chunking_is_deterministic() {
        let bytes = mixed_stream();
        let bytes = bytes.as_bytes();
        let whole = feed_bytes(bytes);
        let one = feed(&one_byte_chunks(bytes));
        let arbitrary = feed(&bytes.chunks(7).collect::<Vec<_>>());
        let lines = feed(&line_chunks(bytes));
        assert_eq!(one, whole);
        assert_eq!(arbitrary, whole);
        assert_eq!(lines, whole);
        assert_eq!(events_of(&whole).len(), 6);
        assert!(unframed_of(&whole).is_empty());
    }

    #[test]
    fn following_header_flushes_immediately() {
        let mut framer = Framer::new();
        let t0 = Instant::now();
        let input = format!("{HEADER_A}\n\tat com.example.Foo.bar(Foo.java:42)\n{HEADER_B}\n");
        let frames = framer.push(input.as_bytes(), t0);
        let first = format!("{HEADER_A}\n\tat com.example.Foo.bar(Foo.java:42)\n");
        assert_eq!(events_of(&frames), [first.as_str()]);
        let rest = framer.finish();
        let second = format!("{HEADER_B}\n");
        assert_eq!(events_of(&rest), [second.as_str()]);
    }

    #[test]
    fn idle_flush_after_two_seconds_and_not_before() {
        let mut framer = Framer::new();
        let t0 = Instant::now();
        let input = format!("{HEADER_A}\n\tat com.example.Foo.bar(Foo.java:42)\n");
        assert!(framer.push(input.as_bytes(), t0).is_empty());
        assert!(framer
            .poll_idle(t0 + Duration::from_millis(1999))
            .is_empty());
        let flushed = framer.poll_idle(t0 + IDLE_FLUSH);
        assert_eq!(events_of(&flushed), [input.as_str()]);
        assert!(framer.poll_idle(t0 + Duration::from_secs(4)).is_empty());
    }

    #[test]
    fn eof_flushes_immediately() {
        let mut framer = Framer::new();
        let t0 = Instant::now();
        let input = format!("{HEADER_A}\n\tat com.example.Foo.bar(Foo.java:42)");
        assert!(framer.push(input.as_bytes(), t0).is_empty());
        let flushed = framer.finish();
        assert_eq!(events_of(&flushed), [input.as_str()]);
    }

    #[test]
    fn late_continuation_after_idle_is_unframed() {
        let mut framer = Framer::new();
        let t0 = Instant::now();
        let event = format!("{HEADER_A}\n\tat com.example.Foo.bar(Foo.java:42)\n");
        assert!(framer.push(event.as_bytes(), t0).is_empty());
        let flushed = framer.poll_idle(t0 + IDLE_FLUSH);
        assert_eq!(events_of(&flushed), [event.as_str()]);

        let late = "\tat com.example.Foo.baz(Foo.java:99)\n";
        let frames = framer.push(late.as_bytes(), t0 + IDLE_FLUSH);
        assert!(events_of(&frames).is_empty(), "{frames:?}");
        assert_eq!(unframed_of(&frames), [late]);
    }

    #[test]
    fn anonymized_fixtures_cover_required_shapes() {
        let ordinary = feed_bytes(ORDINARY.as_bytes());
        assert_eq!(events_of(&ordinary), [ORDINARY]);
        assert!(ORDINARY.contains("[FelixDispatchQueue]"));

        let nested = feed_bytes(NESTED_HTTP.as_bytes());
        assert_eq!(events_of(&nested), [NESTED_HTTP]);
        assert!(NESTED_HTTP.contains("[192.0.2.10 [1724666401456]"));

        let one = feed_bytes(ONE_LINE.as_bytes());
        assert_eq!(events_of(&one), [ONE_LINE]);
        assert_eq!(ONE_LINE.lines().count(), 1);
        assert!(ONE_LINE.ends_with('\n'));

        let stack = feed_bytes(LONG_STACK.as_bytes());
        assert_eq!(events_of(&stack), [LONG_STACK]);
        assert!(LONG_STACK.matches("\tat ").count() > 20);

        let causes = feed_bytes(NESTED_CAUSES.as_bytes());
        assert_eq!(events_of(&causes), [NESTED_CAUSES]);
        assert!(NESTED_CAUSES.contains("Caused by:"));
        assert!(NESTED_CAUSES.contains("Suppressed:"));

        let tail = feed_bytes(NO_TRAILING.as_bytes());
        assert_eq!(events_of(&tail), [NO_TRAILING]);
        assert!(!NO_TRAILING.ends_with('\n'));
        assert!(parse_event(NO_TRAILING).is_some());
    }

    #[test]
    fn crlf_and_blank_lines_are_preserved() {
        let input = format!("{HEADER_A}\r\n\r\n\tat com.example.Foo.bar(Foo.java:42)\r\n");
        let frames = feed_bytes(input.as_bytes());
        assert_eq!(events_of(&frames), [input.as_str()]);
    }
}
