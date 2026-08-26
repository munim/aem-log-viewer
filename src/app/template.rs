use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::LazyLock;

use regex::{Captures, Regex};

use super::cli::Level;
use super::tuning::{DEFAULT_BUCKET_CAP, DEFAULT_SIMILARITY, MAX_BUCKET_CAP};

pub(super) const WILDCARD: &str = "<*>";

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(super) struct BucketKey {
    pub level: Level,
    pub logger: String,
    pub terminal_exception: Option<String>,
    pub terminal_frame: Option<String>,
    pub token_count: usize,
}

impl BucketKey {
    pub(super) fn group_key(&self, index: usize) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{index}",
            self.level.as_str(),
            self.logger,
            self.terminal_exception.as_deref().unwrap_or(""),
            self.terminal_frame.as_deref().unwrap_or(""),
            self.token_count,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TemplateMerge {
    pub survivor: usize,
    pub removed: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum LearnOutcome {
    Matched {
        bucket: BucketKey,
        index: usize,
        merges: Vec<TemplateMerge>,
    },
    Created {
        bucket: BucketKey,
        index: usize,
    },
    Capacity {
        bucket: BucketKey,
    },
    Rejected {
        bucket: BucketKey,
    },
}

impl LearnOutcome {
    pub(super) fn group_key(&self) -> Option<String> {
        match self {
            Self::Capacity { .. } | Self::Rejected { .. } => None,
            Self::Matched { bucket, index, .. } | Self::Created { bucket, index } => {
                Some(bucket.group_key(*index))
            }
        }
    }

    pub(super) fn merges(&self) -> &[TemplateMerge] {
        match self {
            Self::Matched { merges, .. } => merges,
            Self::Created { .. } | Self::Capacity { .. } | Self::Rejected { .. } => &[],
        }
    }

    pub(super) fn index(&self) -> Option<usize> {
        match self {
            Self::Matched { index, .. } | Self::Created { index, .. } => Some(*index),
            Self::Capacity { .. } | Self::Rejected { .. } => None,
        }
    }

    pub(super) fn bucket(&self) -> Option<&BucketKey> {
        match self {
            Self::Matched { bucket, .. }
            | Self::Created { bucket, .. }
            | Self::Capacity { bucket }
            | Self::Rejected { bucket } => Some(bucket),
        }
    }
}

/// Bounded online learner. Templates only generalize; buckets never exceed cap.
pub(super) struct TemplateStore {
    similarity: f64,
    bucket_cap: usize,
    buckets: HashMap<BucketKey, Vec<Option<Vec<String>>>>,
}

impl Default for TemplateStore {
    fn default() -> Self {
        Self::new(DEFAULT_SIMILARITY, DEFAULT_BUCKET_CAP)
    }
}

impl TemplateStore {
    pub(super) fn new(similarity: f64, bucket_cap: u32) -> Self {
        Self {
            similarity,
            bucket_cap: (bucket_cap as usize).clamp(1, MAX_BUCKET_CAP as usize),
            buckets: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn learn(
        &mut self,
        level: Level,
        logger: &str,
        terminal_exception: Option<&str>,
        terminal_frame: Option<&str>,
        message: &str,
    ) -> LearnOutcome {
        self.learn_allowing(
            level,
            logger,
            terminal_exception,
            terminal_frame,
            message,
            true,
        )
    }

    pub(super) fn learn_allowing(
        &mut self,
        level: Level,
        logger: &str,
        terminal_exception: Option<&str>,
        terminal_frame: Option<&str>,
        message: &str,
        allow_create: bool,
    ) -> LearnOutcome {
        let tokens = normalize(message);
        let bucket = BucketKey {
            level,
            logger: logger.to_owned(),
            terminal_exception: terminal_exception.map(str::to_owned),
            terminal_frame: terminal_frame.map(str::to_owned),
            token_count: tokens.len(),
        };
        self.learn_tokens(bucket, tokens, allow_create)
    }

    fn learn_tokens(
        &mut self,
        bucket: BucketKey,
        tokens: Vec<String>,
        allow_create: bool,
    ) -> LearnOutcome {
        if let Some(best) = self.best_candidate(&bucket, &tokens) {
            let templates = self.buckets.get_mut(&bucket).expect("matched bucket");
            let slot = templates[best].as_mut().expect("live matched template");
            generalize(slot, &tokens);
            let merges = self.collapse(&bucket);
            let index = resolve_survivor(best, &merges);
            return LearnOutcome::Matched {
                bucket,
                index,
                merges,
            };
        }
        let live = self.live_len(&bucket);
        if live >= self.bucket_cap {
            return LearnOutcome::Capacity { bucket };
        }
        if !allow_create {
            return LearnOutcome::Rejected { bucket };
        }
        let templates = self.buckets.entry(bucket.clone()).or_default();
        let index = templates.len();
        templates.push(Some(tokens));
        LearnOutcome::Created { bucket, index }
    }

    fn best_candidate(&self, bucket: &BucketKey, tokens: &[String]) -> Option<usize> {
        let templates = self.buckets.get(bucket)?;
        let mut best: Option<(usize, f64)> = None;
        for (index, candidate) in templates.iter().enumerate() {
            let Some(candidate) = candidate else {
                continue;
            };
            let score = similarity(candidate, tokens);
            if score < self.similarity {
                continue;
            }
            match best {
                Some((_, best_score)) if score <= best_score => {}
                _ => best = Some((index, score)),
            }
        }
        best.map(|(index, _)| index)
    }

    fn collapse(&mut self, bucket: &BucketKey) -> Vec<TemplateMerge> {
        let Some(templates) = self.buckets.get_mut(bucket) else {
            return Vec::new();
        };
        let mut merges = Vec::new();
        loop {
            let pair = next_compatible_pair(templates, self.similarity);
            let Some((survivor, removed)) = pair else {
                break;
            };
            let taken = templates[removed].take().expect("live removed template");
            generalize(templates[survivor].as_mut().expect("live survivor"), &taken);
            merges.push(TemplateMerge { survivor, removed });
        }
        merges
    }

    fn live_len(&self, bucket: &BucketKey) -> usize {
        self.buckets.get(bucket).map_or(0, |templates| {
            templates.iter().filter(|slot| slot.is_some()).count()
        })
    }

    pub(super) fn template(&self, bucket: &BucketKey, index: usize) -> Option<&[String]> {
        self.buckets
            .get(bucket)
            .and_then(|templates| templates.get(index))
            .and_then(Option::as_ref)
            .map(Vec::as_slice)
    }

    #[cfg(test)]
    fn bucket_len(&self, bucket: &BucketKey) -> usize {
        self.live_len(bucket)
    }
}

fn next_compatible_pair(
    templates: &[Option<Vec<String>>],
    threshold: f64,
) -> Option<(usize, usize)> {
    let live: Vec<(usize, &[String])> = templates
        .iter()
        .enumerate()
        .filter_map(|(index, slot)| slot.as_ref().map(|tokens| (index, tokens.as_slice())))
        .collect();
    for (offset, &(left, a)) in live.iter().enumerate() {
        for &(right, b) in live.iter().skip(offset + 1) {
            if similarity(a, b) >= threshold && similarity(b, a) >= threshold {
                return Some((left, right));
            }
        }
    }
    None
}

fn resolve_survivor(mut index: usize, merges: &[TemplateMerge]) -> usize {
    for merge in merges {
        if merge.removed == index {
            index = merge.survivor;
        }
    }
    index
}

/// Whitespace tokens with conservative scalar wildcards. Never copies the event.
pub(super) fn normalize(message: &str) -> Vec<String> {
    message
        .split_whitespace()
        .map(|token| normalize_token(token).into_owned())
        .collect()
}

fn normalize_token(token: &str) -> Cow<'_, str> {
    if token == WILDCARD {
        return Cow::Borrowed(WILDCARD);
    }
    if is_uuid(token) || is_ip(token) || is_timestamp(token) || is_duration(token) {
        return Cow::Borrowed(WILDCARD);
    }
    if let Some(replaced) = object_identity(token) {
        return Cow::Owned(replaced);
    }
    if let Some(replaced) = query_values(token) {
        return Cow::Owned(replaced);
    }
    if is_long_hex(token) || is_long_integer(token) {
        return Cow::Borrowed(WILDCARD);
    }
    Cow::Borrowed(token)
}

fn is_uuid(token: &str) -> bool {
    let token = token.trim_matches(|c| c == '{' || c == '}');
    uuid::Uuid::parse_str(token).is_ok()
}

fn is_ip(token: &str) -> bool {
    let unbracketed = token
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(token);
    if unbracketed.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    match token.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            let host = host
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
                .unwrap_or(host);
            host.parse::<std::net::IpAddr>().is_ok()
        }
        _ => false,
    }
}

fn is_timestamp(token: &str) -> bool {
    TIMESTAMP_RE.is_match(token)
}

fn is_duration(token: &str) -> bool {
    DURATION_RE.is_match(token)
}

fn is_long_hex(token: &str) -> bool {
    token.len() >= 8 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_long_integer(token: &str) -> bool {
    token.len() >= 4 && token.bytes().all(|b| b.is_ascii_digit())
}

fn object_identity(token: &str) -> Option<String> {
    let (prefix, suffix) = token.rsplit_once('@')?;
    if prefix.is_empty() {
        return None;
    }
    if suffix.len() >= 4 && suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(format!("{prefix}@{WILDCARD}"))
    } else {
        None
    }
}

fn query_values(token: &str) -> Option<String> {
    if !token.contains('=') || !(token.contains('?') || token.contains('&')) {
        return None;
    }
    match QUERY_RE.replace_all(token, |caps: &Captures| format!("{}{WILDCARD}", &caps[1])) {
        Cow::Owned(replaced) => Some(replaced),
        Cow::Borrowed(_) => None,
    }
}

fn similarity(template: &[String], event: &[String]) -> f64 {
    let mut compared = 0usize;
    let mut matched = 0usize;
    for (candidate, token) in template.iter().zip(event) {
        if candidate == WILDCARD {
            continue;
        }
        compared += 1;
        if candidate == token {
            matched += 1;
        }
    }
    if compared == 0 {
        1.0
    } else {
        matched as f64 / compared as f64
    }
}

fn generalize(template: &mut [String], event: &[String]) {
    for (candidate, token) in template.iter_mut().zip(event) {
        if candidate == WILDCARD {
            continue;
        }
        if candidate != token {
            candidate.clear();
            candidate.push_str(WILDCARD);
        }
    }
}

static TIMESTAMP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^(",
        r"\d{2}\.\d{2}\.\d{4}",
        r"|",
        r"\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})?)?",
        r"|",
        r"\d{2}:\d{2}:\d{2}(?:\.\d+)?",
        r")$"
    ))
    .expect("timestamp regex")
});

static DURATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\d+(?:\.\d+)?(?:ns|us|µs|μs|ms|s|m|h|d)$").expect("duration regex")
});

static QUERY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([?&][^=&#\s]+=)([^&#\s]*)").expect("query regex"));

#[cfg(test)]
mod tests {
    use super::*;

    fn learn(
        store: &mut TemplateStore,
        logger: &str,
        exception: Option<&str>,
        frame: Option<&str>,
        message: &str,
    ) -> LearnOutcome {
        store.learn(Level::Error, logger, exception, frame, message)
    }

    fn learn_msg(store: &mut TemplateStore, message: &str) -> LearnOutcome {
        learn(store, "com.example.Foo", None, None, message)
    }

    fn bucket(outcome: &LearnOutcome) -> &BucketKey {
        match outcome {
            LearnOutcome::Matched { bucket, .. }
            | LearnOutcome::Created { bucket, .. }
            | LearnOutcome::Capacity { bucket }
            | LearnOutcome::Rejected { bucket } => bucket,
        }
    }

    fn index_of(outcome: &LearnOutcome) -> Option<usize> {
        match outcome {
            LearnOutcome::Matched { index, .. } | LearnOutcome::Created { index, .. } => {
                Some(*index)
            }
            LearnOutcome::Capacity { .. } | LearnOutcome::Rejected { .. } => None,
        }
    }

    fn tokens(message: &str) -> Vec<String> {
        normalize(message)
    }

    #[test]
    fn scalars_normalize_without_copying_paths_or_short_status() {
        assert_eq!(
            tokens("id 550e8400-e29b-41d4-a716-446655440000 done"),
            vec!["id", WILDCARD, "done"]
        );
        assert_eq!(
            tokens("from 192.0.2.1 via 2001:db8::1"),
            vec!["from", WILDCARD, "via", WILDCARD]
        );
        assert_eq!(
            tokens("at 26.08.2026 12:00:00.123 or 2026-08-26T12:00:00.123Z"),
            vec!["at", WILDCARD, WILDCARD, "or", WILDCARD]
        );
        assert_eq!(
            tokens("took 1500ms then 2s"),
            vec!["took", WILDCARD, "then", WILDCARD]
        );
        assert_eq!(
            tokens("GET /content/site/us/en.html?foo=bar&x=1"),
            vec!["GET", "/content/site/us/en.html?foo=<*>&x=<*>"]
        );
        assert_eq!(
            tokens("obj com.example.Foo@6d06d69c"),
            vec!["obj", "com.example.Foo@<*>"]
        );
        assert_eq!(tokens("hash deadbeefcafe"), vec!["hash", WILDCARD]);
        assert_eq!(tokens("n 1234 ok 42"), vec!["n", WILDCARD, "ok", "42"]);
        assert_eq!(
            tokens("status 200 path /content/site/us/en.html"),
            vec!["status", "200", "path", "/content/site/us/en.html"]
        );
    }

    #[test]
    fn normalization_is_idempotent() {
        let inputs = [
            "id 550e8400-e29b-41d4-a716-446655440000 {6ba7b810-9dad-11d1-80b4-00c04fd430c8}",
            "from 192.0.2.1 to [::1]: still 10.0.0.1:8080",
            "at 26.08.2026 12:00:00.123 2026-08-26 2026-08-26T12:00:00.123+02:00",
            "took 1500ms 1.5s 10m 2h 30ns 12us",
            "GET /content/site/us/en.html?foo=bar&x=1 leftover",
            "obj com.example.Foo@6d06d69c Foo@<*>",
            "hash deadbeefcafe ABCDEF0123456789",
            "n 1234 42 7 1000 999",
            "status 200 path /content/site/us/en.html",
            "plain words only",
            "",
            WILDCARD,
        ];
        for input in inputs {
            let once = normalize(input);
            let twice = normalize(&once.join(" "));
            assert_eq!(once, twice, "{input}");
        }
        for n in 1000..1120 {
            let input = format!("count {n} status 200");
            let once = normalize(&input);
            let twice = normalize(&once.join(" "));
            assert_eq!(once, twice, "{input}");
        }
        for octet in 0..40 {
            let input = format!("peer 192.0.2.{octet}");
            let once = normalize(&input);
            let twice = normalize(&once.join(" "));
            assert_eq!(once, twice, "{input}");
        }
    }

    #[test]
    fn insertion_is_deterministic_for_the_same_sequence() {
        let messages = [
            "alpha bravo charlie delta echo",
            "alpha bravo charlie delta foxtrot",
            "alpha other charlie delta echo",
            "alpha bravo charlie delta golf",
            "unique zulu message here now",
        ];
        let mut first = TemplateStore::default();
        let mut second = TemplateStore::default();
        let mut first_keys = Vec::new();
        let mut second_keys = Vec::new();
        for message in messages {
            first_keys.push(learn_msg(&mut first, message));
            second_keys.push(learn_msg(&mut second, message));
        }
        assert_eq!(first_keys, second_keys);
        let bucket = bucket(&first_keys[0]);
        assert_eq!(first.template(bucket, 0), second.template(bucket, 0));
        assert_eq!(first.template(bucket, 1), second.template(bucket, 1));
    }

    #[test]
    fn best_candidate_tie_uses_earliest_insertion() {
        let mut store = TemplateStore::default();
        let a = learn_msg(&mut store, "a b c d e");
        let b = learn_msg(&mut store, "a b x y z");
        assert!(matches!(a, LearnOutcome::Created { index: 0, .. }));
        assert!(matches!(b, LearnOutcome::Created { index: 1, .. }));
        let hit = learn_msg(&mut store, "a b q y e");
        assert_eq!(index_of(&hit), Some(0));
        let bucket = bucket(&hit);
        assert_eq!(
            store.template(bucket, 0).unwrap(),
            ["a", "b", WILDCARD, WILDCARD, "e"]
        );
        assert_eq!(
            store.template(bucket, 1).unwrap(),
            ["a", "b", "x", "y", "z"]
        );
    }

    #[test]
    fn similarity_ignores_existing_wildcards_and_uses_default_threshold() {
        let mut store = TemplateStore::default();
        assert!(matches!(
            learn_msg(&mut store, "a b c d e"),
            LearnOutcome::Created { index: 0, .. }
        ));
        assert!(matches!(
            learn_msg(&mut store, "a x c d e"),
            LearnOutcome::Matched { index: 0, merges, .. } if merges.is_empty()
        ));
        assert!(matches!(
            learn_msg(&mut store, "a y c d e"),
            LearnOutcome::Matched { index: 0, merges, .. } if merges.is_empty()
        ));
        assert!(matches!(
            learn_msg(&mut store, "a y q d z"),
            LearnOutcome::Created { index: 1, .. }
        ));
        assert!(matches!(
            learn_msg(&mut store, "p q r s t"),
            LearnOutcome::Created { .. }
        ));
        assert!(matches!(
            learn_msg(&mut store, "p q r u v"),
            LearnOutcome::Matched { .. }
        ));
    }

    #[test]
    fn default_similarity_accepts_three_of_five() {
        let mut store = TemplateStore::default();
        let created = learn_msg(&mut store, "one two three four five");
        let hit = learn_msg(&mut store, "one two three x y");
        let miss = learn_msg(&mut store, "one x a b c");
        assert_eq!(index_of(&created), Some(0));
        assert_eq!(index_of(&hit), Some(0));
        assert!(matches!(miss, LearnOutcome::Created { index: 1, .. }));
    }

    #[test]
    fn generalization_is_monotonic() {
        let mut store = TemplateStore::default();
        let first = learn_msg(&mut store, "a b c d e");
        let bucket = bucket(&first).clone();
        let snapshots = ["a b c d x", "a b c y x", "a b z y x", "a b z y x"];
        let mut wildcards = 0usize;
        for message in snapshots {
            let outcome = learn_msg(&mut store, message);
            assert_eq!(index_of(&outcome), Some(0));
            let template = store.template(&bucket, 0).unwrap();
            let next = template.iter().filter(|token| *token == WILDCARD).count();
            assert!(next >= wildcards, "{template:?}");
            for (index, token) in template.iter().enumerate() {
                if token != WILDCARD {
                    let expected = ["a", "b", "c", "d", "e"][index];
                    assert_eq!(token, expected);
                }
            }
            wildcards = next;
        }
        assert_eq!(
            store.template(&bucket, 0).unwrap(),
            ["a", "b", WILDCARD, WILDCARD, WILDCARD]
        );
    }

    const MERGE_A: &str = "alpha beta gamma delta epsilon";
    const MERGE_B: &str = "alpha other unique novel epsilon";

    fn replace_token(message: &str, position: usize, replacement: &str) -> String {
        let mut tokens: Vec<&str> = message.split_whitespace().collect();
        tokens[position] = replacement;
        tokens.join(" ")
    }

    fn learn_replaced(
        store: &mut TemplateStore,
        original: &str,
        position: usize,
        replacement: &str,
    ) -> LearnOutcome {
        learn_msg(store, &replace_token(original, position, replacement))
    }

    fn drive_bidirectional_merge(store: &mut TemplateStore) -> LearnOutcome {
        assert!(matches!(
            learn_msg(store, MERGE_A),
            LearnOutcome::Created { index: 0, .. }
        ));
        assert!(matches!(
            learn_msg(store, MERGE_B),
            LearnOutcome::Created { index: 1, .. }
        ));
        assert!(learn_replaced(store, MERGE_A, 1, "BETA")
            .merges()
            .is_empty());
        assert!(learn_replaced(store, MERGE_A, 2, "GAMMA")
            .merges()
            .is_empty());
        assert!(learn_replaced(store, MERGE_B, 1, "OTHER")
            .merges()
            .is_empty());
        learn_replaced(store, MERGE_B, 2, "UNIQUE")
    }

    #[test]
    fn compatible_templates_merge_oldest_index_and_never_specialize() {
        let mut store = TemplateStore::default();
        let merged = drive_bidirectional_merge(&mut store);
        assert_eq!(index_of(&merged), Some(0));
        assert_eq!(
            merged.merges(),
            [TemplateMerge {
                survivor: 0,
                removed: 1
            }]
        );
        assert_eq!(store.bucket_len(bucket(&merged)), 1);
        assert_eq!(
            store.template(bucket(&merged), 0).unwrap(),
            ["alpha", WILDCARD, WILDCARD, WILDCARD, "epsilon"]
        );
        assert!(store.template(bucket(&merged), 1).is_none());
        let again = learn_msg(&mut store, MERGE_B);
        assert_eq!(index_of(&again), Some(0));
        assert!(again.merges().is_empty());
        assert_eq!(
            store.template(bucket(&again), 0).unwrap(),
            ["alpha", WILDCARD, WILDCARD, WILDCARD, "epsilon"]
        );
    }

    #[test]
    fn incompatible_fixed_tokens_do_not_merge_or_specialize() {
        let mut store = TemplateStore::default();
        learn_msg(&mut store, MERGE_A);
        learn_msg(&mut store, MERGE_B);
        let hit = learn_replaced(&mut store, MERGE_A, 1, "BETA");
        assert_eq!(index_of(&hit), Some(0));
        assert!(hit.merges().is_empty());
        assert_eq!(store.bucket_len(bucket(&hit)), 2);
        assert_eq!(
            store.template(bucket(&hit), 0).unwrap(),
            ["alpha", WILDCARD, "gamma", "delta", "epsilon"]
        );
        assert_eq!(
            store.template(bucket(&hit), 1).unwrap(),
            ["alpha", "other", "unique", "novel", "epsilon"]
        );
        let still = learn_replaced(&mut store, MERGE_B, 1, "OTHER");
        assert!(still.merges().is_empty());
        assert_eq!(store.bucket_len(bucket(&still)), 2);
    }

    #[test]
    fn removed_template_index_stays_dead_and_capacity_counts_live_only() {
        let mut store = TemplateStore::new(0.60, 2);
        let merged = drive_bidirectional_merge(&mut store);
        assert_eq!(store.bucket_len(bucket(&merged)), 1);
        let fresh = learn_msg(&mut store, "zeta eta theta iota kappa");
        assert!(matches!(fresh, LearnOutcome::Created { index: 2, .. }));
        assert_eq!(store.bucket_len(bucket(&fresh)), 2);
        let overflow = learn_msg(&mut store, "lambda mu nu xi omicron");
        assert!(matches!(overflow, LearnOutcome::Capacity { .. }));
    }

    #[test]
    fn bucket_growth_is_bounded_by_cap_and_ceiling() {
        let mut store = TemplateStore::new(1.0, 5);
        let mut created = 0;
        let mut capacity = 0;
        for i in 0..40 {
            match learn_msg(&mut store, &format!("unique-token-{i}")) {
                LearnOutcome::Created { .. } => created += 1,
                LearnOutcome::Capacity { .. } => capacity += 1,
                LearnOutcome::Matched { .. } => panic!("exact similarity should not match"),
                LearnOutcome::Rejected { .. } => panic!("create was allowed"),
            }
        }
        assert_eq!(created, 5);
        assert_eq!(capacity, 35);
        let first = learn_msg(&mut store, "unique-token-0");
        assert_eq!(store.bucket_len(bucket(&first)), 5);

        let mut ceiling = TemplateStore::new(1.0, 50_000);
        for i in 0..MAX_BUCKET_CAP + 25 {
            ceiling.learn(
                Level::Error,
                "com.example.Foo",
                None,
                None,
                &format!("ceiling-{i}"),
            );
        }
        let probe = ceiling.learn(Level::Error, "com.example.Foo", None, None, "ceiling-0");
        assert_eq!(ceiling.bucket_len(bucket(&probe)), MAX_BUCKET_CAP as usize);
        assert!(matches!(
            ceiling.learn(
                Level::Error,
                "com.example.Foo",
                None,
                None,
                "fresh-unmatched"
            ),
            LearnOutcome::Capacity { .. }
        ));
    }

    #[test]
    fn structural_identity_keeps_logger_exception_and_frame_separate() {
        let mut store = TemplateStore::default();
        let message = "Resource not found /content/site/us/en.html";
        let a = learn(&mut store, "com.example.Foo", None, None, message);
        let b = learn(&mut store, "com.example.Bar", None, None, message);
        let c = learn(
            &mut store,
            "com.example.Foo",
            Some("java.lang.RuntimeException"),
            Some("com.example.Foo.bar"),
            message,
        );
        let d = learn(
            &mut store,
            "com.example.Foo",
            Some("java.lang.RuntimeException"),
            Some("com.example.Foo.baz"),
            message,
        );
        assert!(matches!(a, LearnOutcome::Created { index: 0, .. }));
        assert!(matches!(b, LearnOutcome::Created { index: 0, .. }));
        assert!(matches!(c, LearnOutcome::Created { index: 0, .. }));
        assert!(matches!(d, LearnOutcome::Created { index: 0, .. }));
        assert_ne!(bucket(&a), bucket(&b));
        assert_ne!(bucket(&a), bucket(&c));
        assert_ne!(bucket(&c), bucket(&d));
        assert_eq!(bucket(&a).token_count, 4);
    }

    #[test]
    fn anonymized_paths_and_package_versions_group() {
        let mut store = TemplateStore::default();
        let a = learn_msg(&mut store, "Resource not found /content/site/us/en.html");
        let b = learn_msg(&mut store, "Resource not found /content/site/de/de.html");
        assert_eq!(index_of(&a), Some(0));
        assert_eq!(index_of(&b), Some(0));
        assert_eq!(
            store.template(bucket(&a), 0).unwrap(),
            ["Resource", "not", "found", WILDCARD]
        );

        let mut versions = TemplateStore::default();
        let v1 = learn_msg(
            &mut versions,
            "Failed to start bundle com.example.core 1.2.3",
        );
        let v2 = learn_msg(
            &mut versions,
            "Failed to start bundle com.example.core 1.2.4",
        );
        assert_eq!(index_of(&v1), Some(0));
        assert_eq!(index_of(&v2), Some(0));
        assert_eq!(
            versions.template(bucket(&v1), 0).unwrap(),
            [
                "Failed",
                "to",
                "start",
                "bundle",
                "com.example.core",
                WILDCARD
            ]
        );
    }

    #[test]
    fn exact_token_count_is_part_of_the_bucket() {
        let mut store = TemplateStore::default();
        let short = learn_msg(&mut store, "failed now");
        let long = learn_msg(&mut store, "failed now extra");
        assert_ne!(bucket(&short).token_count, bucket(&long).token_count);
        assert!(matches!(short, LearnOutcome::Created { .. }));
        assert!(matches!(long, LearnOutcome::Created { .. }));
    }
}
