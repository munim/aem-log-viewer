use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};

use super::cli::Timezone;

const AEM_WALL: &str = "%d.%m.%Y %H:%M:%S%.3f";

/// Why a zone-less wall time could not be mapped to one instant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TimeFault {
    Ambiguous,
    Nonexistent,
}

impl TimeFault {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Ambiguous => "ambiguous_wall_time",
            Self::Nonexistent => "nonexistent_wall_time",
        }
    }
}

/// Source instant after zone interpretation, or arrival-time fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct InterpretedTime {
    pub instant: DateTime<Utc>,
    pub fallback: Option<TimeFault>,
}

/// Interprets zone-less AEM timestamps in a configured zone.
#[derive(Clone, Copy, Debug)]
pub(super) struct TimeInterpreter {
    timezone: Timezone,
    /// Test-only stand-in for the host zone when `timezone` is `Local`.
    injected_local: Option<chrono_tz::Tz>,
}

impl TimeInterpreter {
    pub(super) fn new(timezone: Timezone) -> Self {
        Self {
            timezone,
            injected_local: None,
        }
    }

    /// Run the `local` mapper with this IANA zone so tests do not depend on
    /// the host timezone.
    #[cfg(test)]
    pub(super) fn with_local_zone(tz: chrono_tz::Tz) -> Self {
        Self {
            timezone: Timezone::Local,
            injected_local: Some(tz),
        }
    }

    pub(super) fn interpret(self, wall: &str, arrival: DateTime<Utc>) -> InterpretedTime {
        let Some(naive) = NaiveDateTime::parse_from_str(wall, AEM_WALL).ok() else {
            return InterpretedTime {
                instant: arrival,
                fallback: Some(TimeFault::Nonexistent),
            };
        };
        match self.map_wall(naive) {
            chrono::LocalResult::Single(instant) => InterpretedTime {
                instant,
                fallback: None,
            },
            chrono::LocalResult::Ambiguous(_, _) => InterpretedTime {
                instant: arrival,
                fallback: Some(TimeFault::Ambiguous),
            },
            chrono::LocalResult::None => InterpretedTime {
                instant: arrival,
                fallback: Some(TimeFault::Nonexistent),
            },
        }
    }

    fn map_wall(self, naive: NaiveDateTime) -> chrono::LocalResult<DateTime<Utc>> {
        match self.timezone {
            Timezone::Utc => chrono::LocalResult::Single(naive.and_utc()),
            Timezone::Local => match self.injected_local {
                Some(tz) => tz
                    .from_local_datetime(&naive)
                    .map(|dt| dt.with_timezone(&Utc)),
                None => chrono::Local
                    .from_local_datetime(&naive)
                    .map(|dt| dt.with_timezone(&Utc)),
            },
            Timezone::Iana(tz) => tz
                .from_local_datetime(&naive)
                .map(|dt| dt.with_timezone(&Utc)),
        }
    }
}

