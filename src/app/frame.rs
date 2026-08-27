use std::time::{Duration, Instant};

use super::cli::Level;
#[cfg(test)]
use super::tuning::{DEFAULT_EVENT_BYTES, DEFAULT_EVENT_LINES, DEFAULT_SAMPLE_BYTES};

/// Flush a pending event after this much byte inactivity.
pub(super) const IDLE_FLUSH: Duration = Duration::from_secs(2);

/// Why the parser rejected or truncated input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DiagnosticReason {
    EventByteLimit,
    EventLineLimit,
    UnframedPrefix,
    UnframedLate,
    InvalidUtf8,
}

impl DiagnosticReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::EventByteLimit => "event_byte_limit",
            Self::EventLineLimit => "event_line_limit",
            Self::UnframedPrefix => "unframed_prefix",
            Self::UnframedLate => "unframed_late",
            Self::InvalidUtf8 => "invalid_utf8",
        }
    }
}

/// Aggregated parser failure. Separate from AEM error groups.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Diagnostic {
    pub reason: DiagnosticReason,
    pub count: u64,
    pub sample: String,
    pub line: u64,
    pub offset: u64,
}

/// A complete framed AEM event, or a parser diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Frame {
    Event(String),
    Diagnostic(Diagnostic),
}

/// Turns arbitrary byte chunks into complete AEM error-log events.
///
/// A valid event starts with `dd.MM.yyyy HH:mm:ss.SSS`, a node token, `*LEVEL*`,
/// and a balanced bracketed thread/request context. A later valid header, two
/// seconds without bytes, or EOF ends the pending event. Continuation text after
/// an idle flush is unframed rather than attached to the previous event.
///
/// Event byte and line caps are enforced before retained allocation can exceed
/// the configured limit by more than one input chunk. Oversized events emit one
/// truncated event plus one aggregated diagnostic; further continuation is
/// discarded until the next valid header.
pub(super) struct Framer {
    buf: Vec<u8>,
    pending: Option<String>,
    pending_lines: u32,
    last_byte_at: Option<Instant>,
    max_bytes: usize,
    max_lines: u32,
    sample_max: usize,
    buf_offset: u64,
    line_number: u64,
    discarding: bool,
    after_idle: bool,
    unframed: Option<Acc>,
    truncation: Option<Acc>,
}

#[derive(Clone, Debug)]
struct Acc {
    reason: DiagnosticReason,
    count: u64,
    sample: String,
    line: u64,
    offset: u64,
}

impl Acc {
    fn into_diagnostic(self) -> Diagnostic {
        Diagnostic {
            reason: self.reason,
            count: self.count,
            sample: self.sample,
            line: self.line,
            offset: self.offset,
        }
    }
}

