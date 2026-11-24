//! Calendar rules deciding at which minutes a retain level fires.

use chrono::{Datelike, NaiveDateTime, NaiveTime, Timelike, Weekday};
use serde::{Deserialize, Serialize};

/// When a retain level runs. Empty calendar lists mean "any"; `times` must
/// not be empty because the scheduler only enqueues on exact minutes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub times: Vec<NaiveTime>,
    pub days_of_week: Vec<Weekday>,
    pub days_of_month: Vec<u32>,
    pub months: Vec<u32>,
}

impl Schedule {
    /// Whether the schedule fires on the minute containing `at`.
    pub fn matches(&self, at: NaiveDateTime) -> bool {
        let minute = (at.hour(), at.minute());
        self.times.iter().any(|t| (t.hour(), t.minute()) == minute)
            && (self.days_of_week.is_empty() || self.days_of_week.contains(&at.weekday()))
            && (self.days_of_month.is_empty() || self.days_of_month.contains(&at.day()))
            && (self.months.is_empty() || self.months.contains(&at.month()))
    }

    /// Returns a description of the first problem found, if any.
    pub fn problem(&self) -> Option<&'static str> {
        if self.times.is_empty() {
            return Some("at least one time of day is required");
        }
        if self.days_of_month.iter().any(|d| !(1..=31).contains(d)) {
            return Some("days of month must be between 1 and 31");
        }
        if self.months.iter().any(|m| !(1..=12).contains(m)) {
            return Some("months must be between 1 and 12");
        }
        None
    }
}

/// Parsers for the comma separated lists used in forms and the API.
pub mod parse {
    use super::*;

    fn items(input: &str) -> impl Iterator<Item = &str> {
        input.split([',', ' ', '|']).map(str::trim).filter(|s| !s.is_empty())
    }

    /// Parses `"08:00, 13:30"`.
    pub fn times(input: &str) -> Result<Vec<NaiveTime>, String> {
        let mut out: Vec<NaiveTime> = items(input)
            .map(|s| NaiveTime::parse_from_str(s, "%H:%M").map_err(|_| format!("invalid time {s:?}, expected HH:MM")))
            .collect::<Result<_, _>>()?;
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Parses `"1, 15"`.
    pub fn numbers(input: &str) -> Result<Vec<u32>, String> {
        let mut out: Vec<u32> = items(input)
            .map(|s| s.parse().map_err(|_| format!("invalid number {s:?}")))
            .collect::<Result<_, _>>()?;
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Parses `"mon, fri"` or ISO numbers `"1, 5"`.
    pub fn weekdays(input: &str) -> Result<Vec<Weekday>, String> {
        let mut out: Vec<Weekday> = items(input)
            .map(|s| match s.parse::<u8>() {
                Ok(n) => weekday_from_iso(n).ok_or_else(|| format!("invalid weekday {s:?}")),
                Err(_) => s.parse::<Weekday>().map_err(|_| format!("invalid weekday {s:?}")),
            })
            .collect::<Result<_, _>>()?;
        out.sort_by_key(Weekday::number_from_monday);
        out.dedup();
        Ok(out)
    }

    pub fn weekday_from_iso(n: u8) -> Option<Weekday> {
        (1..=7).contains(&n).then(|| Weekday::try_from(n - 1).ok()).flatten()
    }

    /// Inverse of the parsers above, for pre-filling forms.
    pub fn join<T: ToString>(values: impl IntoIterator<Item = T>) -> String {
        values.into_iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")
    }

    pub fn join_times(times: &[NaiveTime]) -> String {
        join(times.iter().map(|t| t.format("%H:%M")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    fn daily_at(time: &str) -> Schedule {
        Schedule {
            times: parse::times(time).unwrap(),
            ..Schedule::default()
        }
    }

    #[test]
    fn matches_exact_minute_only() {
        let s = daily_at("21:00");
        assert!(s.matches(at(2024, 3, 4, 21, 0)));
        assert!(!s.matches(at(2024, 3, 4, 21, 1)));
        assert!(!s.matches(at(2024, 3, 4, 20, 0)));
    }

    #[test]
    fn respects_calendar_filters() {
        let s = Schedule {
            times: parse::times("03:00").unwrap(),
            days_of_week: parse::weekdays("mon").unwrap(),
            months: vec![3],
            ..Schedule::default()
        };
        // 2024-03-04 is a Monday.
        assert!(s.matches(at(2024, 3, 4, 3, 0)));
        assert!(!s.matches(at(2024, 3, 5, 3, 0)));
        assert!(!s.matches(at(2024, 4, 1, 3, 0)));
    }

    #[test]
    fn parsers_accept_legacy_pipe_separator_and_dedup() {
        assert_eq!(parse::numbers("15|1|1").unwrap(), vec![1, 15]);
        assert_eq!(
            parse::weekdays("7, 1, tue").unwrap(),
            vec![Weekday::Mon, Weekday::Tue, Weekday::Sun]
        );
        assert!(parse::times("25:00").is_err());
        assert!(parse::weekdays("8").is_err());
    }

    #[test]
    fn problem_requires_times_and_valid_ranges() {
        assert!(Schedule::default().problem().is_some());
        let mut s = daily_at("10:00");
        assert_eq!(s.problem(), None);
        s.days_of_month = vec![32];
        assert!(s.problem().is_some());
    }

    #[test]
    fn join_round_trips() {
        let times = parse::times("13:30, 08:00").unwrap();
        assert_eq!(parse::join_times(&times), "08:00, 13:30");
        assert_eq!(parse::join([1, 2]), "1, 2");
    }
}
