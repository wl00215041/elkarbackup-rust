use std::collections::HashMap;

use chrono::{NaiveTime, Weekday};
use elkar_core::ids::PolicyId;
use elkar_core::policy::{Policy, PolicySpec, RetainLevel, RetainRule};
use elkar_core::schedule::{Schedule, parse::weekday_from_iso};
use sqlx::{FromRow, PgPool, Postgres, Transaction};

use crate::convert::{parse, to_i32, to_u32};
use crate::error::{OptionalExt, StorageError, StorageResult, affected};

#[derive(FromRow)]
struct PolicyRow {
    id: i64,
    name: String,
    description: String,
    include: String,
    exclude: String,
    sync_first: bool,
}

#[derive(FromRow)]
struct RetainRow {
    policy_id: i64,
    level: String,
    count: i32,
    times: Vec<NaiveTime>,
    days_of_week: Vec<i16>,
    days_of_month: Vec<i16>,
    months: Vec<i16>,
}

impl TryFrom<RetainRow> for RetainRule {
    type Error = StorageError;

    fn try_from(row: RetainRow) -> StorageResult<Self> {
        let unsigned = |values: Vec<i16>| -> StorageResult<Vec<u32>> {
            values
                .into_iter()
                .map(|v| u32::try_from(v).map_err(|_| StorageError::Corrupt(format!("negative calendar value {v}"))))
                .collect()
        };
        let days_of_week = row
            .days_of_week
            .into_iter()
            .map(|d| {
                u8::try_from(d)
                    .ok()
                    .and_then(weekday_from_iso)
                    .ok_or_else(|| StorageError::Corrupt(format!("invalid weekday {d}")))
            })
            .collect::<StorageResult<Vec<Weekday>>>()?;
        Ok(Self {
            level: parse::<RetainLevel>(&row.level)?,
            count: to_u32(row.count, "count")?,
            schedule: Schedule {
                times: row.times,
                days_of_week,
                days_of_month: unsigned(row.days_of_month)?,
                months: unsigned(row.months)?,
            },
        })
    }
}

fn small(values: &[u32]) -> Vec<i16> {
    values.iter().map(|v| i16::try_from(*v).unwrap_or(i16::MAX)).collect()
}

pub struct PolicyRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> PolicyRepo<'a> {
    pub(crate) fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, spec: &PolicySpec) -> StorageResult<PolicyId> {
        let mut tx = self.pool.begin().await?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO policies (name, description, include, exclude, sync_first)
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(&spec.name)
        .bind(&spec.description)
        .bind(&spec.include)
        .bind(&spec.exclude)
        .bind(spec.sync_first)
        .fetch_one(&mut *tx)
        .await?;
        insert_retains(&mut tx, id, &spec.retains).await?;
        tx.commit().await?;
        Ok(PolicyId(id))
    }

    pub async fn update(&self, id: PolicyId, spec: &PolicySpec) -> StorageResult<()> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE policies SET name = $2, description = $3, include = $4, exclude = $5, sync_first = $6
             WHERE id = $1",
        )
        .bind(id.0)
        .bind(&spec.name)
        .bind(&spec.description)
        .bind(&spec.include)
        .bind(&spec.exclude)
        .bind(spec.sync_first)
        .execute(&mut *tx)
        .await?;
        affected(result)?;
        sqlx::query("DELETE FROM policy_retains WHERE policy_id = $1")
            .bind(id.0)
            .execute(&mut *tx)
            .await?;
        insert_retains(&mut tx, id.0, &spec.retains).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn get(&self, id: PolicyId) -> StorageResult<Policy> {
        let row: Option<PolicyRow> = sqlx::query_as("SELECT * FROM policies WHERE id = $1")
            .bind(id.0)
            .fetch_optional(self.pool)
            .await?;
        let mut policies = self.attach_retains(vec![row.found()?]).await?;
        Ok(policies.remove(0))
    }

    pub async fn list(&self) -> StorageResult<Vec<Policy>> {
        let rows: Vec<PolicyRow> = sqlx::query_as("SELECT * FROM policies ORDER BY name")
            .fetch_all(self.pool)
            .await?;
        self.attach_retains(rows).await
    }

    pub async fn delete(&self, id: PolicyId) -> StorageResult<()> {
        let result = sqlx::query("DELETE FROM policies WHERE id = $1")
            .bind(id.0)
            .execute(self.pool)
            .await?;
        affected(result)
    }

    async fn attach_retains(&self, rows: Vec<PolicyRow>) -> StorageResult<Vec<Policy>> {
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        let retain_rows: Vec<RetainRow> = sqlx::query_as("SELECT * FROM policy_retains WHERE policy_id = ANY($1)")
            .bind(&ids)
            .fetch_all(self.pool)
            .await?;
        let mut by_policy: HashMap<i64, Vec<RetainRule>> = HashMap::new();
        for row in retain_rows {
            let policy_id = row.policy_id;
            by_policy.entry(policy_id).or_default().push(row.try_into()?);
        }
        Ok(rows
            .into_iter()
            .map(|row| {
                let mut retains = by_policy.remove(&row.id).unwrap_or_default();
                retains.sort_by_key(|r| RetainLevel::ALL.iter().position(|l| *l == r.level));
                Policy {
                    id: PolicyId(row.id),
                    spec: PolicySpec {
                        name: row.name,
                        description: row.description,
                        include: row.include,
                        exclude: row.exclude,
                        sync_first: row.sync_first,
                        retains,
                    },
                }
            })
            .collect())
    }
}

async fn insert_retains(
    tx: &mut Transaction<'_, Postgres>,
    policy_id: i64,
    retains: &[RetainRule],
) -> StorageResult<()> {
    for rule in retains {
        let weekdays: Vec<i16> = rule
            .schedule
            .days_of_week
            .iter()
            .map(|d| i16::from(u8::try_from(d.number_from_monday()).unwrap_or(1)))
            .collect();
        sqlx::query(
            "INSERT INTO policy_retains (policy_id, level, count, times, days_of_week, days_of_month, months)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(policy_id)
        .bind(rule.level.as_str())
        .bind(to_i32(rule.count))
        .bind(&rule.schedule.times)
        .bind(weekdays)
        .bind(small(&rule.schedule.days_of_month))
        .bind(small(&rule.schedule.months))
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}
