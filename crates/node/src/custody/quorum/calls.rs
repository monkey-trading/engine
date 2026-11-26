//! The quorum's contract reads: a call's return data voted under its own operation, and the schedule's
//! governance record.

use alloy::primitives::{Address, Bytes};
use futures_util::future::join_all;

use super::Quorum;
use crate::custody::evm::{BlockRef, GovernanceRecord, VaultReader};

impl<R: VaultReader + Sync> Quorum<R> {
    /// `contract_call`, its quorum and provider alerts kept under `operation`, so each distinct call
    /// of a caller has its own episode and one that succeeds cannot end another's.
    /// Error cases:
    /// - no quorum
    pub(crate) async fn call_as(
        &self,
        operation: &'static str,
        at: BlockRef,
        to: Address,
        data: Bytes,
    ) -> anyhow::Result<Bytes> {
        let answers = join_all(self.providers.iter().enumerate().map(|(i, (_, r))| {
            let data = data.clone();
            async move {
                self.qualified(i)?;
                r.contract_call(at, to, data).await
            }
        }))
        .await;
        self.vote(operation, answers).map(|v| v.0)
    }

    /// The same governance record at the same canonical block hash: a provider that reads none where the
    /// majority reads one cannot drop the record, nor one that reads another replace it.
    /// Error cases:
    /// - no quorum
    pub(super) async fn voted_governance_record(&self, at: BlockRef) -> anyhow::Result<GovernanceRecord> {
        let answers = join_all(self.providers.iter().enumerate().map(|(i, (_, r))| async move {
            self.qualified(i)?;
            r.governance_record(at).await
        }))
        .await;
        self.vote("governance_record", answers).map(|v| v.0)
    }
}