pub(super) fn rfc3339_millis(instant: DateTime<Utc>) -> String {
    instant.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Later events on the same group cannot move its temporal clock backward.
pub(super) fn clamp_effective(
    previous: Option<DateTime<Utc>>,
    candidate: DateTime<Utc>,
) -> DateTime<Utc> {
    match previous {
        Some(prev) if candidate < prev => prev,
        _ => candidate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::frame;
    use chrono::Timelike;

    fn utc(y: i32, m: u32, d: u32, hh: u32, mm: u32, ss: u32, ms: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, hh, mm, ss)
            .unwrap()
            .with_nanosecond(ms * 1_000_000)
            .unwrap()
    }

    fn arrival() -> DateTime<Utc> {
        utc(2026, 8, 26, 20, 0, 0, 0)
    }

    fn interpret(timezone: Timezone, wall: &str) -> InterpretedTime {
        TimeInterpreter::new(timezone).interpret(wall, arrival())
    }

    fn iana(name: &str) -> Timezone {
        Timezone::Iana(name.parse().expect(name))
    }

    #[test]
    fn utc_default_converts_aem_wall_to_matching_instant() {
        let got = interpret(Timezone::Utc, "26.08.2026 12:00:00.123");
        assert_eq!(got.fallback, None);
        assert_eq!(got.instant, utc(2026, 8, 26, 12, 0, 0, 123));
        assert_eq!(rfc3339_millis(got.instant), "2026-08-26T12:00:00.123Z");
    }

    #[test]
    fn utc_source_time_agrees_with_embedded_request_epoch() {
        let epoch_ms = 1_787_745_601_456i64;
        let event = format!(
            "26.08.2026 12:00:01.456 author-0 *ERROR* [192.0.2.10 [{epoch_ms}] GET /content/site/us/en.html HTTP/1.1] com.example.core.filters.ErrorFilter Uncaught request exception"
        );
        let meta = frame::parse_metadata(&event).expect("header");
        let request = meta.request_context.expect("request context");
        assert_eq!(request.request_id, epoch_ms.to_string());
        let got = interpret(Timezone::Utc, meta.timestamp);
        assert_eq!(got.fallback, None);
        assert_eq!(got.instant.timestamp_millis(), epoch_ms);
    }

    #[test]
    fn named_positive_offset_converts_to_utc() {
        let got = interpret(iana("Asia/Tokyo"), "26.08.2026 12:00:00.000");
        assert_eq!(got.fallback, None);
        assert_eq!(rfc3339_millis(got.instant), "2026-08-26T03:00:00.000Z");
    }

    #[test]
    fn named_negative_offset_converts_to_utc() {
        let got = interpret(iana("America/New_York"), "26.08.2026 12:00:00.000");
        assert_eq!(got.fallback, None);
        assert_eq!(rfc3339_millis(got.instant), "2026-08-26T16:00:00.000Z");
    }

    #[test]
    fn cross_midnight_conversion_uses_previous_utc_date() {
        let got = interpret(iana("Asia/Tokyo"), "26.08.2026 02:00:00.000");
        assert_eq!(got.fallback, None);
        assert_eq!(rfc3339_millis(got.instant), "2026-08-25T17:00:00.000Z");
    }

    #[test]
    fn dst_spring_gap_is_nonexistent_and_uses_arrival() {
        // America/New_York 2026-03-08: 02:00 → 03:00.
        let got = interpret(iana("America/New_York"), "08.03.2026 02:30:00.000");
        assert_eq!(got.fallback, Some(TimeFault::Nonexistent));
        assert_eq!(got.fallback.unwrap().as_str(), "nonexistent_wall_time");
        assert_eq!(got.instant, arrival());
    }

    #[test]
    fn dst_fall_overlap_is_ambiguous_and_uses_arrival() {
        // America/New_York 2026-11-01: 02:00 → 01:00.
        let got = interpret(iana("America/New_York"), "01.11.2026 01:30:00.000");
        assert_eq!(got.fallback, Some(TimeFault::Ambiguous));
        assert_eq!(got.fallback.unwrap().as_str(), "ambiguous_wall_time");
        assert_eq!(got.instant, arrival());
    }

    #[test]
    fn injected_local_zone_matches_named_iana_rules() {
        let wall = "26.08.2026 12:00:00.000";
        let ny: chrono_tz::Tz = "America/New_York".parse().unwrap();
        let injected = TimeInterpreter::with_local_zone(ny).interpret(wall, arrival());
        let named = TimeInterpreter::new(Timezone::Iana(ny)).interpret(wall, arrival());
        assert_eq!(injected, named);
        assert_eq!(rfc3339_millis(injected.instant), "2026-08-26T16:00:00.000Z");
        assert_eq!(
            TimeInterpreter::with_local_zone(ny).timezone,
            Timezone::Local
        );
    }

    #[test]
    fn impossible_civil_date_uses_arrival_fallback() {
        let got = interpret(Timezone::Utc, "31.02.2026 12:00:00.000");
        assert_eq!(got.fallback, Some(TimeFault::Nonexistent));
        assert_eq!(got.instant, arrival());
    }

    #[test]
    fn small_source_regression_does_not_move_group_clock_backward() {
        let first = utc(2026, 8, 26, 12, 0, 2, 0);
        let earlier = utc(2026, 8, 26, 12, 0, 1, 0);
        let later = utc(2026, 8, 26, 12, 0, 3, 0);
        assert_eq!(clamp_effective(None, first), first);
        assert_eq!(clamp_effective(Some(first), earlier), first);
        assert_eq!(clamp_effective(Some(first), later), later);
        assert_eq!(clamp_effective(Some(first), first), first);
    }
}