impl Framer {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::with_limits(
            DEFAULT_EVENT_BYTES,
            DEFAULT_EVENT_LINES,
            DEFAULT_SAMPLE_BYTES,
        )
    }

    pub(crate) fn with_limits(max_bytes: u64, max_lines: u32, sample_max_bytes: u64) -> Self {
        Self {
            buf: Vec::new(),
            pending: None,
            pending_lines: 0,
            last_byte_at: None,
            max_bytes: usize::try_from(max_bytes).unwrap_or(usize::MAX).max(1),
            max_lines: max_lines.max(1),
            sample_max: usize::try_from(sample_max_bytes)
                .unwrap_or(usize::MAX)
                .max(1),
            buf_offset: 0,
            line_number: 1,
            discarding: false,
            after_idle: false,
            unframed: None,
            truncation: None,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8], now: Instant) -> Vec<Frame> {
        if bytes.is_empty() {
            return Vec::new();
        }
        self.last_byte_at = Some(now);
        self.buf.extend_from_slice(bytes);
        let mut out = self.drain_complete_lines();
        if self.must_force_line() {
            out.extend(self.take_incomplete_as_line());
        }
        debug_assert!(
            self.buf.len() <= self.max_bytes,
            "incomplete line retained {} bytes over limit {}",
            self.buf.len(),
            self.max_bytes
        );
        out
    }

    pub(super) fn poll_idle(&mut self, now: Instant) -> Vec<Frame> {
        let Some(last) = self.last_byte_at else {
            return Vec::new();
        };
        if now.saturating_duration_since(last) < IDLE_FLUSH {
            return Vec::new();
        }
        self.flush_idle()
    }

    pub(crate) fn finish(&mut self) -> Vec<Frame> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let tail = std::mem::take(&mut self.buf);
            self.apply_line(&tail, &mut out);
        }
        self.take_pending(&mut out);
        self.flush_truncation(&mut out);
        self.flush_unframed(&mut out);
        self.discarding = false;
        out
    }

    fn flush_idle(&mut self) -> Vec<Frame> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let tail = std::mem::take(&mut self.buf);
            self.apply_line(&tail, &mut out);
        }
        if self.pending.is_some() {
            self.take_pending(&mut out);
            self.after_idle = true;
        }
        self.flush_unframed(&mut out);
        out
    }

    fn drain_complete_lines(&mut self) -> Vec<Frame> {
        let mut out = Vec::new();
        while let Some(line) = take_line(&mut self.buf) {
            self.apply_line(&line, &mut out);
        }
        out
    }

    fn take_incomplete_as_line(&mut self) -> Vec<Frame> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        let line = std::mem::take(&mut self.buf);
        let mut out = Vec::new();
        self.apply_line(&line, &mut out);
        out
    }

    fn must_force_line(&self) -> bool {
        if self.buf.is_empty() {
            return false;
        }
        self.buf.len() > self.max_bytes
            || self.pending_len().saturating_add(self.buf.len()) > self.max_bytes
    }

    fn pending_len(&self) -> usize {
        self.pending.as_ref().map(String::len).unwrap_or(0)
    }

    fn apply_line(&mut self, raw: &[u8], out: &mut Vec<Frame>) {
        let offset = self.buf_offset;
        let line_no = self.line_number;
        let raw_len = raw.len() as u64;
        let inspect_len = raw.len().min(self.max_bytes.saturating_add(1));
        let text = decode_lossy(&raw[..inspect_len]);

        if let Some((rel, count)) = utf8_faults(raw) {
            out.push(Frame::Diagnostic(Diagnostic {
                reason: DiagnosticReason::InvalidUtf8,
                count,
                sample: bound_sample(&text, self.sample_max),
                line: line_no,
                offset: offset + rel as u64,
            }));
        }

        let content = line_content(&text);
        if parse_header(content).is_some() {
            self.flush_truncation(out);
            self.flush_unframed(out);
            self.discarding = false;
            self.take_pending(out);
            self.after_idle = false;
            self.start_event(text, raw_len, offset, line_no, out);
        } else if self.discarding {
            self.add_truncation(raw_len, &text, line_no, offset);
        } else if self.pending.is_some() {
            self.append_or_truncate(text, raw_len, line_no, offset, out);
        } else if !content.is_empty() {
            self.note_unframed(&text, raw_len, line_no, offset);
        }

        self.buf_offset += raw_len;
        self.line_number += 1;
    }

    fn start_event(
        &mut self,
        text: String,
        raw_len: u64,
        offset: u64,
        line_no: u64,
        out: &mut Vec<Frame>,
    ) {
        if (text.len() as u64) > self.max_bytes as u64 || raw_len > self.max_bytes as u64 {
            let kept = truncate_str(&text, self.max_bytes).to_owned();
            let discarded = raw_len.saturating_sub(kept.len() as u64);
            let disc_off = offset + kept.len() as u64;
            let sample_src = text.get(kept.len()..).unwrap_or("");
            out.push(Frame::Event(kept));
            self.pending = None;
            self.pending_lines = 0;
            self.discarding = true;
            self.note_truncation(
                DiagnosticReason::EventByteLimit,
                discarded,
                sample_src,
                line_no,
                disc_off,
            );
            return;
        }
        self.pending = Some(text);
        self.pending_lines = 1;
    }

    fn append_or_truncate(
        &mut self,
        text: String,
        raw_len: u64,
        line_no: u64,
        offset: u64,
        out: &mut Vec<Frame>,
    ) {
        if self.pending_lines.saturating_add(1) > self.max_lines {
            self.take_pending(out);
            self.discarding = true;
            self.note_truncation(
                DiagnosticReason::EventLineLimit,
                raw_len,
                &text,
                line_no,
                offset,
            );
            return;
        }

        let pending_len = self.pending_len();
        if pending_len.saturating_add(raw_len as usize) > self.max_bytes {
            let room = self.max_bytes.saturating_sub(pending_len);
            let kept = truncate_str(&text, room);
            if let Some(event) = self.pending.as_mut() {
                event.push_str(kept);
            }
            let discarded = raw_len.saturating_sub(kept.len() as u64);
            let disc_off = offset + kept.len() as u64;
            let sample_src = text.get(kept.len()..).unwrap_or("");
            self.take_pending(out);
            self.discarding = true;
            self.note_truncation(
                DiagnosticReason::EventByteLimit,
                discarded,
                sample_src,
                line_no,
                disc_off,
            );
            return;
        }

        if let Some(event) = self.pending.as_mut() {
            event.push_str(&text);
        }
        self.pending_lines = self.pending_lines.saturating_add(1);
    }

    fn take_pending(&mut self, out: &mut Vec<Frame>) {
        if let Some(event) = self.pending.take() {
            out.push(Frame::Event(event));
        }
        self.pending_lines = 0;
    }

    fn note_truncation(
        &mut self,
        reason: DiagnosticReason,
        count: u64,
        sample_src: &str,
        line: u64,
        offset: u64,
    ) {
        match &mut self.truncation {
            Some(acc) => acc.count = acc.count.saturating_add(count),
            None => {
                self.truncation = Some(Acc {
                    reason,
                    count,
                    sample: bound_sample(sample_src, self.sample_max),
                    line,
                    offset,
                });
            }
        }
    }

    fn add_truncation(&mut self, count: u64, sample_src: &str, line: u64, offset: u64) {
        let reason = self
            .truncation
            .as_ref()
            .map(|acc| acc.reason)
            .unwrap_or(DiagnosticReason::EventByteLimit);
        self.note_truncation(reason, count, sample_src, line, offset);
    }

    fn note_unframed(&mut self, text: &str, count: u64, line: u64, offset: u64) {
        let reason = if self.after_idle {
            DiagnosticReason::UnframedLate
        } else {
            DiagnosticReason::UnframedPrefix
        };
        match &mut self.unframed {
            Some(acc) => acc.count = acc.count.saturating_add(count),
            None => {
                self.unframed = Some(Acc {
                    reason,
                    count,
                    sample: bound_sample(text, self.sample_max),
                    line,
                    offset,
                });
            }
        }
    }

    fn flush_truncation(&mut self, out: &mut Vec<Frame>) {
        if let Some(acc) = self.truncation.take() {
            out.push(Frame::Diagnostic(acc.into_diagnostic()));
        }
    }

    fn flush_unframed(&mut self, out: &mut Vec<Frame>) {
        if let Some(acc) = self.unframed.take() {
            out.push(Frame::Diagnostic(acc.into_diagnostic()));
        }
    }
}

