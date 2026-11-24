//! Backup policies: which retain levels exist, how many snapshots each keeps
//! and when each one fires.

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::error::ValidationErrors;
use crate::ids::PolicyId;
use crate::schedule::Schedule;

text_enum! {
    /// rsnapshot retain levels, declared from the most to the least frequent.
    pub enum RetainLevel {
        Hourly => "hourly",
        Daily => "daily",
        Weekly => "weekly",
        Monthly => "monthly",
        Yearly => "yearly",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainRule {
    pub level: RetainLevel,
    /// Number of snapshots rsnapshot keeps for this level (always > 0).
    pub count: u32,
    pub schedule: Schedule,
}

/// Fields shared by a stored policy and a create/update request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicySpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Newline separated rsync include patterns inherited by jobs.
    #[serde(default)]
    pub include: String,
    /// Newline separated rsync exclude patterns inherited by jobs.
    #[serde(default)]
    pub exclude: String,
    #[serde(default)]
    pub sync_first: bool,
    pub retains: Vec<RetainRule>,
}

impl PolicySpec {
    /// Validates and normalises the spec (retains sorted by level).
    pub fn validate(mut self) -> Result<Self, ValidationErrors> {
        let mut errors = ValidationErrors::new();
        self.name = self.name.trim().to_owned();
        errors.check(!self.name.is_empty(), "name", "name is required");
        errors.check(
            !self.retains.is_empty(),
            "retains",
            "at least one retain level is required",
        );

        self.retains.sort_by_key(|r| level_rank(r.level));
        if self.retains.windows(2).any(|w| w[0].level == w[1].level) {
            errors.add("retains", "each retain level may appear only once");
        }
        for rule in &self.retains {
            errors.check(rule.count > 0, "retains", "retain count must be greater than zero");
            if let Some(problem) = rule.schedule.problem() {
                errors.add("retains", format!("{}: {problem}", rule.level));
            }
        }
        errors.into_result().map(|()| self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub id: PolicyId,
    #[serde(flatten)]
    pub spec: PolicySpec,
}

impl Policy {
    /// Retain rules ordered from the most frequent level to the least.
    pub fn retains(&self) -> &[RetainRule] {
        &self.spec.retains
    }

    /// The level that actually synchronises data; higher levels only rotate.
    pub fn lowest_level(&self) -> Option<RetainLevel> {
        self.spec.retains.first().map(|r| r.level)
    }

    /// Levels scheduled at `at`, least frequent first, which is the order
    /// rsnapshot needs so that rotations happen before the new sync.
    pub fn scheduled_levels(&self, at: NaiveDateTime) -> Vec<RetainLevel> {
        self.spec
            .retains
            .iter()
            .rev()
            .filter(|r| r.schedule.matches(at))
            .map(|r| r.level)
            .collect()
    }

    /// Levels to run for a job enqueued at `enqueued_at`. A job enqueued on
    /// demand (outside any schedule) runs only the lowest level.
    pub fn levels_to_run(&self, enqueued_at: NaiveDateTime) -> Vec<RetainLevel> {
        let scheduled = self.scheduled_levels(enqueued_at);
        if scheduled.is_empty() {
            self.lowest_level().into_iter().collect()
        } else {
            scheduled
        }
    }

    /// With `sync_first`, rsnapshot needs an explicit `sync` before the
    /// lowest level rotation.
    pub fn must_sync(&self, level: RetainLevel) -> bool {
        self.spec.sync_first && self.lowest_level() == Some(level)
    }
}

fn level_rank(level: RetainLevel) -> usize {
    RetainLevel::ALL
        .iter()
        .position(|l| *l == level)
        .expect("ALL lists every variant")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::parse;
    use chrono::NaiveDate;

    fn rule(level: RetainLevel, count: u32, times: &str) -> RetainRule {
        RetainRule {
            level,
            count,
            schedule: Schedule {
                times: parse::times(times).unwrap(),
                ..Schedule::default()
            },
        }
    }

    fn policy(retains: Vec<RetainRule>, sync_first: bool) -> Policy {
        let spec = PolicySpec {
            name: "p".into(),
            sync_first,
            retains,
            ..PolicySpec::default()
        }
        .validate()
        .unwrap();
        Policy { id: PolicyId(1), spec }
    }

    fn at(h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2024, 1, 1)
            .unwrap()
            .and_hms_opt(h, m, 0)
            .unwrap()
    }

    #[test]
    fn validate_sorts_levels_and_rejects_duplicates() {
        let p = policy(
            vec![
                rule(RetainLevel::Weekly, 4, "21:00"),
                rule(RetainLevel::Daily, 7, "21:00"),
            ],
            false,
        );
        assert_eq!(p.lowest_level(), Some(RetainLevel::Daily));

        let dup = PolicySpec {
            name: "p".into(),
            retains: vec![
                rule(RetainLevel::Daily, 1, "10:00"),
                rule(RetainLevel::Daily, 2, "11:00"),
            ],
            ..PolicySpec::default()
        };
        assert!(dup.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_name_and_zero_count() {
        let spec = PolicySpec {
            name: "  ".into(),
            retains: vec![rule(RetainLevel::Daily, 0, "10:00")],
            ..PolicySpec::default()
        };
        let errors = spec.validate().unwrap_err();
        assert_eq!(errors.iter().count(), 2);
    }

    #[test]
    fn scheduled_levels_are_least_frequent_first() {
        let p = policy(
            vec![
                rule(RetainLevel::Daily, 7, "21:00"),
                rule(RetainLevel::Weekly, 4, "21:00"),
            ],
            false,
        );
        assert_eq!(
            p.scheduled_levels(at(21, 0)),
            vec![RetainLevel::Weekly, RetainLevel::Daily]
        );
        assert!(p.scheduled_levels(at(9, 0)).is_empty());
    }

    #[test]
    fn on_demand_runs_lowest_level() {
        let p = policy(
            vec![
                rule(RetainLevel::Daily, 7, "21:00"),
                rule(RetainLevel::Weekly, 4, "21:00"),
            ],
            false,
        );
        assert_eq!(p.levels_to_run(at(9, 0)), vec![RetainLevel::Daily]);
    }

    #[test]
    fn must_sync_only_for_lowest_level_with_sync_first() {
        let retains = vec![
            rule(RetainLevel::Daily, 7, "21:00"),
            rule(RetainLevel::Weekly, 4, "21:00"),
        ];
        let p = policy(retains.clone(), true);
        assert!(p.must_sync(RetainLevel::Daily));
        assert!(!p.must_sync(RetainLevel::Weekly));
        assert!(!policy(retains, false).must_sync(RetainLevel::Daily));
    }
}
