// This file is part of Rundler.
//
// Rundler is free software: you can redistribute it and/or modify it under the
// terms of the GNU Lesser General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later version.
//
// Rundler is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
// without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
// See the GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License along with Rundler.
// If not, see https://www.gnu.org/licenses/.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use anyhow::{bail, ensure, Context};
use futures::future;
use metrics::{Counter, Gauge};
use metrics_derive::Metrics;
use rundler_contracts::{
    v0_6::IEntryPoint::{
        Deposited as DepositedV06, UserOperationEvent as UserOperationEventV06,
        Withdrawn as WithdrawnV06,
    },
    v0_7::IEntryPoint::{
        Deposited as DepositedV07, UserOperationEvent as UserOperationEventV07,
        Withdrawn as WithdrawnV07,
    },
};
use rundler_provider::{Block, EvmProvider, Filter, Log};
use rundler_task::{block_watcher, GracefulShutdown};
use rundler_types::{EntryPointVersion, Timestamp, UserOperationId};
use tokio::{
    select,
    sync::{broadcast, Semaphore},
    time,
};
use tracing::{info, warn};

const MAX_LOAD_OPS_CONCURRENCY: usize = 64;
const SYNC_ERROR_COUNT_MAX: usize = 50;

/// A data structure that holds the currently known recent state of the chain,
/// with logic for updating itself and returning what has changed.
///
/// Will update itself when `.sync_to_block_number` is called, at which point it
/// will query a node to determine the new state of the chain.
#[derive(Debug)]
pub(crate) struct Chain<P: EvmProvider> {
    provider: P,
    settings: Settings,
    /// Blocks are stored from earliest to latest, so the oldest block is at the
    /// front of this deque and the newest at the back.
    blocks: VecDeque<BlockSummary>,
    /// Semaphore to limit the number of concurrent `eth_getLogs` calls.
    load_ops_semaphore: Semaphore,
    sync_error_count: usize,
    /// Filter template.
    filter_template: Filter,
    /// Metrics of chain events.
    metrics: ChainMetrics,
}