#[allow(dead_code)]
/// Level and grouping key retained for the analyzer's legacy parser contract.
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

/// Structured fields extracted from one event header. All text fields borrow
/// the framed event; offsets are byte ranges relative to the event start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct EventMetadata<'a> {
    pub timestamp: &'a str,
    pub node: &'a str,
    pub level: Level,
    pub thread: &'a str,
    pub logger: &'a str,
    pub message: &'a str,
    pub request_context: Option<RequestContext<'a>>,
    pub terminal_exception: Option<&'a str>,
    pub terminal_frame: Option<&'a str>,
    pub offsets: SourceOffsets,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub(super) struct RequestContext<'a> {
    pub client_ip: &'a str,
    pub request_id: &'a str,
    pub method: &'a str,
    pub path: &'a str,
    pub protocol: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub(super) struct SourceOffset {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub(super) struct SourceOffsets {
    pub timestamp: SourceOffset,
    pub node: SourceOffset,
    pub level: SourceOffset,
    pub thread: SourceOffset,
    pub logger: SourceOffset,
    pub message: SourceOffset,
}

impl<'a> EventMetadata<'a> {
    /// Legacy exact-message identity. Request context and thread scheduling
    /// details are evidence only. Live grouping uses [`crate::app::template`].
    #[cfg(test)]
    pub(super) fn grouping_key(&self) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}",
            self.level.as_str(),
            self.logger,
            self.message
        )
    }
}

/// Parses structured metadata from a framed event, including terminal identity.
pub(super) fn parse_metadata(event: &str) -> Option<EventMetadata<'_>> {
    let line_end = event.find('\n').unwrap_or(event.len());
    let line = event
        .get(..line_end)?
        .strip_suffix('\r')
        .unwrap_or(&event[..line_end]);
    let header = parse_header(line)?;
    let thread = header
        .context
        .get(1..header.context.len().checked_sub(1)?)?;
    let after_context = line.get(header.context_end..)?.strip_prefix(' ')?;
    let (logger, message) = parse_logger_message(after_context, thread);
    let base = line.as_ptr() as usize;
    let relative = |slice: &str| SourceOffset {
        start: slice.as_ptr() as usize - base,
        end: slice.as_ptr() as usize - base + slice.len(),
    };
    let logger_offset = if logger == "unknown" {
        SourceOffset { start: 0, end: 0 }
    } else {
        relative(logger)
    };
    let (terminal_exception, terminal_frame) = parse_terminal_identity(event, message);
    Some(EventMetadata {
        timestamp: header.timestamp,
        node: header.node,
        level: header.level,
        thread,
        logger,
        message,
        request_context: parse_request_context(thread),
        terminal_exception,
        terminal_frame,
        offsets: SourceOffsets {
            timestamp: relative(header.timestamp),
            node: relative(header.node),
            level: relative(header.level_token),
            thread: relative(thread),
            logger: logger_offset,
            message: relative(message),
        },
    })
}

/// Last declared exception in event order, plus the first stack frame after it.
/// `Suppressed` declarations replace prior identity. Frame identity is class.method.
fn parse_terminal_identity<'a>(
    event: &'a str,
    header_message: &'a str,
) -> (Option<&'a str>, Option<&'a str>) {
    let mut exception = exception_declaration(header_message);
    let mut frame = None;
    let mut want_frame = exception.is_some();

    let body = match event.split_once('\n') {
        Some((_, rest)) => rest,
        None => return (exception, frame),
    };
    for line in body.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(class) = exception_declaration(line) {
            exception = Some(class);
            frame = None;
            want_frame = true;
            continue;
        }
        if !want_frame {
            continue;
        }
        if let Some(identity) = stack_frame_identity(line) {
            frame = Some(identity);
            want_frame = false;
        }
    }
    (exception, frame)
}

