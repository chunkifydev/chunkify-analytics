//! Short leases shared by every analytics server. A claim is a row inserted
//! with a deduplication token per (kind, key, term): when servers race, only
//! one row is stored, and its owner holds the claim for that term.
use super::{Client, Params};
use crate::api::ApiError;
use serde_json::{Value, json};

impl Client {
    /// ClickHouse's clock, shared by every server, in milliseconds.
    pub(super) async fn now_ms(&self) -> Result<i64, ApiError> {
        self.one(
            "SELECT toUnixTimestamp64Milli(now64(3))",
            &Params::default(),
        )
        .await?
        .first()
        .and_then(Value::as_i64)
        .ok_or_else(ApiError::internal)
    }

    /// Claim `key` of `kind` for `term`. True when `owner` holds it, whether
    /// this call or an earlier one by the same owner stored the claim.
    pub(super) async fn claim(
        &self,
        kind: &str,
        key: i64,
        term: i64,
        owner: &str,
    ) -> Result<bool, ApiError> {
        let row = json!({"kind": kind, "key": key, "term": term, "owner": owner});
        let token = format!("chunkify-claim-{kind}-{key}-{term}");
        self.insert(
            "claims",
            serde_json::to_vec(&row).map_err(|_| ApiError::internal())?,
            &[
                ("async_insert", "0"),
                ("insert_deduplicate", "1"),
                ("insert_deduplication_token", &token),
            ],
        )
        .await?;
        let mut params = Params::default();
        let sql = format!(
            "SELECT owner FROM claims WHERE kind = {} AND key = {} AND term = {}
            ORDER BY written_at LIMIT 1",
            params.text(kind),
            params.int(key),
            params.int(term)
        );
        let winner = self.all(&sql, &params).await?;
        Ok(winner
            .first()
            .and_then(|row| row.first())
            .and_then(Value::as_str)
            == Some(owner))
    }

    /// Keys of `kind` claimed by another owner for any term in `terms`.
    pub(super) async fn claimed_by_others(
        &self,
        kind: &str,
        terms: std::ops::RangeInclusive<i64>,
        owner: &str,
    ) -> Result<Vec<i64>, ApiError> {
        let mut params = Params::default();
        let sql = format!(
            "SELECT DISTINCT key FROM claims
            WHERE kind = {} AND term >= {} AND term <= {} AND owner != {}",
            params.text(kind),
            params.int(*terms.start()),
            params.int(*terms.end()),
            params.text(owner)
        );
        self.all(&sql, &params)
            .await?
            .iter()
            .map(|row| {
                row.first()
                    .and_then(Value::as_i64)
                    .ok_or_else(ApiError::internal)
            })
            .collect()
    }
}
