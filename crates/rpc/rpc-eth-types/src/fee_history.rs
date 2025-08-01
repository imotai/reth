//! Consist of types adjacent to the fee history cache and its configs

use std::{
    collections::{BTreeMap, VecDeque},
    fmt::Debug,
    sync::{atomic::Ordering::SeqCst, Arc},
};

use alloy_consensus::{BlockHeader, Header, Transaction, TxReceipt};
use alloy_eips::eip7840::BlobParams;
use alloy_rpc_types_eth::TxGasAndReward;
use futures::{
    future::{Fuse, FusedFuture},
    FutureExt, Stream, StreamExt,
};
use metrics::atomics::AtomicU64;
use reth_chain_state::CanonStateNotification;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_primitives_traits::{Block, BlockBody, NodePrimitives, SealedBlock};
use reth_rpc_server_types::constants::gas_oracle::MAX_HEADER_HISTORY;
use reth_storage_api::BlockReaderIdExt;
use serde::{Deserialize, Serialize};
use tracing::trace;

use crate::utils::checked_blob_gas_used_ratio;

use super::{EthApiError, EthStateCache};

/// Contains cached fee history entries for blocks.
///
/// Purpose for this is to provide cached data for `eth_feeHistory`.
#[derive(Debug, Clone)]
pub struct FeeHistoryCache<H> {
    inner: Arc<FeeHistoryCacheInner<H>>,
}

impl<H> FeeHistoryCache<H>
where
    H: BlockHeader + Clone,
{
    /// Creates new `FeeHistoryCache` instance, initialize it with the more recent data, set bounds
    pub fn new(config: FeeHistoryCacheConfig) -> Self {
        let inner = FeeHistoryCacheInner {
            lower_bound: Default::default(),
            upper_bound: Default::default(),
            config,
            entries: Default::default(),
        };
        Self { inner: Arc::new(inner) }
    }

    /// How the cache is configured.
    #[inline]
    pub fn config(&self) -> &FeeHistoryCacheConfig {
        &self.inner.config
    }

    /// Returns the configured resolution for percentile approximation.
    #[inline]
    pub fn resolution(&self) -> u64 {
        self.config().resolution
    }

    /// Returns all blocks that are missing in the cache in the [`lower_bound`, `upper_bound`]
    /// range.
    ///
    /// This function is used to populate the cache with missing blocks, which can happen if the
    /// node switched to stage sync node.
    async fn missing_consecutive_blocks(&self) -> VecDeque<u64> {
        let entries = self.inner.entries.read().await;
        (self.lower_bound()..self.upper_bound())
            .rev()
            .filter(|&block_number| !entries.contains_key(&block_number))
            .collect()
    }

    /// Insert block data into the cache.
    async fn insert_blocks<'a, I, B, R, C>(&self, blocks: I, chain_spec: &C)
    where
        B: Block<Header = H> + 'a,
        R: TxReceipt + 'a,
        I: IntoIterator<Item = (&'a SealedBlock<B>, &'a [R])>,
        C: EthChainSpec,
    {
        let mut entries = self.inner.entries.write().await;

        let percentiles = self.predefined_percentiles();
        // Insert all new blocks and calculate approximated rewards
        for (block, receipts) in blocks {
            let mut fee_history_entry = FeeHistoryEntry::<H>::new(
                block,
                chain_spec.blob_params_at_timestamp(block.header().timestamp()),
            );
            fee_history_entry.rewards = calculate_reward_percentiles_for_block(
                &percentiles,
                fee_history_entry.header.gas_used(),
                fee_history_entry.header.base_fee_per_gas().unwrap_or_default(),
                block.body().transactions(),
                receipts,
            )
            .unwrap_or_default();
            entries.insert(block.number(), fee_history_entry);
        }

        // enforce bounds by popping the oldest entries
        while entries.len() > self.inner.config.max_blocks as usize {
            entries.pop_first();
        }

        if entries.is_empty() {
            self.inner.upper_bound.store(0, SeqCst);
            self.inner.lower_bound.store(0, SeqCst);
            return
        }

        let upper_bound = *entries.last_entry().expect("Contains at least one entry").key();

        // also enforce proper lower bound in case we have gaps
        let target_lower = upper_bound.saturating_sub(self.inner.config.max_blocks);
        while entries.len() > 1 && *entries.first_key_value().unwrap().0 < target_lower {
            entries.pop_first();
        }

        let lower_bound = *entries.first_entry().expect("Contains at least one entry").key();
        self.inner.upper_bound.store(upper_bound, SeqCst);
        self.inner.lower_bound.store(lower_bound, SeqCst);
    }

    /// Get `UpperBound` value for `FeeHistoryCache`
    pub fn upper_bound(&self) -> u64 {
        self.inner.upper_bound.load(SeqCst)
    }

    /// Get `LowerBound` value for `FeeHistoryCache`
    pub fn lower_bound(&self) -> u64 {
        self.inner.lower_bound.load(SeqCst)
    }

    /// Collect fee history for the given range (inclusive `start_block..=end_block`).
    ///
    /// This function retrieves fee history entries from the cache for the specified range.
    /// If the requested range (`start_block` to `end_block`) is within the cache bounds,
    /// it returns the corresponding entries.
    /// Otherwise it returns None.
    pub async fn get_history(
        &self,
        start_block: u64,
        end_block: u64,
    ) -> Option<Vec<FeeHistoryEntry<H>>> {
        if end_block < start_block {
            // invalid range, return None
            return None
        }
        let lower_bound = self.lower_bound();
        let upper_bound = self.upper_bound();
        if start_block >= lower_bound && end_block <= upper_bound {
            let entries = self.inner.entries.read().await;
            let result = entries
                .range(start_block..=end_block)
                .map(|(_, fee_entry)| fee_entry.clone())
                .collect::<Vec<_>>();

            if result.is_empty() {
                return None
            }

            Some(result)
        } else {
            None
        }
    }

    /// Generates predefined set of percentiles
    ///
    /// This returns 100 * resolution points
    pub fn predefined_percentiles(&self) -> Vec<f64> {
        let res = self.resolution() as f64;
        (0..=100 * self.resolution()).map(|p| p as f64 / res).collect()
    }
}

