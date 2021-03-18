use super::logs::Logs;
use super::pool::internal::Pool;
use crate::{
    blockcfg::{ApplyBlockLedger, Contents, ContentsBuilder, LedgerParameters},
    fragment::FragmentId,
};
use chain_core::property::Fragment as _;
use jormungandr_lib::interfaces::FragmentStatus;

use async_trait::async_trait;
use futures::prelude::*;
use tracing::{span, Level};

use std::error::Error;
use std::iter;

pub enum SelectionOutput {
    Commit { fragment_id: FragmentId },
    RequestSmallerFee,
    RequestSmallerSize,
    Reject { reason: String },
}

#[async_trait]
pub trait FragmentSelectionAlgorithm {
    async fn select(
        &mut self,
        ledger: ApplyBlockLedger,
        ledger_params: &LedgerParameters,
        logs: &mut Logs,
        pool: &mut Pool,
        abort_future: futures::channel::oneshot::Receiver<()>,
    ) -> (Contents, ApplyBlockLedger);
}

#[derive(Debug)]
pub enum FragmentSelectionAlgorithmParams {
    OldestFirst,
}

pub struct OldestFirst;

impl OldestFirst {
    pub fn new() -> Self {
        OldestFirst
    }
}

impl Default for OldestFirst {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl FragmentSelectionAlgorithm for OldestFirst {
    async fn select(
        &mut self,
        mut ledger: ApplyBlockLedger,
        ledger_params: &LedgerParameters,
        logs: &mut Logs,
        pool: &mut Pool,
        abort_future: futures::channel::oneshot::Receiver<()>,
    ) -> (Contents, ApplyBlockLedger) {
        let mut current_total_size = 0;
        let mut contents_builder = ContentsBuilder::new();
        let mut return_to_pool = Vec::new();

        let abort_future = abort_future.shared();

        while let Some(fragment) = pool.remove_oldest() {
            let id = fragment.id();
            let fragment_raw = fragment.to_raw(); // TODO: replace everything to FragmentRaw in the node
            let fragment_size = fragment_raw.size_bytes_plus_size() as u32;

            let span = span!(Level::TRACE, "fragment_selection_algorithm", kind="older_first", hash=%id.to_string());
            let _enter = span.enter();
            if fragment_size > ledger_params.block_content_max_size {
                let reason = format!(
                    "fragment size {} exceeds maximum block content size {}",
                    fragment_size, ledger_params.block_content_max_size
                );
                tracing::debug!("{}", reason);
                logs.modify(id, FragmentStatus::Rejected { reason });
                continue;
            }

            let total_size = current_total_size + fragment_size;

            if total_size > ledger_params.block_content_max_size {
                // return a fragment to the pool later if does not fit the contents size limit
                return_to_pool.push(fragment);
                continue;
            }

            tracing::debug!("applying fragment in simulation");

            let fragment1 = fragment.clone();
            let ledger1 = ledger.clone();
            let fragment_future =
                tokio::task::spawn_blocking(move || ledger1.apply_fragment(&fragment1));

            let result = tokio::select! {
                join_result = fragment_future => join_result.unwrap(),
                _ = abort_future.clone() => {
                    // When we have other fragments but cannot process this one in time, we just
                    // leave it until the next call.
                    if current_total_size > 0 {
                        return_to_pool.push(fragment);
                        break;
                    }

                    // if we cannot process a single fragment within the given time bounds it
                    // should be rejected
                    let reason = "cannot process a single fragment within the given time bounds";
                    tracing::debug!("{}", reason);
                    logs.modify(id, FragmentStatus::Rejected { reason: reason.to_string() });
                    break;
                }
            };

            match result {
                Ok(ledger_new) => {
                    contents_builder.push(fragment);
                    ledger = ledger_new;
                    tracing::debug!("successfully applied and committed the fragment");
                }
                Err(error) => {
                    let mut msg = error.to_string();
                    for e in iter::successors(error.source(), |&e| e.source()) {
                        msg.push_str(": ");
                        msg.push_str(&e.to_string());
                    }
                    tracing::debug!(?error, "fragment is rejected");
                    logs.modify(id, FragmentStatus::Rejected { reason: msg })
                }
            }

            current_total_size = total_size;

            if total_size == ledger_params.block_content_max_size {
                break;
            }

            drop(_enter);
        }

        pool.insert_all(return_to_pool);

        (contents_builder.into(), ledger)
    }
}
