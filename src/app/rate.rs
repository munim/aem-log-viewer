use chrono::{DateTime, Duration, Utc};

use super::tuning::Tuning;

/// EWMA half-lives and New/Increasing thresholds. Units: seconds and events/second.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct RateParams {
    pub fast_half_life_secs: f64,
    pub baseline_half_life_secs: f64,
    pub new_age: Duration,
    pub increasing_min_age: Duration,
    pub increasing_ratio: f64,
    pub increasing_min_rate: f64,
}

impl RateParams {
    pub(super) fn from_tuning(tuning: &Tuning) -> Self {
        Self {
            fast_half_life_secs: f64::from(tuning.fast_half_life_secs),
            baseline_half_life_secs: f64::from(tuning.baseline_half_life_secs),
            new_age: Duration::seconds(i64::from(tuning.new_age_secs)),
            increasing_min_age: Duration::seconds(i64::from(tuning.increasing_min_age_secs)),
            increasing_ratio: tuning.increasing_ratio,
            increasing_min_rate: tuning.increasing_min_rate,
        }
    }
}

impl Default for RateParams {
    fn default() -> Self {
        Self::from_tuning(&Tuning::default())
    }
}

/// Constant-memory fast and baseline EWMAs in events/second.
#[derive(Clone, Copy, Debug)]
pub(super) struct RateState {
    fast: f64,
    baseline: f64,
    updated_at: DateTime<Utc>,
}

impl RateState {
    pub(super) fn first(at: DateTime<Utc>, params: &RateParams) -> Self {
        let mut state = Self {
            fast: 0.0,
            baseline: 0.0,
            updated_at: at,
        };
        state.observe(at, params);
        state
    }

    pub(super) fn observe(&mut self, at: DateTime<Utc>, params: &RateParams) {
        let at = at.max(self.updated_at);
        self.decay_to(at, params);
        self.fast += impulse(params.fast_half_life_secs);
        self.baseline += impulse(params.baseline_half_life_secs);
    }

    pub(super) fn decay_to(&mut self, at: DateTime<Utc>, params: &RateParams) {
        let at = at.max(self.updated_at);
        let dt = secs_since(self.updated_at, at);
        self.fast *= decay_factor(dt, params.fast_half_life_secs);
        self.baseline *= decay_factor(dt, params.baseline_half_life_secs);
        self.updated_at = at;
    }

    pub(super) fn merge(mut self, mut other: Self, params: &RateParams) -> Self {
        let common = self.updated_at.max(other.updated_at);
        self.decay_to(common, params);
        other.decay_to(common, params);
        Self {
            fast: self.fast + other.fast,
            baseline: self.baseline + other.baseline,
            updated_at: common,
        }
    }

    pub(super) fn rates_at(&self, at: DateTime<Utc>, params: &RateParams) -> (f64, f64) {
        let mut copy = *self;
        copy.decay_to(at.max(copy.updated_at), params);
        (copy.fast, copy.baseline)
    }

    #[allow(dead_code)]
    pub(super) fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }
}

/// Snapshot used for New/Increasing/Muted membership and view order.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub(super) struct RateSnapshot {
    pub id: u64,
    pub count: u64,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub muted: bool,
    pub fast: f64,
    pub baseline: f64,
}

impl RateSnapshot {
    pub(super) fn is_new(&self, now: DateTime<Utc>, params: &RateParams) -> bool {
        age(self.first_seen, now) <= params.new_age
    }

    pub(super) fn is_increasing(&self, now: DateTime<Utc>, params: &RateParams) -> bool {
        age(self.first_seen, now) >= params.increasing_min_age
            && self.fast >= params.increasing_ratio * self.baseline
            && self.fast >= params.increasing_min_rate
    }