#[derive(Default, Debug, Eq, PartialEq)]
pub struct ChainUpdate {
    pub latest_block_number: u64,
    pub latest_block_hash: B256,
    pub latest_block_timestamp: Timestamp,
    /// Blocks before this number are no longer tracked in this `Chain`, so no
    /// further updates related to them will be sent.
    pub earliest_remembered_block_number: u64,
    pub reorg_depth: u64,
    pub mined_ops: Vec<MinedOp>,
    pub unmined_ops: Vec<MinedOp>,
    /// List of on-chain entity balance updates made in the most recent block
    pub entity_balance_updates: Vec<BalanceUpdate>,
    /// List of entity balance updates that have been unmined due to a reorg
    pub unmined_entity_balance_updates: Vec<BalanceUpdate>,
    /// Boolean to state if the most recent chain update had a reorg
    /// that was larger than the existing history that has been tracked
    pub reorg_larger_than_history: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MinedOp {
    pub hash: B256,
    pub entry_point: Address,
    pub sender: Address,
    pub nonce: U256,
    pub actual_gas_cost: U256,
    pub paymaster: Option<Address>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BalanceUpdate {
    pub address: Address,
    pub entrypoint: Address,
    pub amount: U256,
    pub is_addition: bool,
}

impl MinedOp {
    pub fn id(&self) -> UserOperationId {
        UserOperationId {
            sender: self.sender,
            nonce: self.nonce,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Settings {
    pub(crate) history_size: u64,
    pub(crate) poll_interval: Duration,
    pub(crate) entry_point_addresses: HashMap<Address, EntryPointVersion>,
    pub(crate) max_sync_retries: u64,
}

#[derive(Debug)]
struct BlockSummary {
    number: u64,
    hash: B256,
    timestamp: Timestamp,
    parent_hash: B256,
    ops: Vec<MinedOp>,
    entity_balance_updates: Vec<BalanceUpdate>,
}

impl<P: EvmProvider> Chain<P> {
    pub(crate) fn new(provider: P, settings: Settings) -> Self {
        let history_size = settings.history_size as usize;
        assert!(history_size > 0, "history size should be positive");

        let mut events = vec![];

        if settings
            .entry_point_addresses
            .values()
            .any(|v| *v == EntryPointVersion::V0_6)
        {
            events.push(UserOperationEventV06::SIGNATURE_HASH);
            events.push(DepositedV06::SIGNATURE_HASH);
            events.push(WithdrawnV06::SIGNATURE_HASH);
        }
        if settings
            .entry_point_addresses
            .values()
            .any(|v| *v == EntryPointVersion::V0_7)
        {
            events.push(UserOperationEventV07::SIGNATURE_HASH);
            events.push(DepositedV07::SIGNATURE_HASH);
            events.push(WithdrawnV07::SIGNATURE_HASH);
        }

        let filter_template = Filter::new()
            .address(
                settings
                    .entry_point_addresses
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>(),
            )
            .event_signature(events);

        Self {
            provider,
            settings,
            blocks: VecDeque::new(),
            sync_error_count: 0,
            load_ops_semaphore: Semaphore::new(MAX_LOAD_OPS_CONCURRENCY),
            filter_template,
            metrics: ChainMetrics::default(),
        }
    }

    pub(crate) async fn watch(
        mut self,
        sender: broadcast::Sender<Arc<ChainUpdate>>,
        shutdown: GracefulShutdown,
    ) {
        loop {
            select! {
                update = self.wait_for_update() => {
                    let _ = sender.send(Arc::new(update));
                }
                _ = shutdown.clone() => {
                    info!("Shutting down chain watcher");
                    break;
                }
            }
        }
    }

    async fn wait_for_update(&mut self) -> ChainUpdate {
        let mut block_hash = self
            .blocks
            .back()
            .map(|block| block.hash)
            .unwrap_or_default();
        loop {
            let (hash, block) = block_watcher::wait_for_new_block(
                &self.provider,
                block_hash,
                self.settings.poll_interval,
            )
            .await;
            block_hash = hash;

            for i in 0..=self.settings.max_sync_retries {
                if i > 0 {
                    self.metrics.sync_retries.increment(1);
                }

                let update = self.sync_to_block(block.clone()).await;
                match update {
                    Ok(update) => return update,
                    Err(error) => {
                        warn!("Failed to update chain at block {block_hash:?}: {error:?}");
                    }
                }

                time::sleep(self.settings.poll_interval).await;
            }

            warn!(
                "Failed to update chain at block {:?} after {} retries. Abandoning sync.",
                block_hash, self.settings.max_sync_retries
            );
            self.metrics.sync_abandoned.increment(1);
        }
    }

    pub(crate) async fn sync_to_block(&mut self, new_head: Block) -> anyhow::Result<ChainUpdate> {
        let new_head = BlockSummary::try_from_block_without_ops(new_head, None)?;
        let Some(current_block) = self.blocks.back() else {
            return self.reset_and_initialize(new_head).await;
        };
        let current_block_number = current_block.number;
        let new_block_number = new_head.number;

        if current_block_number > new_block_number + self.settings.history_size {
            self.sync_error_count += 1;

            if self.sync_error_count >= SYNC_ERROR_COUNT_MAX {
                return self.reset_and_initialize(new_head).await;
            }

            bail!(
            "new block number {new_block_number} should be greater than start of history (current block: {current_block_number})"
            )
        }

        if current_block_number + self.settings.history_size < new_block_number {
            warn!(
                "New block {new_block_number} number is {} blocks ahead of the previously known head. Chain history will skip ahead.",
                new_block_number - current_block_number,
            );
            return self.reset_and_initialize(new_head).await;
        }

        let added_blocks = self
            .load_added_blocks_connecting_to_existing_chain(current_block_number, new_head)
            .await?;
        Ok(self.update_with_blocks(current_block_number, added_blocks))
    }

    async fn reset_and_initialize(&mut self, head: BlockSummary) -> anyhow::Result<ChainUpdate> {
        let min_block_number = head.number.saturating_sub(self.settings.history_size - 1);
        let mut blocks = self
            .load_blocks_back_to_number_no_ops(head, min_block_number)
            .await
            .context("should load full history when resetting chain")?;
        self.load_ops_into_block_summaries(&mut blocks).await?;
        self.blocks = blocks;
        self.sync_error_count = 0;
        let mined_ops: Vec<_> = self
            .blocks
            .iter()
            .flat_map(|block| &block.ops)
            .copied()
            .collect();

        let entity_balance_updates: Vec<_> = self
            .blocks
            .iter()
            .flat_map(|block| &block.entity_balance_updates)
            .copied()
            .collect();

        Ok(self.new_update(0, mined_ops, vec![], entity_balance_updates, vec![], false))
    }

    /// Given a collection of blocks to add to the chain, whose numbers may
    /// overlap the current numbers in the case of reorgs, update the state of
    /// this data structure and return an update struct.
    fn update_with_blocks(
        &mut self,
        current_block_number: u64,
        added_blocks: VecDeque<BlockSummary>,
    ) -> ChainUpdate {
        let mined_ops: Vec<_> = added_blocks
            .iter()
            .flat_map(|block| &block.ops)
            .copied()
            .collect();

        let entity_balance_updates: Vec<_> = added_blocks
            .iter()
            .flat_map(|block| &block.entity_balance_updates)
            .copied()
            .collect();

        let reorg_depth = current_block_number + 1 - added_blocks[0].number;
        let unmined_ops: Vec<_> = self
            .blocks
            .iter()
            .skip(self.blocks.len() - reorg_depth as usize)
            .flat_map(|block| &block.ops)
            .copied()
            .collect();

        let unmined_entity_balance_updates: Vec<_> = self
            .blocks
            .iter()
            .skip(self.blocks.len() - reorg_depth as usize)
            .flat_map(|block| &block.entity_balance_updates)
            .copied()
            .collect();

        let is_reorg_larger_than_history = reorg_depth >= self.settings.history_size;

        for _ in 0..reorg_depth {
            self.blocks.pop_back();
        }
        self.blocks.extend(added_blocks);
        while self.blocks.len() > self.settings.history_size as usize {
            self.blocks.pop_front();
        }

        self.metrics.block_height.set(current_block_number as f64);
        if reorg_depth > 0 {
            self.metrics.reorgs_detected.increment(1);
            self.metrics.total_reorg_depth.increment(reorg_depth);
        }

        self.new_update(
            reorg_depth,
            mined_ops,
            unmined_ops,
            entity_balance_updates,
            unmined_entity_balance_updates,
            is_reorg_larger_than_history,
        )
    }

    async fn load_added_blocks_connecting_to_existing_chain(
        &self,
        current_block_number: u64,
        new_head: BlockSummary,
    ) -> anyhow::Result<VecDeque<BlockSummary>> {
        // Load blocks from last known number to current.
        let mut added_blocks = self
            .load_blocks_back_to_number_no_ops(new_head, current_block_number + 1)
            .await
            .context("chain should load blocks from last processed to latest block")?;
        ensure!(
            !added_blocks.is_empty(),
            "added blocks should never be empty"
        );
        // Continue to load blocks backwards until we connect with the known chain, if necessary.
        loop {
            let earliest_new_block = &added_blocks[0];
            if earliest_new_block.number == 0 {
                break;
            }
            let Some(presumed_parent) = self.block_with_number(earliest_new_block.number - 1)
            else {
                warn!(
                    "Reorg is deeper than chain history size ({})",
                    self.blocks.len()
                );
                break;
            };
            if presumed_parent.hash == earliest_new_block.parent_hash {
                break;
            }
            // The earliest newly loaded block's parent does not match the known
            // chain, so continue to load blocks backwards, replacing the known
            // chain, until it does.
            let block = self
                .provider
                .get_block(earliest_new_block.parent_hash.into())
                .await
                .context("should load parent block when handling reorg")?
                .context("block with parent hash of known block should exist")?;
            let block = BlockSummary::try_from_block_without_ops(
                block,
                Some(earliest_new_block.number - 1),
            )?;
            added_blocks.push_front(block);
        }
        self.load_ops_into_block_summaries(&mut added_blocks)
            .await?;
        Ok(added_blocks)
    }

    async fn fetch_block_with_retries(&self, block_hash: B256) -> Option<Block> {
        for attempt in 1..=self.settings.max_sync_retries {
            match self.provider.get_block(block_hash.into()).await {
                Ok(Some(block)) => return Some(block),
                Ok(None) => warn!(
                    "Block with hash {:?} not found. Retrying... (attempt {}/{})",
                    block_hash, attempt, self.settings.max_sync_retries
                ),
                Err(err) => warn!(
                    "Error fetching block with hash {:?}: {}. Retrying... (attempt {}/{})",
                    block_hash, err, attempt, self.settings.max_sync_retries
                ),
            }
            time::sleep(self.settings.poll_interval).await;
        }

        warn!(
            "Failed to fetch block with hash {:?} after {} attempts.",
            block_hash, self.settings.max_sync_retries
        );
        None
    }

    async fn load_blocks_back_to_number_no_ops(
        &self,
        head: BlockSummary,
        min_block_number: u64,
    ) -> anyhow::Result<VecDeque<BlockSummary>> {
        let mut blocks =
            VecDeque::with_capacity(head.number.saturating_sub(min_block_number) as usize + 1);
        blocks.push_front(head);
        while blocks[0].number > min_block_number {
            let parent_hash = blocks[0].parent_hash;
            let parent = self.fetch_block_with_retries(parent_hash).await;

            if let Some(parent) = parent {
                blocks.push_front(BlockSummary::try_from_block_without_ops(
                    parent,
                    Some(blocks[0].number - 1),
                )?);
            } else {
                bail!(
                "Unable to backtrack chain history beyond block number {} due to missing parent block.",
                blocks[0].number
            );
            }
        }
        Ok(blocks)
    }

    async fn load_ops_into_block_summaries(
        &self,
        blocks: &mut VecDeque<BlockSummary>,
    ) -> anyhow::Result<()> {
        // As when loading blocks, load op events block-by-block, specifying
        // block hash. Don't load with a single call by block number range
        // because if the network is in the middle of a reorg, then we can't
        // tell which branch we read events from.
        let future_opses = blocks
            .iter()
            .map(|block| self.load_ops_in_block_with_hash(block.hash));
        let opses = future::try_join_all(future_opses)
            .await
            .context("should load ops for new blocks")?;
        for (i, (ops, balance_updates)) in opses.into_iter().enumerate() {
            blocks[i].ops = ops;
            blocks[i].entity_balance_updates = balance_updates;
        }
        Ok(())
    }

    async fn load_ops_in_block_with_hash(
        &self,
        block_hash: B256,
    ) -> anyhow::Result<(Vec<MinedOp>, Vec<BalanceUpdate>)> {
        let _permit = self
            .load_ops_semaphore
            .acquire()
            .await
            .expect("semaphore should not be closed");

        let filter = self.filter_template.clone().at_block_hash(block_hash);
        let logs = self
            .provider
            .get_logs(&filter)
            .await
            .context("chain state should load user operation events")?;

        let mut mined_ops = vec![];
        let mut entity_balance_updates = vec![];
        for log in logs {
            match self.settings.entry_point_addresses.get(&log.address()) {
                Some(EntryPointVersion::V0_6) => {
                    Self::load_v0_6(log, &mut mined_ops, &mut entity_balance_updates)
                }
                Some(EntryPointVersion::V0_7) => {
                    Self::load_v0_7(log, &mut mined_ops, &mut entity_balance_updates)
                }
                Some(EntryPointVersion::Unspecified) | None => {
                    warn!(
                        "Log with unknown entry point address: {:?}. Ignoring.",
                        log.address()
                    );
                }
            }
        }

        Ok((mined_ops, entity_balance_updates))
    }

    fn load_v0_6(log: Log, mined_ops: &mut Vec<MinedOp>, balance_updates: &mut Vec<BalanceUpdate>) {
        let address = log.address();

        match log.topic0() {
            Some(&UserOperationEventV06::SIGNATURE_HASH) => {
                let Ok(decoded) = log.log_decode::<UserOperationEventV06>() else {
                    warn!("Failed to decode v0.6 UserOperationEvent: {:?}", log);
                    return;
                };
                let event = decoded.data();

                let paymaster = if event.paymaster.is_zero() {
                    None
                } else {
                    Some(event.paymaster)
                };
                let mined = MinedOp {
                    hash: event.userOpHash,
                    entry_point: address,
                    sender: event.sender,
                    nonce: event.nonce,
                    actual_gas_cost: event.actualGasCost,
                    paymaster,
                };
                mined_ops.push(mined);
            }
            Some(&DepositedV06::SIGNATURE_HASH) => {
                let Ok(decoded) = log.log_decode::<DepositedV06>() else {
                    warn!("Failed to decode v0.6 Deposited: {:?}", log);
                    return;
                };
                let event = decoded.data();

                let info = BalanceUpdate {
                    entrypoint: address,
                    address: event.account,
                    amount: event.totalDeposit,
                    is_addition: true,
                };
                balance_updates.push(info);
            }
            Some(&WithdrawnV06::SIGNATURE_HASH) => {
                let Ok(decoded) = log.log_decode::<WithdrawnV06>() else {
                    warn!("Failed to decode v0.6 Withdrawn: {:?}", log);
                    return;
                };
                let event = decoded.data();

                let info = BalanceUpdate {
                    entrypoint: address,
                    address: event.account,
                    amount: event.amount,
                    is_addition: false,
                };
                balance_updates.push(info);
            }
            _ => {
                warn!("Unknown event signature: {:?}", log.topic0());
            }
        }
    }

    fn load_v0_7(log: Log, mined_ops: &mut Vec<MinedOp>, balance_updates: &mut Vec<BalanceUpdate>) {
        let address = log.address();

        match log.topic0() {
            Some(&UserOperationEventV07::SIGNATURE_HASH) => {
                let Ok(decoded) = log.log_decode::<UserOperationEventV07>() else {
                    warn!("Failed to decode v0.7 UserOperationEvent: {:?}", log);
                    return;
                };
                let event = decoded.data();

                let paymaster = if event.paymaster.is_zero() {
                    None
                } else {
                    Some(event.paymaster)
                };
                let mined = MinedOp {
                    hash: event.userOpHash,
                    entry_point: address,
                    sender: event.sender,
                    nonce: event.nonce,
                    actual_gas_cost: event.actualGasCost,
                    paymaster,
                };
                mined_ops.push(mined);
            }
            Some(&DepositedV07::SIGNATURE_HASH) => {
                let Ok(decoded) = log.log_decode::<DepositedV07>() else {
                    warn!("Failed to decode v0.7 Deposited: {:?}", log);
                    return;
                };
                let event = decoded.data();

                let info = BalanceUpdate {
                    entrypoint: address,
                    address: event.account,
                    amount: event.totalDeposit,
                    is_addition: true,
                };
                balance_updates.push(info);
            }
            Some(&WithdrawnV07::SIGNATURE_HASH) => {
                let Ok(decoded) = log.log_decode::<WithdrawnV07>() else {
                    warn!("Failed to decode v0.7 Withdrawn: {:?}", log);
                    return;
                };
                let event = decoded.data();

                let info = BalanceUpdate {
                    entrypoint: address,
                    address: event.account,
                    amount: event.amount,
                    is_addition: false,
                };
                balance_updates.push(info);
            }
            _ => {
                warn!("Unknown event signature: {:?}", log.topic0());
            }
        }
    }

    fn block_with_number(&self, number: u64) -> Option<&BlockSummary> {
        let earliest_number = self.blocks.front()?.number;
        if number < earliest_number {
            return None;
        }
        self.blocks.get((number - earliest_number) as usize)
    }

    fn new_update(
        &self,
        reorg_depth: u64,
        mined_ops: Vec<MinedOp>,
        unmined_ops: Vec<MinedOp>,
        entity_balance_updates: Vec<BalanceUpdate>,
        unmined_entity_balance_updates: Vec<BalanceUpdate>,
        reorg_larger_than_history: bool,
    ) -> ChainUpdate {
        let latest_block = self
            .blocks
            .back()
            .expect("new_update should not be called when blocks is empty");
        ChainUpdate {
            latest_block_number: latest_block.number,
            latest_block_hash: latest_block.hash,
            latest_block_timestamp: latest_block.timestamp,
            earliest_remembered_block_number: self.blocks[0].number,
            reorg_depth,
            mined_ops,
            unmined_ops,
            entity_balance_updates,
            unmined_entity_balance_updates,
            reorg_larger_than_history,
        }
    }
}

impl BlockSummary {
    /// Converts a block returned from a provider into a `BlockSummary` with no
    /// ops. Takes an expected block number and returns an error if it doesn't
    /// match the block. While a provider should never return a block number
    /// that doesn't match what we expect, if the provider does return bad data
    /// it's better to catch it now than run into panics from bad indexing math
    /// later.
    fn try_from_block_without_ops(
        block: Block,
        expected_block_number: Option<u64>,
    ) -> anyhow::Result<Self> {
        if let Some(expected_block_number) = expected_block_number {
            ensure!(
                block.header.number == expected_block_number,
                "block number {} should match expected {}",
                block.header.number,
                expected_block_number
            );
        }
        Ok(Self {
            number: block.header.number,
            hash: block.header.hash,
            timestamp: block.header.timestamp.into(),
            parent_hash: block.header.parent_hash,
            ops: Vec::new(),
            entity_balance_updates: Vec::new(),
        })
    }
}

#[derive(Debug)]
pub struct DedupedOps {
    pub mined_ops: Vec<MinedOp>,
    pub unmined_ops: Vec<MinedOp>,
}

impl ChainUpdate {
    /// "Cancels out" ops that appear in both mined and unmined.
    pub fn deduped_ops(&self) -> DedupedOps {
        let mined_op_hashes: HashSet<_> = self.mined_ops.iter().map(|op| op.hash).collect();
        let unmined_op_hashes: HashSet<_> = self.unmined_ops.iter().map(|op| op.hash).collect();
        let mined_ops = self
            .mined_ops
            .iter()
            .filter(|op| !unmined_op_hashes.contains(&op.hash))
            .copied()
            .collect();
        let unmined_ops = self
            .unmined_ops
            .iter()
            .filter(|op| !mined_op_hashes.contains(&op.hash))
            .copied()
            .collect();
        DedupedOps {
            mined_ops,
            unmined_ops,
        }
    }
}

#[derive(Metrics)]
#[metrics(scope = "op_pool_chain")]
struct ChainMetrics {
    #[metric(describe = "the height of block.")]
    block_height: Gauge,
    #[metric(describe = "the count of reorg event detected.")]
    reorgs_detected: Counter,
    #[metric(describe = "the count of reorg depth.")]
    total_reorg_depth: Counter,
    #[metric(describe = "the count of sync retries.")]
    sync_retries: Counter,
    #[metric(describe = "the count of sync abanded.")]
    sync_abandoned: Counter,
}

#[cfg(test)]
mod tests {
    use std::ops::DerefMut;

    use alloy_primitives::{address, Log as PrimitiveLog, LogData};
    use parking_lot::RwLock;
    use rundler_provider::{
        BlockHeader, BlockId, FilterBlockOption, MockEvmProvider, RpcBlockHash,
    };

    use super::*;

    const HISTORY_SIZE: u64 = 3;
    const ENTRY_POINT_ADDRESS_V0_6: Address = address!("0123456789012345678901234567890123456789");
    const ENTRY_POINT_ADDRESS_V0_7: Address = address!("9876543210987654321098765432109876543210");

    #[derive(Clone, Debug)]
    struct MockBlock {
        hash: B256,
        events: Vec<MockEntryPointEvents>,
    }

    #[derive(Clone, Debug, Default)]
    struct MockEntryPointEvents {
        address: Address,
        op_hashes: Vec<B256>,
        deposit_addresses: Vec<Address>,
        withdrawal_addresses: Vec<Address>,
    }

    impl MockBlock {
        fn new(hash: B256) -> Self {
            Self {
                hash,
                events: vec![],
            }
        }

        fn add_ep(
            mut self,
            address: Address,
            op_hashes: Vec<B256>,
            deposit_addresses: Vec<Address>,
            withdrawal_addresses: Vec<Address>,
        ) -> Self {
            self.events.push(MockEntryPointEvents {
                address,
                op_hashes,
                deposit_addresses,
                withdrawal_addresses,
            });
            self
        }
    }

    #[derive(Clone, Debug)]
    struct ProviderController {
        blocks: Arc<RwLock<Vec<MockBlock>>>,
    }

    impl ProviderController {
        fn set_blocks(&self, blocks: Vec<MockBlock>) {
            *self.blocks.write() = blocks;
        }

        fn get_blocks_mut(&self) -> impl DerefMut<Target = Vec<MockBlock>> + '_ {
            self.blocks.write()
        }

        fn get_head(&self) -> Block {
            let hash = self.blocks.read().last().unwrap().hash;
            self.get_block(hash.into()).unwrap()
        }

        fn get_block(&self, id: BlockId) -> Option<Block> {
            let BlockId::Hash(RpcBlockHash {
                block_hash: hash,
                require_canonical: _,
            }) = id
            else {
                panic!("get_block only supports hash ids");
            };

            let blocks = self.blocks.read();
            let number = blocks.iter().position(|block| block.hash == hash)?;
            let parent_hash = if number > 0 {
                blocks[number - 1].hash
            } else {
                B256::ZERO
            };
            Some(Block {
                header: BlockHeader {
                    hash,
                    inner: alloy_consensus::Header {
                        parent_hash,
                        number: number as u64,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            })
        }

        fn get_logs_by_block_hash(&self, filter: &Filter, block_hash: B256) -> Vec<Log> {
            let blocks = self.blocks.read();
            let block = blocks.iter().find(|block| block.hash == block_hash);
            let Some(block) = block else {
                return vec![];
            };

            let mut joined_logs: Vec<Log> = Vec::new();

            for events in &block.events {
                if events.address == ENTRY_POINT_ADDRESS_V0_6 {
                    if filter.topics[0].matches(&UserOperationEventV06::SIGNATURE_HASH) {
                        joined_logs
                            .extend(events.op_hashes.iter().copied().map(fake_mined_log_v0_6));
                    }
                    if filter.topics[0].matches(&DepositedV06::SIGNATURE_HASH) {
                        joined_logs.extend(
                            events
                                .deposit_addresses
                                .iter()
                                .copied()
                                .map(fake_deposit_log_v0_6),
                        );
                    }
                    if filter.topics[0].matches(&WithdrawnV06::SIGNATURE_HASH) {
                        joined_logs.extend(
                            events
                                .withdrawal_addresses
                                .iter()
                                .copied()
                                .map(fake_withdrawal_log_v0_6),
                        );
                    }
                } else if events.address == ENTRY_POINT_ADDRESS_V0_7 {
                    if filter.topics[0].matches(&UserOperationEventV07::SIGNATURE_HASH) {
                        joined_logs
                            .extend(events.op_hashes.iter().copied().map(fake_mined_log_v0_7));
                    }
                    if filter.topics[0].matches(&DepositedV07::SIGNATURE_HASH) {
                        joined_logs.extend(
                            events
                                .deposit_addresses
                                .iter()
                                .copied()
                                .map(fake_deposit_log_v0_7),
                        );
                    }
                    if filter.topics[0].matches(&WithdrawnV07::SIGNATURE_HASH) {
                        joined_logs.extend(
                            events
                                .withdrawal_addresses
                                .iter()
                                .copied()
                                .map(fake_withdrawal_log_v0_7),
                        );
                    }
                } else {
                    panic!("Unknown entry point address: {:?}", events.address);
                }
            }

            joined_logs
        }
    }

    #[tokio::test]
    async fn test_initial_load() {
        let (mut chain, controller) = new_chain();
        controller.set_blocks(vec![
            MockBlock::new(hash(0)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(101), hash(102)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(1)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(103)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(2)).add_ep(ENTRY_POINT_ADDRESS_V0_6, vec![], vec![], vec![]),
            MockBlock::new(hash(3)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(104), hash(105)],
                vec![],
                vec![],
            ),
        ]);
        let update = chain.sync_to_block(controller.get_head()).await.unwrap();
        // With a history size of 3, we should get updates from all blocks except the first one.
        assert_eq!(
            update,
            ChainUpdate {
                latest_block_number: 3,
                latest_block_hash: hash(3),
                latest_block_timestamp: 0.into(),
                earliest_remembered_block_number: 1,
                reorg_depth: 0,
                mined_ops: vec![
                    fake_mined_op(103, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_op(104, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_op(105, ENTRY_POINT_ADDRESS_V0_6),
                ],
                unmined_ops: vec![],
                entity_balance_updates: vec![],
                unmined_entity_balance_updates: vec![],
                reorg_larger_than_history: false,
            }
        );
    }

    #[tokio::test]
    async fn test_simple_advance() {
        let (mut chain, controller) = new_chain();
        controller.set_blocks(vec![
            MockBlock::new(hash(0)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(101), hash(102)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(1)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(103)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(2)).add_ep(ENTRY_POINT_ADDRESS_V0_6, vec![], vec![], vec![]),
            MockBlock::new(hash(3)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(104), hash(105)],
                vec![],
                vec![],
            ),
        ]);
        chain.sync_to_block(controller.get_head()).await.unwrap();
        controller
            .get_blocks_mut()
            .push(MockBlock::new(hash(4)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(106)],
                vec![],
                vec![],
            ));
        let update = chain.sync_to_block(controller.get_head()).await.unwrap();
        assert_eq!(
            update,
            ChainUpdate {
                latest_block_number: 4,
                latest_block_hash: hash(4),
                latest_block_timestamp: 0.into(),
                earliest_remembered_block_number: 2,
                reorg_depth: 0,
                mined_ops: vec![fake_mined_op(106, ENTRY_POINT_ADDRESS_V0_6)],
                unmined_ops: vec![],
                entity_balance_updates: vec![],
                unmined_entity_balance_updates: vec![],
                reorg_larger_than_history: false,
            }
        );
    }

    #[tokio::test]
    async fn test_forward_reorg() {
        let (mut chain, controller) = new_chain();
        controller.set_blocks(vec![
            MockBlock::new(hash(0)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(100)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(1)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(101)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(2)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(102)],
                vec![Address::ZERO],
                vec![addr(1)],
            ),
        ]);
        chain.sync_to_block(controller.get_head()).await.unwrap();
        {
            // Replaces the head of the chain with three new blocks.
            let mut blocks = controller.get_blocks_mut();
            blocks.pop();
            blocks.extend([
                MockBlock::new(hash(12)).add_ep(
                    ENTRY_POINT_ADDRESS_V0_6,
                    vec![hash(112)],
                    vec![],
                    vec![],
                ),
                MockBlock::new(hash(13)).add_ep(
                    ENTRY_POINT_ADDRESS_V0_6,
                    vec![hash(113)],
                    vec![],
                    vec![],
                ),
                MockBlock::new(hash(14)).add_ep(
                    ENTRY_POINT_ADDRESS_V0_6,
                    vec![hash(114)],
                    vec![],
                    vec![addr(3)],
                ),
            ]);
        }
        let update = chain.sync_to_block(controller.get_head()).await.unwrap();
        assert_eq!(
            update,
            ChainUpdate {
                latest_block_number: 4,
                latest_block_hash: hash(14),
                latest_block_timestamp: 0.into(),
                earliest_remembered_block_number: 2,
                reorg_depth: 1,
                mined_ops: vec![
                    fake_mined_op(112, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_op(113, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_op(114, ENTRY_POINT_ADDRESS_V0_6)
                ],
                unmined_ops: vec![fake_mined_op(102, ENTRY_POINT_ADDRESS_V0_6)],
                entity_balance_updates: vec![fake_mined_balance_update(
                    addr(3),
                    0,
                    false,
                    ENTRY_POINT_ADDRESS_V0_6
                )],
                unmined_entity_balance_updates: vec![
                    fake_mined_balance_update(addr(0), 0, true, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_balance_update(addr(1), 0, false, ENTRY_POINT_ADDRESS_V0_6),
                ],
                reorg_larger_than_history: false,
            }
        );
    }

    #[tokio::test]
    async fn test_sideways_reorg() {
        let (mut chain, controller) = new_chain();
        controller.set_blocks(vec![
            MockBlock::new(hash(0)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(100)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(1)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(101)],
                vec![addr(1)],
                vec![addr(9)],
            ),
            MockBlock::new(hash(2)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(102)],
                vec![],
                vec![],
            ),
        ]);
        chain.sync_to_block(controller.get_head()).await.unwrap();
        {
            // Replaces the top two blocks with two new ones.
            let mut blocks = controller.get_blocks_mut();
            blocks.pop();
            blocks.pop();
            blocks.extend([
                MockBlock::new(hash(11)).add_ep(
                    ENTRY_POINT_ADDRESS_V0_6,
                    vec![hash(111)],
                    vec![addr(2)],
                    vec![],
                ),
                MockBlock::new(hash(12)).add_ep(
                    ENTRY_POINT_ADDRESS_V0_6,
                    vec![hash(112)],
                    vec![],
                    vec![],
                ),
            ]);
        }
        let update = chain.sync_to_block(controller.get_head()).await.unwrap();
        assert_eq!(
            update,
            ChainUpdate {
                entity_balance_updates: vec![fake_mined_balance_update(
                    addr(2),
                    0,
                    true,
                    ENTRY_POINT_ADDRESS_V0_6
                )],
                latest_block_number: 2,
                latest_block_hash: hash(12),
                latest_block_timestamp: 0.into(),
                earliest_remembered_block_number: 0,
                reorg_depth: 2,
                mined_ops: vec![
                    fake_mined_op(111, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_op(112, ENTRY_POINT_ADDRESS_V0_6)
                ],
                unmined_ops: vec![
                    fake_mined_op(101, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_op(102, ENTRY_POINT_ADDRESS_V0_6)
                ],
                unmined_entity_balance_updates: vec![
                    fake_mined_balance_update(addr(1), 0, true, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_balance_update(addr(9), 0, false, ENTRY_POINT_ADDRESS_V0_6),
                ],
                reorg_larger_than_history: false,
            }
        );
    }

    #[tokio::test]
    async fn test_backwards_reorg() {
        let (mut chain, controller) = new_chain();
        controller.set_blocks(vec![
            MockBlock::new(hash(0)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(100)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(1)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(101)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(2)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(102)],
                vec![],
                vec![],
            ),
        ]);
        chain.sync_to_block(controller.get_head()).await.unwrap();
        {
            // Replaces the top two blocks with just one new one.
            let mut blocks = controller.get_blocks_mut();
            blocks.pop();
            blocks.pop();
            blocks.push(MockBlock::new(hash(11)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(111)],
                vec![addr(1)],
                vec![],
            ));
        }
        let update = chain.sync_to_block(controller.get_head()).await.unwrap();
        assert_eq!(
            update,
            ChainUpdate {
                latest_block_number: 1,
                entity_balance_updates: vec![fake_mined_balance_update(
                    addr(1),
                    0,
                    true,
                    ENTRY_POINT_ADDRESS_V0_6
                )],
                latest_block_hash: hash(11),
                latest_block_timestamp: 0.into(),
                earliest_remembered_block_number: 0,
                reorg_depth: 2,
                mined_ops: vec![fake_mined_op(111, ENTRY_POINT_ADDRESS_V0_6)],
                unmined_ops: vec![
                    fake_mined_op(101, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_op(102, ENTRY_POINT_ADDRESS_V0_6)
                ],
                unmined_entity_balance_updates: vec![],
                reorg_larger_than_history: false,
            }
        );
    }

    #[tokio::test]
    async fn test_reorg_longer_than_history() {
        let (mut chain, controller) = new_chain();
        controller.set_blocks(vec![
            MockBlock::new(hash(0)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(100)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(1)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(101)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(2)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(102)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(3)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(103)],
                vec![],
                vec![],
            ),
        ]);
        chain.sync_to_block(controller.get_head()).await.unwrap();
        // The history has size 3, so after this update it's completely unrecognizable.
        controller.set_blocks(vec![
            MockBlock::new(hash(0)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(100)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(11)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(111)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(12)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(112)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(13)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(113)],
                vec![],
                vec![],
            ),
        ]);
        let update = chain.sync_to_block(controller.get_head()).await.unwrap();
        assert_eq!(
            update,
            ChainUpdate {
                latest_block_number: 3,
                latest_block_hash: hash(13),
                latest_block_timestamp: 0.into(),
                earliest_remembered_block_number: 1,
                reorg_depth: 3,
                mined_ops: vec![
                    fake_mined_op(111, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_op(112, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_op(113, ENTRY_POINT_ADDRESS_V0_6)
                ],
                unmined_ops: vec![
                    fake_mined_op(101, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_op(102, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_op(103, ENTRY_POINT_ADDRESS_V0_6)
                ],
                entity_balance_updates: vec![],
                unmined_entity_balance_updates: vec![],
                reorg_larger_than_history: true,
            }
        );
    }

    #[tokio::test]
    async fn test_advance_larger_than_history_size() {
        let (mut chain, controller) = new_chain();
        controller.set_blocks(vec![
            MockBlock::new(hash(0)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(100)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(1)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(101)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(2)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(102)],
                vec![],
                vec![],
            ),
        ]);
        chain.sync_to_block(controller.get_head()).await.unwrap();
        {
            let mut blocks = controller.get_blocks_mut();
            for i in 3..7 {
                blocks.push(MockBlock::new(hash(10 + i)).add_ep(
                    ENTRY_POINT_ADDRESS_V0_6,
                    vec![hash(100 + i)],
                    vec![],
                    vec![],
                ));
            }
        }
        let update = chain.sync_to_block(controller.get_head()).await.unwrap();
        assert_eq!(
            update,
            ChainUpdate {
                latest_block_number: 6,
                latest_block_hash: hash(16),
                latest_block_timestamp: 0.into(),
                earliest_remembered_block_number: 4,
                reorg_depth: 0,
                entity_balance_updates: vec![],
                unmined_entity_balance_updates: vec![],
                mined_ops: vec![
                    fake_mined_op(104, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_op(105, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_op(106, ENTRY_POINT_ADDRESS_V0_6)
                ],
                unmined_ops: vec![],
                reorg_larger_than_history: false,
            }
        );
    }

    /// This test probably only matters for running against a local chain.
    #[tokio::test]
    async fn test_latest_block_number_smaller_than_history_size() {
        let (mut chain, controller) = new_chain();
        let blocks = vec![
            MockBlock::new(hash(0)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(101), hash(102)],
                vec![],
                vec![],
            ),
            MockBlock::new(hash(1)).add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(103)],
                vec![],
                vec![],
            ),
        ];
        controller.set_blocks(blocks);
        let update = chain.sync_to_block(controller.get_head()).await.unwrap();
        assert_eq!(
            update,
            ChainUpdate {
                latest_block_number: 1,
                latest_block_hash: hash(1),
                latest_block_timestamp: 0.into(),
                earliest_remembered_block_number: 0,
                reorg_depth: 0,
                mined_ops: vec![
                    fake_mined_op(101, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_op(102, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_op(103, ENTRY_POINT_ADDRESS_V0_6),
                ],
                unmined_ops: vec![],
                entity_balance_updates: vec![],
                unmined_entity_balance_updates: vec![],
                reorg_larger_than_history: false,
            }
        );
    }

    #[tokio::test]
    async fn test_mixed_event_types() {
        let (mut chain, controller) = new_chain();
        controller.set_blocks(vec![MockBlock::new(hash(0))
            .add_ep(
                ENTRY_POINT_ADDRESS_V0_6,
                vec![hash(101), hash(102)],
                vec![addr(1), addr(2)],
                vec![addr(3), addr(4)],
            )
            .add_ep(
                ENTRY_POINT_ADDRESS_V0_7,
                vec![hash(201), hash(202)],
                vec![addr(5), addr(6)],
                vec![addr(7), addr(8)],
            )]);
        let update = chain.sync_to_block(controller.get_head()).await.unwrap();
        assert_eq!(
            update,
            ChainUpdate {
                latest_block_number: 0,
                latest_block_hash: hash(0),
                latest_block_timestamp: 0.into(),
                earliest_remembered_block_number: 0,
                reorg_depth: 0,
                mined_ops: vec![
                    fake_mined_op(101, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_op(102, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_op(201, ENTRY_POINT_ADDRESS_V0_7),
                    fake_mined_op(202, ENTRY_POINT_ADDRESS_V0_7),
                ],
                unmined_ops: vec![],
                entity_balance_updates: vec![
                    fake_mined_balance_update(addr(1), 0, true, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_balance_update(addr(2), 0, true, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_balance_update(addr(3), 0, false, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_balance_update(addr(4), 0, false, ENTRY_POINT_ADDRESS_V0_6),
                    fake_mined_balance_update(addr(5), 0, true, ENTRY_POINT_ADDRESS_V0_7),
                    fake_mined_balance_update(addr(6), 0, true, ENTRY_POINT_ADDRESS_V0_7),
                    fake_mined_balance_update(addr(7), 0, false, ENTRY_POINT_ADDRESS_V0_7),
                    fake_mined_balance_update(addr(8), 0, false, ENTRY_POINT_ADDRESS_V0_7),
                ],
                unmined_entity_balance_updates: vec![],
                reorg_larger_than_history: false,
            }
        );
    }

    fn new_chain() -> (Chain<impl EvmProvider>, ProviderController) {
        let (provider, controller) = new_mock_provider();
        let chain = Chain::new(
            Arc::new(provider),
            Settings {
                history_size: HISTORY_SIZE,
                poll_interval: Duration::from_secs(250), // Not used in tests.
                entry_point_addresses: HashMap::from([
                    (ENTRY_POINT_ADDRESS_V0_6, EntryPointVersion::V0_6),
                    (ENTRY_POINT_ADDRESS_V0_7, EntryPointVersion::V0_7),
                ]),
                max_sync_retries: 1,
            },
        );
        (chain, controller)
    }

    fn new_mock_provider() -> (impl EvmProvider, ProviderController) {
        let controller = ProviderController {
            blocks: Arc::new(RwLock::new(vec![])),
        };
        let mut provider = MockEvmProvider::new();

        provider.expect_get_block().returning({
            let controller = controller.clone();
            move |id| Ok(controller.get_block(id))
        });

        provider.expect_get_logs().returning({
            let controller = controller.clone();
            move |filter| {
                let FilterBlockOption::AtBlockHash(block_hash) = filter.block_option else {
                    panic!("mock provider only supports getLogs at specific block hashes");
                };
                Ok(controller.get_logs_by_block_hash(filter, block_hash))
            }
        });

        (provider, controller)
    }

    fn fake_mined_log_v0_6(op_hash: B256) -> Log {
        let mut log_data = LogData::default();
        log_data.set_topics_unchecked(vec![
            UserOperationEventV06::SIGNATURE_HASH,
            op_hash,
            B256::ZERO, // sender
            B256::ZERO, // paymaster
        ]);
        log_data.data = UserOperationEventV06 {
            userOpHash: op_hash,
            sender: Address::ZERO,
            paymaster: Address::ZERO,
            nonce: U256::ZERO,
            success: true,
            actualGasCost: U256::ZERO,
            actualGasUsed: U256::ZERO,
        }
        .encode_data()
        .into();

        Log {
            inner: PrimitiveLog {
                address: ENTRY_POINT_ADDRESS_V0_6,
                data: log_data,
            },
            ..Default::default()
        }
    }

    fn fake_deposit_log_v0_6(deposit_address: Address) -> Log {
        let mut log_data = LogData::default();
        log_data.set_topics_unchecked(vec![
            DepositedV06::SIGNATURE_HASH,
            deposit_address.into_word(),
        ]);
        log_data.data = DepositedV06 {
            totalDeposit: U256::ZERO,
            account: deposit_address,
        }
        .encode_data()
        .into();

        Log {
            inner: PrimitiveLog {
                address: ENTRY_POINT_ADDRESS_V0_6,
                data: log_data,
            },
            ..Default::default()
        }
    }

    fn fake_withdrawal_log_v0_6(withdrawal_address: Address) -> Log {
        let mut log_data = LogData::default();
        log_data.set_topics_unchecked(vec![
            WithdrawnV06::SIGNATURE_HASH,
            withdrawal_address.into_word(),
        ]);
        log_data.data = WithdrawnV06 {
            amount: U256::ZERO,
            account: withdrawal_address,
            withdrawAddress: Address::ZERO,
        }
        .encode_data()
        .into();

        Log {
            inner: PrimitiveLog {
                address: ENTRY_POINT_ADDRESS_V0_6,
                data: log_data,
            },
            ..Default::default()
        }
    }

    fn fake_mined_log_v0_7(op_hash: B256) -> Log {
        let mut log_data = LogData::default();
        log_data.set_topics_unchecked(vec![
            UserOperationEventV07::SIGNATURE_HASH,
            op_hash,
            B256::ZERO, // sender
            B256::ZERO, // paymaster
        ]);
        log_data.data = UserOperationEventV07 {
            userOpHash: op_hash,
            sender: Address::ZERO,
            paymaster: Address::ZERO,
            nonce: U256::ZERO,
            success: true,
            actualGasCost: U256::ZERO,
            actualGasUsed: U256::ZERO,
        }
        .encode_data()
        .into();

        Log {
            inner: PrimitiveLog {
                address: ENTRY_POINT_ADDRESS_V0_7,
                data: log_data,
            },
            ..Default::default()
        }
    }

    fn fake_deposit_log_v0_7(deposit_address: Address) -> Log {
        let mut log_data = LogData::default();
        log_data.set_topics_unchecked(vec![
            DepositedV07::SIGNATURE_HASH,
            deposit_address.into_word(),
        ]);
        log_data.data = DepositedV07 {
            totalDeposit: U256::ZERO,
            account: deposit_address,
        }
        .encode_data()
        .into();

        Log {
            inner: PrimitiveLog {
                address: ENTRY_POINT_ADDRESS_V0_7,
                data: log_data,
            },
            ..Default::default()
        }
    }

    fn fake_withdrawal_log_v0_7(withdrawal_address: Address) -> Log {
        let mut log_data = LogData::default();
        log_data.set_topics_unchecked(vec![
            WithdrawnV07::SIGNATURE_HASH,
            withdrawal_address.into_word(),
        ]);
        log_data.data = WithdrawnV06 {
            amount: U256::ZERO,
            account: withdrawal_address,
            withdrawAddress: Address::ZERO,
        }
        .encode_data()
        .into();

        Log {
            inner: PrimitiveLog {
                address: ENTRY_POINT_ADDRESS_V0_7,
                data: log_data,
            },
            ..Default::default()
        }
    }

    fn fake_mined_op(n: u8, ep: Address) -> MinedOp {
        MinedOp {
            hash: hash(n),
            entry_point: ep,
            sender: Address::ZERO,
            nonce: U256::ZERO,
            actual_gas_cost: U256::ZERO,
            paymaster: None,
        }
    }

    fn fake_mined_balance_update(
        address: Address,
        amount: u128,
        is_addition: bool,
        ep: Address,
    ) -> BalanceUpdate {
        BalanceUpdate {
            address,
            entrypoint: ep,
            amount: U256::from(amount),
            is_addition,
        }
    }

    // Helper that makes fake hashes.
    fn hash(n: u8) -> B256 {
        let mut hash = B256::ZERO;
        hash.0[0] = n;
        hash
    }

    // Helper that makes fake addresses.
    fn addr(n: u8) -> Address {
        let mut address = Address::ZERO;
        address.0[0] = n;
        address
    }
}