/// Settings for the [`FeeHistoryCache`].
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeHistoryCacheConfig {
    /// Max number of blocks in cache.
    ///
    /// Default is [`MAX_HEADER_HISTORY`] plus some change to also serve slightly older blocks from
    /// cache, since `fee_history` supports the entire range
    pub max_blocks: u64,
    /// Percentile approximation resolution
    ///
    /// Default is 4 which means 0.25
    pub resolution: u64,
}

impl Default for FeeHistoryCacheConfig {
    fn default() -> Self {
        Self { max_blocks: MAX_HEADER_HISTORY + 100, resolution: 4 }
    }
}

/// Container type for shared state in [`FeeHistoryCache`]
#[derive(Debug)]
struct FeeHistoryCacheInner<H> {
    /// Stores the lower bound of the cache
    lower_bound: AtomicU64,
    /// Stores the upper bound of the cache
    upper_bound: AtomicU64,
    /// Config for `FeeHistoryCache`, consists of resolution for percentile approximation
    /// and max number of blocks
    config: FeeHistoryCacheConfig,
    /// Stores the entries of the cache
    entries: tokio::sync::RwLock<BTreeMap<u64, FeeHistoryEntry<H>>>,
}

/// Awaits for new chain events and directly inserts them into the cache so they're available
/// immediately before they need to be fetched from disk.
pub async fn fee_history_cache_new_blocks_task<St, Provider, N>(
    fee_history_cache: FeeHistoryCache<N::BlockHeader>,
    mut events: St,
    provider: Provider,
    cache: EthStateCache<N>,
) where
    St: Stream<Item = CanonStateNotification<N>> + Unpin + 'static,
    Provider:
        BlockReaderIdExt<Block = N::Block, Receipt = N::Receipt> + ChainSpecProvider + 'static,
    N: NodePrimitives,
    N::BlockHeader: BlockHeader + Clone,
{
    // We're listening for new blocks emitted when the node is in live sync.
    // If the node transitions to stage sync, we need to fetch the missing blocks
    let mut missing_blocks = VecDeque::new();
    let mut fetch_missing_block = Fuse::terminated();

    loop {
        if fetch_missing_block.is_terminated() {
            if let Some(block_number) = missing_blocks.pop_front() {
                trace!(target: "rpc::fee", ?block_number, "Fetching missing block for fee history cache");
                if let Ok(Some(hash)) = provider.block_hash(block_number) {
                    // fetch missing block
                    fetch_missing_block = cache.get_block_and_receipts(hash).boxed().fuse();
                }
            }
        }

        let chain_spec = provider.chain_spec();

        tokio::select! {
            res = &mut fetch_missing_block =>  {
                if let Ok(res) = res {
                    let res = res.as_ref()
                        .map(|(b, r)| (b.sealed_block(), r.as_slice()));
                    fee_history_cache.insert_blocks(res, &chain_spec).await;
                }
            }
            event = events.next() =>  {
                let Some(event) = event else {
                     // the stream ended, we are done
                    break
                };

                let committed = event.committed();
                let blocks_and_receipts = committed
                    .blocks_and_receipts()
                    .map(|(block, receipts)| {
                        (block.sealed_block(), receipts.as_slice())
                    });
                fee_history_cache.insert_blocks(blocks_and_receipts, &chain_spec).await;

                // keep track of missing blocks
                missing_blocks = fee_history_cache.missing_consecutive_blocks().await;
            }
        }
    }
}