    fn ratio(&self) -> f64 {
        if self.baseline > 0.0 {
            self.fast / self.baseline
        } else if self.fast > 0.0 {
            f64::INFINITY
        } else {
            0.0
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(super) enum View {
    Volume,
    New,
    Increasing,
    Muted,
}

#[allow(dead_code)]
pub(super) fn rank(
    view: View,
    groups: &[RateSnapshot],
    now: DateTime<Utc>,
    params: &RateParams,
) -> Vec<RateSnapshot> {
    let mut out: Vec<RateSnapshot> = groups
        .iter()
        .copied()
        .filter(|group| match view {
            View::Volume => true,
            View::New => group.is_new(now, params),
            View::Increasing => group.is_increasing(now, params),
            View::Muted => group.muted,
        })
        .collect();
    out.sort_by(|a, b| match view {
        View::Volume | View::Muted => b.count.cmp(&a.count).then(a.id.cmp(&b.id)),
        View::New => b.first_seen.cmp(&a.first_seen).then(a.id.cmp(&b.id)),
        View::Increasing => b
            .ratio()
            .total_cmp(&a.ratio())
            .then(b.fast.total_cmp(&a.fast))
            .then(a.id.cmp(&b.id)),
    });
    out
}

fn impulse(half_life_secs: f64) -> f64 {
    std::f64::consts::LN_2 / half_life_secs
}

fn decay_factor(dt_secs: f64, half_life_secs: f64) -> f64 {
    if dt_secs <= 0.0 {
        1.0
    } else {
        2_f64.powf(-dt_secs / half_life_secs)
    }
}

fn secs_since(from: DateTime<Utc>, to: DateTime<Utc>) -> f64 {
    to.signed_duration_since(from)
        .to_std()
        .map(|span| span.as_secs_f64())
        .unwrap_or(0.0)
}

fn age(first_seen: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    now.signed_duration_since(first_seen).max(Duration::zero())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(ss: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, ss).unwrap()
    }

    fn params() -> RateParams {
        RateParams::default()
    }

    fn close(got: f64, expected: f64) {
        assert!(
            (got - expected).abs() <= 1e-12,
            "got {got} expected {expected}"
        );
    }

    fn snap(
        id: u64,
        count: u64,
        first: DateTime<Utc>,
        muted: bool,
        fast: f64,
        baseline: f64,
    ) -> RateSnapshot {
        RateSnapshot {
            id,
            count,
            first_seen: first,
            last_seen: first,
            muted,
            fast,
            baseline,
        }
    }

    #[test]
    fn first_event_is_half_life_impulse_in_events_per_second() {
        let state = RateState::first(utc(0), &params());
        close(state.fast, std::f64::consts::LN_2 / 10.0);
        close(state.baseline, std::f64::consts::LN_2 / 300.0);
        assert_eq!(state.updated_at(), utc(0));
    }

    #[test]
    fn one_fast_half_life_of_silence_halves_fast_rate() {
        let mut state = RateState::first(utc(0), &params());
        let start_fast = state.fast;
        let start_base = state.baseline;
        let (fast, baseline) = state.rates_at(utc(10), &params());
        close(fast, start_fast * 0.5);
        close(baseline, start_base * 2_f64.powf(-10.0 / 300.0));
        state.observe(utc(10), &params());
        close(state.fast, start_fast * 0.5 + std::f64::consts::LN_2 / 10.0);
    }

    #[test]
    fn burst_raises_fast_faster_than_baseline() {
        let mut state = RateState::first(utc(0), &params());
        for _ in 1..20 {
            state.observe(utc(0), &params());
        }
        assert!(state.fast > state.baseline);
        assert!(state.fast > 1.0);
        assert!(state.baseline < 0.05);
    }

    #[test]
    fn long_silence_decays_rates_without_periodic_work() {
        let state = RateState::first(utc(0), &params());
        let later = Utc.with_ymd_and_hms(2026, 8, 26, 13, 0, 0).unwrap();
        let (fast, baseline) = state.rates_at(later, &params());
        assert!(fast < 1e-12);
        assert!(baseline < 1e-6);
        assert_eq!(state.updated_at(), utc(0));
    }

    #[test]
    fn sparse_events_leave_low_fast_rate() {
        let mut state = RateState::first(utc(0), &params());
        state.observe(utc(30), &params());
        state.observe(
            Utc.with_ymd_and_hms(2026, 8, 26, 12, 1, 0).unwrap(),
            &params(),
        );
        assert!(state.fast < 0.1);
        assert!(state.fast > 0.0);
    }

    #[test]
    fn out_of_order_source_time_does_not_regress_or_go_negative() {
        let mut state = RateState::first(utc(10), &params());
        let before = state.fast;
        state.observe(utc(5), &params());
        assert_eq!(state.updated_at(), utc(10));
        close(state.fast, before + std::f64::consts::LN_2 / 10.0);
        assert!(state.fast > 0.0);
        assert!(state.baseline > 0.0);
    }

    #[test]
    fn merge_decays_both_sides_to_common_instant_then_sums() {
        let params = params();
        let earlier = RateState::first(utc(0), &params);
        let later = RateState::first(utc(10), &params);
        let merged = earlier.merge(later, &params);
        assert_eq!(merged.updated_at(), utc(10));
        let expected_fast = (std::f64::consts::LN_2 / 10.0) * 0.5 + std::f64::consts::LN_2 / 10.0;
        let expected_base = (std::f64::consts::LN_2 / 300.0) * 2_f64.powf(-10.0 / 300.0)
            + std::f64::consts::LN_2 / 300.0;
        close(merged.fast, expected_fast);
        close(merged.baseline, expected_base);
    }

    #[test]
    fn new_includes_threshold_age_and_excludes_older() {
        let params = params();
        let first = utc(0);
        let at_bound = Utc.with_ymd_and_hms(2026, 8, 26, 12, 1, 0).unwrap();
        let older = Utc.with_ymd_and_hms(2026, 8, 26, 12, 1, 1).unwrap();
        let group = snap(1, 1, first, false, 0.0, 0.0);
        assert!(group.is_new(at_bound, &params));
        assert!(!group.is_new(older, &params));
    }

    #[test]
    fn increasing_uses_inclusive_age_ratio_and_min_rate() {
        let params = params();
        let first = utc(0);
        let ready = Utc.with_ymd_and_hms(2026, 8, 26, 12, 1, 0).unwrap();
        let young = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 59).unwrap();
        let at_threshold = snap(1, 10, first, false, 5.0, 2.5);
        assert!(at_threshold.is_increasing(ready, &params));
        assert!(!at_threshold.is_increasing(young, &params));
        assert!(!snap(2, 10, first, false, 4.99, 0.1).is_increasing(ready, &params));
        assert!(!snap(3, 10, first, false, 5.0, 2.51).is_increasing(ready, &params));
        assert!(snap(4, 10, first, false, 5.0, 2.5).is_increasing(ready, &params));
    }

    #[test]
    fn muted_membership_is_the_mute_flag() {
        let now = utc(0);
        let params = params();
        let quiet = snap(1, 9, now, true, 0.0, 0.0);
        let loud = snap(2, 9, now, false, 9.0, 1.0);
        assert_eq!(rank(View::Muted, &[quiet, loud], now, &params)[0].id, 1);
        assert!(rank(View::Muted, &[loud], now, &params).is_empty());
    }

    #[test]
    fn volume_and_muted_order_by_count_then_stable_id() {
        let now = utc(0);
        let params = params();
        let groups = [
            snap(3, 10, now, true, 0.0, 0.0),
            snap(1, 10, now, true, 0.0, 0.0),
            snap(2, 20, now, false, 0.0, 0.0),
        ];
        let volume: Vec<u64> = rank(View::Volume, &groups, now, &params)
            .iter()
            .map(|g| g.id)
            .collect();
        assert_eq!(volume, [2, 1, 3]);
        let muted: Vec<u64> = rank(View::Muted, &groups, now, &params)
            .iter()
            .map(|g| g.id)
            .collect();
        assert_eq!(muted, [1, 3]);
    }

    #[test]
    fn new_orders_by_latest_first_seen_then_id() {
        let params = params();
        let now = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 50).unwrap();
        let groups = [
            snap(2, 1, utc(10), false, 0.0, 0.0),
            snap(1, 1, utc(10), false, 0.0, 0.0),
            snap(3, 1, utc(20), false, 0.0, 0.0),
        ];
        let ids: Vec<u64> = rank(View::New, &groups, now, &params)
            .iter()
            .map(|g| g.id)
            .collect();
        assert_eq!(ids, [3, 1, 2]);
    }

    #[test]
    fn increasing_orders_by_ratio_then_fast_then_id() {
        let params = params();
        let first = utc(0);
        let now = Utc.with_ymd_and_hms(2026, 8, 26, 12, 2, 0).unwrap();
        let groups = [
            snap(2, 20, first, false, 10.0, 2.0),
            snap(1, 20, first, false, 10.0, 2.0),
            snap(3, 20, first, false, 12.0, 2.0),
            snap(4, 20, first, false, 8.0, 4.0),
        ];
        let ids: Vec<u64> = rank(View::Increasing, &groups, now, &params)
            .iter()
            .map(|g| g.id)
            .collect();
        assert_eq!(ids, [3, 1, 2, 4]);
    }
}