fn exception_declaration(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let line = line
        .strip_prefix("Caused by: ")
        .or_else(|| line.strip_prefix("Suppressed: "))
        .unwrap_or(line);
    let class = java_type_at_start(line)?;
    let rest = &line[class.len()..];
    if rest.is_empty() || rest.starts_with(':') {
        Some(class)
    } else {
        None
    }
}

fn java_type_at_start(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut parts = 0;
    let mut last_start = 0;
    while i < bytes.len() {
        if !is_ident_start(bytes[i]) {
            return None;
        }
        last_start = i;
        i += 1;
        while i < bytes.len() && is_ident_part(bytes[i]) {
            i += 1;
        }
        parts += 1;
        if i < bytes.len() && bytes[i] == b'.' {
            i += 1;
            if i == bytes.len() {
                return None;
            }
            continue;
        }
        break;
    }
    if parts < 2 {
        return None;
    }
    if !s[last_start..i]
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
    {
        return None;
    }
    Some(&s[..i])
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident_part(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

fn stack_frame_identity(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("at ")?;
    let rest = match rest.split_once('/') {
        Some((_, after)) => after,
        None => rest,
    };
    let name = rest.split_once('(')?.0;
    if is_frame_name(name) {
        Some(name)
    } else {
        None
    }
}

fn is_frame_name(s: &str) -> bool {
    let Some((class, method)) = s.rsplit_once('.') else {
        return false;
    };
    if class.is_empty() {
        return false;
    }
    let method_ok = matches!(method, "<init>" | "<clinit>") || is_simple_ident(method);
    method_ok && class.split('.').all(is_simple_ident)
}

fn is_simple_ident(s: &str) -> bool {
    let mut chars = s.bytes();
    matches!(chars.next(), Some(c) if is_ident_start(c)) && chars.all(is_ident_part)
}

fn parse_logger_message<'a>(after_context: &'a str, thread: &'a str) -> (&'a str, &'a str) {
    let mut parts = after_context.splitn(2, char::is_whitespace);
    let first = parts.next().unwrap_or("");
    if first.contains('.') && valid_logger(first) {
        return (first, parts.next().unwrap_or("").trim_start());
    }
    if thread.contains('.') && valid_logger(thread) {
        return (thread, after_context.trim_start());
    }
    ("unknown", after_context.trim_start())
}

fn valid_logger(token: &str) -> bool {
    !token.is_empty()
        && token.split('.').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$'))
        })
}

fn parse_request_context(thread: &str) -> Option<RequestContext<'_>> {
    let (client_ip, rest) = thread.split_once(" [")?;
    let (request_id, request) = rest.split_once("] ")?;
    let mut fields = request.split_whitespace();
    let method = fields.next()?;
    let path = fields.next()?;
    let protocol = fields.next()?;
    if fields.next().is_some() || client_ip.is_empty() || request_id.is_empty() {
        return None;
    }
    Some(RequestContext {
        client_ip,
        request_id,
        method,
        path,
        protocol,
    })
}

struct Header<'a> {
    timestamp: &'a str,
    node: &'a str,
    level: Level,
    level_token: &'a str,
    key: &'a str,
    context: &'a str,
    context_end: usize,
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
    if node.is_empty() || !rest.starts_with('*') {
        return None;
    }
    let (level_token, after_level) = rest[1..].split_once('*')?;
    let level = Level::from_aem(level_token)?;
    let after_level = after_level.strip_prefix(' ')?;
    let context = take_balanced_brackets(after_level)?;
    let context_end = line.len() - after_level.len() + context.len();
    Some(Header {
        timestamp: ts,
        node,
        level,
        level_token,
        key: rest,
        context,
        context_end,
    })
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

fn take_line(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let i = buf.iter().position(|&b| b == b'\n')?;
    Some(buf.drain(..=i).collect())
}