/// Calculates reward percentiles for transactions in a block header.
/// Given a list of percentiles and a sealed block header, this function computes
/// the corresponding rewards for the transactions at each percentile.
///
/// The results are returned as a vector of U256 values.
pub fn calculate_reward_percentiles_for_block<T, R>(
    percentiles: &[f64],
    gas_used: u64,
    base_fee_per_gas: u64,
    transactions: &[T],
    receipts: &[R],
) -> Result<Vec<u128>, EthApiError>
where
    T: Transaction,
    R: TxReceipt,
{
    let mut transactions = transactions
        .iter()
        .zip(receipts)
        .scan(0, |previous_gas, (tx, receipt)| {
            // Convert the cumulative gas used in the receipts
            // to the gas usage by the transaction
            //
            // While we will sum up the gas again later, it is worth
            // noting that the order of the transactions will be different,
            // so the sum will also be different for each receipt.
            let gas_used = receipt.cumulative_gas_used() - *previous_gas;
            *previous_gas = receipt.cumulative_gas_used();

            Some(TxGasAndReward {
                gas_used,
                reward: tx.effective_tip_per_gas(base_fee_per_gas).unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();

    // Sort the transactions by their rewards in ascending order
    transactions.sort_by_key(|tx| tx.reward);

    // Find the transaction that corresponds to the given percentile
    //
    // We use a `tx_index` here that is shared across all percentiles, since we know
    // the percentiles are monotonically increasing.
    let mut tx_index = 0;
    let mut cumulative_gas_used = transactions.first().map(|tx| tx.gas_used).unwrap_or_default();
    let mut rewards_in_block = Vec::with_capacity(percentiles.len());
    for percentile in percentiles {
        // Empty blocks should return in a zero row
        if transactions.is_empty() {
            rewards_in_block.push(0);
            continue
        }

        let threshold = (gas_used as f64 * percentile / 100.) as u64;
        while cumulative_gas_used < threshold && tx_index < transactions.len() - 1 {
            tx_index += 1;
            cumulative_gas_used += transactions[tx_index].gas_used;
        }
        rewards_in_block.push(transactions[tx_index].reward);
    }

    Ok(rewards_in_block)
}

/// A cached entry for a block's fee history.
#[derive(Debug, Clone)]
pub struct FeeHistoryEntry<H = Header> {
    /// The full block header.
    pub header: H,
    /// Gas used ratio this block.
    pub gas_used_ratio: f64,
    /// The base per blob gas for EIP-4844.
    /// For pre EIP-4844 equals to zero.
    pub base_fee_per_blob_gas: Option<u128>,
    /// Blob gas used ratio for this block.
    ///
    /// Calculated as the ratio of blob gas used and the available blob data gas per block.
    /// Will be zero if no blob gas was used or pre EIP-4844.
    pub blob_gas_used_ratio: f64,
    /// Approximated rewards for the configured percentiles.
    pub rewards: Vec<u128>,
    /// Blob parameters for this block.
    pub blob_params: Option<BlobParams>,
}

impl<H> FeeHistoryEntry<H>
where
    H: BlockHeader + Clone,
{
    /// Creates a new entry from a sealed block.
    ///
    /// Note: This does not calculate the rewards for the block.
    pub fn new<B>(block: &SealedBlock<B>, blob_params: Option<BlobParams>) -> Self
    where
        B: Block<Header = H>,
    {
        let header = block.header();
        Self {
            header: block.header().clone(),
            gas_used_ratio: header.gas_used() as f64 / header.gas_limit() as f64,
            base_fee_per_blob_gas: header
                .excess_blob_gas()
                .and_then(|excess_blob_gas| Some(blob_params?.calc_blob_fee(excess_blob_gas))),
            blob_gas_used_ratio: checked_blob_gas_used_ratio(
                block.body().blob_gas_used(),
                blob_params
                    .as_ref()
                    .map(|params| params.max_blob_gas_per_block())
                    .unwrap_or(alloy_eips::eip4844::MAX_DATA_GAS_PER_BLOCK_DENCUN),
            ),
            rewards: Vec::new(),
            blob_params,
        }
    }

    /// Returns the blob fee for the next block according to the EIP-4844 spec.
    ///
    /// Returns `None` if `excess_blob_gas` is None.
    ///
    /// See also [`Self::next_block_excess_blob_gas`]
    pub fn next_block_blob_fee(&self) -> Option<u128> {
        self.next_block_excess_blob_gas()
            .and_then(|excess_blob_gas| Some(self.blob_params?.calc_blob_fee(excess_blob_gas)))
    }

    /// Calculate excess blob gas for the next block according to the EIP-4844 spec.
    ///
    /// Returns a `None` if no excess blob gas is set, no EIP-4844 support
    pub fn next_block_excess_blob_gas(&self) -> Option<u64> {
        self.header.excess_blob_gas().and_then(|excess_blob_gas| {
            Some(
                self.blob_params?
                    .next_block_excess_blob_gas(excess_blob_gas, self.header.blob_gas_used()?),
            )
        })
    }
}