fn line_content(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

fn decode_lossy(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn utf8_faults(bytes: &[u8]) -> Option<(usize, u64)> {
    let mut i = 0;
    let mut first = None;
    let mut count = 0u64;
    while i < bytes.len() {
        match std::str::from_utf8(&bytes[i..]) {
            Ok(_) => break,
            Err(err) => {
                i += err.valid_up_to();
                first.get_or_insert(i);
                count += 1;
                match err.error_len() {
                    Some(len) => i += len,
                    None => break,
                }
            }
        }
    }
    first.map(|off| (off, count))
}

fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub(super) fn bound_sample(s: &str, max: usize) -> String {
    truncate_str(s, max).to_owned()
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
                Frame::Diagnostic(_) => None,
            })
            .collect()
    }

    fn diagnostics_of(frames: &[Frame]) -> Vec<&Diagnostic> {
        frames
            .iter()
            .filter_map(|frame| match frame {
                Frame::Diagnostic(diag) => Some(diag),
                Frame::Event(_) => None,
            })
            .collect()
    }

    fn feed(chunks: &[&[u8]]) -> Vec<Frame> {
        feed_framer(Framer::new(), chunks)
    }

    fn feed_limited(max_bytes: usize, max_lines: u32, chunks: &[&[u8]]) -> Vec<Frame> {
        feed_framer(Framer::with_limits(max_bytes as u64, max_lines, 64), chunks)
    }

    fn feed_framer(mut framer: Framer, chunks: &[&[u8]]) -> Vec<Frame> {
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
        assert!(diagnostics_of(&whole).is_empty());
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
        let mut frames = framer.push(late.as_bytes(), t0 + IDLE_FLUSH);
        frames.extend(framer.finish());
        assert!(events_of(&frames).is_empty(), "{frames:?}");
        let diags = diagnostics_of(&frames);
        assert_eq!(diags.len(), 1, "{frames:?}");
        assert_eq!(diags[0].reason, DiagnosticReason::UnframedLate);
        assert_eq!(diags[0].count, late.len() as u64);
        assert_eq!(diags[0].sample, late);
        assert_eq!(diags[0].line, 3);
        assert_eq!(diags[0].offset, event.len() as u64);
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

    #[test]
    fn exact_byte_limit_keeps_full_event() {
        let event = format!("{HEADER_A}\n");
        let frames = feed_limited(event.len(), 2000, &[event.as_bytes()]);
        assert_eq!(events_of(&frames), [event.as_str()]);
        assert!(diagnostics_of(&frames).is_empty(), "{frames:?}");
        assert!(parse_event(&event).is_some());
    }

    #[test]
    fn one_byte_over_byte_limit_truncates_and_diagnoses() {
        let event = format!("{HEADER_A}\n");
        let mut input = event.clone();
        input.push('X');
        let frames = feed_limited(event.len(), 2000, &[input.as_bytes()]);
        assert_eq!(events_of(&frames), [event.as_str()]);
        let diags = diagnostics_of(&frames);
        assert_eq!(diags.len(), 1, "{frames:?}");
        assert_eq!(diags[0].reason, DiagnosticReason::EventByteLimit);
        assert_eq!(diags[0].count, 1);
        assert_eq!(diags[0].sample, "X");
        assert_eq!(diags[0].offset, event.len() as u64);
        assert!(parse_event(events_of(&frames)[0]).is_some());
    }

    #[test]
    fn exact_line_limit_keeps_full_event() {
        let event = format!("{HEADER_A}\nline2\nline3\n");
        let frames = feed_limited(1024, 3, &[event.as_bytes()]);
        assert_eq!(events_of(&frames), [event.as_str()]);
        assert!(diagnostics_of(&frames).is_empty(), "{frames:?}");
    }

    #[test]
    fn one_line_over_limit_truncates_and_aggregates_discard() {
        let kept = format!("{HEADER_A}\nline2\n");
        let extra = "line3\nline4-should-not-group\n";
        let input = format!("{kept}{extra}");
        let frames = feed_limited(1024, 2, &[input.as_bytes()]);
        assert_eq!(events_of(&frames), [kept.as_str()]);
        let diags = diagnostics_of(&frames);
        assert_eq!(diags.len(), 1, "{frames:?}");
        assert_eq!(diags[0].reason, DiagnosticReason::EventLineLimit);
        assert_eq!(diags[0].count, extra.len() as u64);
        assert_eq!(diags[0].sample, "line3\n");
        assert_eq!(diags[0].line, 3);
        assert_eq!(diags[0].offset, kept.len() as u64);
        assert_eq!(events_of(&frames).len(), 1);
    }

    #[test]
    fn huge_line_is_truncated_without_keeping_the_rest() {
        let prefix = format!("{HEADER_A}\n");
        let mut input = prefix.clone();
        input.push_str(&"x".repeat(10_000));
        let max = prefix.len() + 16;
        let frames = feed_limited(max, 2000, &[input.as_bytes()]);
        let events = events_of(&frames);
        assert_eq!(events.len(), 1, "{frames:?}");
        assert_eq!(events[0].len(), max);
        assert!(events[0].starts_with(HEADER_A));
        assert!(!events[0].contains(&"x".repeat(100)));
        let diags = diagnostics_of(&frames);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].reason, DiagnosticReason::EventByteLimit);
        assert_eq!(diags[0].count, (input.len() - max) as u64);
        assert!(diags[0].sample.len() <= 64);
    }

    #[test]
    fn huge_event_line_count_discards_until_next_header() {
        let header = format!("{HEADER_A}\n");
        let mut input = header.clone();
        for i in 0..500 {
            input.push_str(&format!("stack-{i}\n"));
        }
        input.push_str(&format!("{HEADER_B}\n"));
        let frames = feed_limited(1024 * 1024, 3, &[input.as_bytes()]);
        let events = events_of(&frames);
        assert_eq!(events.len(), 2, "{frames:?}");
        assert!(events[0].starts_with(HEADER_A));
        assert_eq!(events[0].lines().count(), 3);
        assert_eq!(events[1], format!("{HEADER_B}\n"));
        let diags = diagnostics_of(&frames);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].reason, DiagnosticReason::EventLineLimit);
        assert!(diags[0].count > 0);
    }

    #[test]
    fn garbage_prefix_emits_unframed_diagnostic() {
        let prefix = "not a log line\nmore garbage\n";
        let event = format!("{HEADER_A}\n");
        let input = format!("{prefix}{event}");
        let frames = feed_bytes(input.as_bytes());
        assert_eq!(events_of(&frames), [event.as_str()]);
        let diags = diagnostics_of(&frames);
        assert_eq!(diags.len(), 1, "{frames:?}");
        assert_eq!(diags[0].reason, DiagnosticReason::UnframedPrefix);
        assert_eq!(diags[0].count, prefix.len() as u64);
        assert_eq!(diags[0].line, 1);
        assert_eq!(diags[0].offset, 0);
        assert!(diags[0].sample.starts_with("not a log line"));
    }

    #[test]
    fn unframed_sample_is_bounded() {
        let garbage = format!("{}\n", "G".repeat(200));
        let frames = feed_framer(Framer::with_limits(256, 10, 8), &[garbage.as_bytes()]);
        assert!(events_of(&frames).is_empty());
        let diags = diagnostics_of(&frames);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].reason, DiagnosticReason::UnframedPrefix);
        assert_eq!(diags[0].count, garbage.len() as u64);
        assert!(diags[0].sample.len() <= 8);
    }

    #[test]
    fn invalid_utf8_is_replaced_and_diagnosed() {
        let mut bytes = format!("{HEADER_A} ").into_bytes();
        let fault_at = bytes.len() as u64;
        bytes.push(0xFF);
        bytes.extend_from_slice(b"bad\n");
        let frames = feed_bytes(&bytes);
        let events = events_of(&frames);
        assert_eq!(events.len(), 1, "{frames:?}");
        assert!(events[0].contains('\u{FFFD}'), "{}", events[0]);
        assert!(events[0].contains("bad"));
        assert!(parse_event(events[0]).is_some());
        let utf = diagnostics_of(&frames)
            .into_iter()
            .find(|diag| diag.reason == DiagnosticReason::InvalidUtf8)
            .expect("utf8 diagnostic");
        assert_eq!(utf.offset, fault_at);
        assert_eq!(utf.count, 1);
        assert!(utf.sample.contains('\u{FFFD}'));
        assert_eq!(utf.line, 1);
    }

    #[test]
    fn discarded_continuation_cannot_become_an_event() {
        let header_a = format!("{HEADER_A}\n");
        let header_b = format!("{HEADER_B}\n");
        let max = header_a.len() + 16;
        let mut input = header_a.clone();
        input.push_str(&"x".repeat(100));
        input.push('\n');
        input.push_str("this must not become its own group\n");
        input.push_str(&header_b);
        let frames = feed_limited(max, 10, &[input.as_bytes()]);
        let events = events_of(&frames);
        assert_eq!(events.len(), 2, "{frames:?}");
        assert!(events[0].starts_with(HEADER_A));
        assert_eq!(events[0].len(), max);
        assert_eq!(events[1], header_b);
        assert!(events
            .iter()
            .all(|event| !event.contains("must not become")));
        assert_eq!(
            diagnostics_of(&frames)
                .iter()
                .filter(|diag| diag.reason == DiagnosticReason::EventByteLimit)
                .count(),
            1
        );
    }

    #[test]
    fn recovers_after_prefix_oversize_and_utf8() {
        let prefix = b"garbage-prefix\n".as_slice();
        let mut utf8_event = format!("{HEADER_B} ").into_bytes();
        utf8_event.push(0xFF);
        utf8_event.extend_from_slice(b"ok\n");
        let recovered = b"26.08.2026 12:00:02.000 author-0 *ERROR* [FelixDispatchQueue] com.example.Baz recovered\n";
        let limit = utf8_event
            .len()
            .max(recovered.len())
            .max(HEADER_A.len() + 8);
        let mut oversized = format!("{HEADER_A}\n").into_bytes();
        oversized.extend(std::iter::repeat(b'x').take(80));
        oversized.push(b'\n');

        let frames = feed_limited(
            limit,
            8,
            &[
                prefix,
                oversized.as_slice(),
                utf8_event.as_slice(),
                recovered,
            ],
        );
        let events = events_of(&frames);
        assert_eq!(events.len(), 3, "{frames:?}");
        assert!(events[0].starts_with(HEADER_A));
        assert_eq!(events[0].len(), limit);
        assert!(events[1].starts_with(HEADER_B));
        assert!(events[1].contains('\u{FFFD}'), "{}", events[1]);
        assert!(events[2].contains("recovered"));
        for event in &events {
            assert!(parse_event(event).is_some(), "{event}");
        }

        let reasons: Vec<_> = diagnostics_of(&frames)
            .iter()
            .map(|diag| diag.reason)
            .collect();
        assert!(
            reasons.contains(&DiagnosticReason::UnframedPrefix),
            "{reasons:?}"
        );
        assert!(
            reasons.contains(&DiagnosticReason::EventByteLimit),
            "{reasons:?}"
        );
        assert!(
            reasons.contains(&DiagnosticReason::InvalidUtf8),
            "{reasons:?}"
        );
    }

    #[test]
    fn chunked_oversize_matches_whole_buffer() {
        let mut input = format!("{HEADER_A}\n").into_bytes();
        let max = input.len() + 16;
        input.extend(std::iter::repeat(b'z').take(200));
        input.extend_from_slice(b"\n");
        input.extend_from_slice(HEADER_B.as_bytes());
        input.push(b'\n');
        let whole = feed_limited(max, 20, &[&input]);
        let one = feed_limited(max, 20, &one_byte_chunks(&input));
        let arbitrary = feed_limited(max, 20, &input.chunks(9).collect::<Vec<_>>());
        assert_eq!(events_of(&one), events_of(&whole));
        assert_eq!(events_of(&arbitrary), events_of(&whole));
        assert_eq!(events_of(&whole).len(), 2);
        assert!(diagnostics_of(&whole)
            .iter()
            .any(|diag| diag.reason == DiagnosticReason::EventByteLimit));
        assert!(parse_event(events_of(&whole)[1]).is_some());
    }

    #[test]
    fn incomplete_huge_chunk_does_not_retain_over_limit() {
        let max = 64usize;
        let mut framer = Framer::with_limits(max as u64, 10, 16);
        let t0 = Instant::now();
        let chunk = vec![b'x'; 10_000];
        let mut frames = framer.push(&chunk, t0);
        frames.extend(framer.finish());
        assert!(
            events_of(&frames).iter().all(|event| event.len() <= max),
            "{frames:?}"
        );
        let diags = diagnostics_of(&frames);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].reason, DiagnosticReason::UnframedPrefix);
        assert_eq!(diags[0].count, 10_000);
        assert!(diags[0].sample.len() <= 16);
    }
    #[test]
    fn metadata_extracts_standard_header_and_offsets_without_copying_raw() {
        let event = format!("{ORDINARY}\nstack line\n");
        let meta = parse_metadata(&event).expect("metadata");
        assert_eq!(meta.timestamp, "26.08.2026 12:00:00.123");
        assert_eq!(meta.node, "author-0");
        assert_eq!(meta.level, Level::Error);
        assert_eq!(meta.thread, "FelixDispatchQueue");
        assert_eq!(meta.logger, "com.example.bundle.Activator");
        assert_eq!(meta.message, "Failed to start bundle com.example.bundle");
        assert!(meta.request_context.is_none());
        assert_eq!(
            &event[meta.offsets.timestamp.start..meta.offsets.timestamp.end],
            meta.timestamp
        );
        assert_eq!(
            &event[meta.offsets.logger.start..meta.offsets.logger.end],
            meta.logger
        );
        assert_eq!(
            &event[meta.offsets.message.start..meta.offsets.message.end],
            meta.message
        );
    }

    #[test]
    fn metadata_extracts_balanced_http_context_and_ignores_it_for_grouping() {
        let event = format!("{NESTED_HEADER}\n");
        let meta = parse_metadata(&event).expect("metadata");
        let request = meta.request_context.as_ref().expect("request context");
        assert_eq!(request.client_ip, "192.0.2.10");
        assert_eq!(request.request_id, "1724666401456");
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/content/site/us/en.html");
        assert_eq!(request.protocol, "HTTP/1.1");
        let other = "26.08.2026 12:00:02.000 node *ERROR* [10.0.0.2 [99] GET /other HTTP/1.1] com.example.core.filters.ErrorFilter Uncaught request exception\n";
        assert_eq!(
            meta.grouping_key(),
            parse_metadata(other).unwrap().grouping_key()
        );
    }

    #[test]
    fn metadata_uses_custom_and_unknown_logger_fallbacks() {
        let custom = "26.08.2026 12:00:00.123 author-0 *ERROR* [com.example.actions.ActionId] Action-ID 42 completed";
        let custom_meta = parse_metadata(custom).expect("custom metadata");
        assert_eq!(custom_meta.logger, "com.example.actions.ActionId");
        assert_eq!(custom_meta.message, "Action-ID 42 completed");

        let unknown = "26.08.2026 12:00:00.123 author-0 *ERROR* [worker-1] Action-ID 42 completed";
        let unknown_meta = parse_metadata(unknown).expect("unknown metadata");
        assert_eq!(unknown_meta.logger, "unknown");
        assert_eq!(unknown_meta.message, "Action-ID 42 completed");

        let malformed = "26.08.2026 12:00:00.123 author-0 *ERROR* [10.0.0.1 [bad] GET /x] com.example.Log message";
        assert!(parse_metadata(malformed).unwrap().request_context.is_none());
    }

    fn header(message: &str) -> String {
        format!(
            "26.08.2026 12:00:00.123 author-0 *ERROR* [FelixDispatchQueue] com.example.Foo {message}"
        )
    }

    fn identity_of(event: &str) -> (Option<&str>, Option<&str>) {
        let meta = parse_metadata(event).expect("metadata");
        (meta.terminal_exception, meta.terminal_frame)
    }

    #[test]
    fn single_outer_exception_yields_class_and_first_frame() {
        let event = format!(
            "{}\njava.lang.RuntimeException: boom\n\tat com.example.Foo.bar(Foo.java:42)\n\tat com.example.Foo.baz(Foo.java:99)\n",
            header("boom")
        );
        assert_eq!(
            identity_of(&event),
            (
                Some("java.lang.RuntimeException"),
                Some("com.example.Foo.bar")
            )
        );
    }

    #[test]
    fn last_caused_by_wins_with_its_first_frame() {
        let event = format!(
            "{}\njava.lang.RuntimeException: wrap\n\tat com.example.Foo.wrap(Foo.java:10)\nCaused by: java.lang.IllegalStateException: mid\n\tat com.example.Mid.run(Mid.java:7)\nCaused by: javax.jcr.RepositoryException: leaf\n\tat com.example.repo.Opener.open(Opener.java:17)\n\t... 12 more\n",
            header("wrap")
        );
        assert_eq!(
            identity_of(&event),
            (
                Some("javax.jcr.RepositoryException"),
                Some("com.example.repo.Opener.open")
            )
        );
    }

    #[test]
    fn later_suppressed_replaces_prior_identity() {
        let event = format!(
            "{}\njava.lang.RuntimeException: wrap\n\tat com.example.Foo.wrap(Foo.java:10)\nCaused by: java.lang.IllegalStateException: mid\n\tat com.example.Mid.run(Mid.java:7)\n\tSuppressed: java.io.IOException: temp stream closed\n\t\tat com.example.util.TempStream.close(TempStream.java:28)\n",
            header("wrap")
        );
        assert_eq!(
            identity_of(&event),
            (
                Some("java.io.IOException"),
                Some("com.example.util.TempStream.close")
            )
        );
    }

    #[test]
    fn one_line_exception_has_null_frame() {
        let event = format!(
            "{}\njavax.jcr.RepositoryException: workspace not found\n",
            header("failed")
        );
        assert_eq!(
            identity_of(&event),
            (Some("javax.jcr.RepositoryException"), None)
        );

        let header_only = header("javax.jcr.RepositoryException: workspace not found");
        assert_eq!(
            identity_of(&header_only),
            (Some("javax.jcr.RepositoryException"), None)
        );
    }

    #[test]
    fn events_without_exception_have_null_identity() {
        assert_eq!(identity_of(ORDINARY), (None, None));
        assert_eq!(identity_of(&header("Failed to start bundle")), (None, None));
    }

    #[test]
    fn frame_identity_drops_source_location() {
        let event = format!(
            "{}\njava.lang.IllegalArgumentException: bad\n\tat com.example.search.QueryParser.parse(QueryParser.java:55)\n",
            header("bad")
        );
        let (_, frame) = identity_of(&event);
        assert_eq!(frame, Some("com.example.search.QueryParser.parse"));
        assert!(!frame.unwrap().contains("QueryParser.java"));
        assert!(!frame.unwrap().contains(':'));
        assert!(!frame.unwrap().contains('('));
    }

    #[test]
    fn exception_like_prose_is_not_identity() {
        let event = format!(
            "{}\nFailed because java.lang.NullPointerException was thrown\nCaused by: missing configuration\nSee java.lang.RuntimeException for details\n\tat com.example.Foo.bar(Foo.java:42)\n",
            header("Failed because java.lang.NullPointerException was thrown")
        );
        assert_eq!(identity_of(&event), (None, None));
    }

    #[test]
    fn identity_covers_wrappers_suppressed_repeats_and_elision() {
        let causes = parse_metadata(NESTED_CAUSES).expect("nested causes");
        assert_eq!(
            causes.terminal_exception,
            Some("javax.jcr.RepositoryException")
        );
        assert_eq!(
            causes.terminal_frame,
            Some("com.example.core.repo.WorkspaceOpener.open")
        );

        let stack = parse_metadata(LONG_STACK).expect("long stack");
        assert_eq!(
            stack.terminal_exception,
            Some("java.lang.IllegalArgumentException")
        );
        assert_eq!(
            stack.terminal_frame,
            Some("com.example.core.search.QueryParser.parse")
        );

        let event = format!(
            "{}\njava.lang.RuntimeException: boom\n\tat com.example.Foo.bar(Foo.java:42)\n\tat com.example.Foo.bar(Foo.java:42)\n\t... 8 more\n",
            header("boom")
        );
        assert_eq!(
            identity_of(&event),
            (
                Some("java.lang.RuntimeException"),
                Some("com.example.Foo.bar")
            )
        );
    }
}
