//! Loads a pending block from database. Helper trait for `eth_` call and trace RPC methods.

use super::{Call, LoadBlock, LoadPendingBlock, LoadState, LoadTransaction};
use crate::FromEvmError;
use alloy_consensus::BlockHeader;
use alloy_primitives::B256;
use alloy_rpc_types_eth::{BlockId, TransactionInfo};
use futures::Future;
use reth_chainspec::ChainSpecProvider;
use reth_errors::ProviderError;
use reth_evm::{
    evm::EvmFactoryExt, system_calls::SystemCaller, tracing::TracingCtx, ConfigureEvm, Database,
    Evm, EvmEnvFor, EvmFor, HaltReasonFor, InspectorFor, TxEnvFor,
};
use reth_primitives_traits::{BlockBody, Recovered, RecoveredBlock, SignedTransaction};
use reth_revm::{database::StateProviderDatabase, db::CacheDB};
use reth_rpc_eth_types::{
    cache::db::{StateCacheDb, StateCacheDbRefMutWrapper, StateProviderTraitObjWrapper},
    EthApiError,
};
use reth_storage_api::{ProviderBlock, ProviderTx};
use revm::{context_interface::result::ResultAndState, DatabaseCommit};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::sync::Arc;

/// Executes CPU heavy tasks.
pub trait Trace: LoadState<Error: FromEvmError<Self::Evm>> {
    /// Executes the [`reth_evm::EvmEnv`] against the given [Database] without committing state
    /// changes.
    #[expect(clippy::type_complexity)]
    fn inspect<DB, I>(
        &self,
        db: DB,
        evm_env: EvmEnvFor<Self::Evm>,
        tx_env: TxEnvFor<Self::Evm>,
        inspector: I,
    ) -> Result<
        (ResultAndState<HaltReasonFor<Self::Evm>>, (EvmEnvFor<Self::Evm>, TxEnvFor<Self::Evm>)),
        Self::Error,
    >
    where
        DB: Database<Error = ProviderError>,
        I: InspectorFor<Self::Evm, DB>,
    {
        let mut evm = self.evm_config().evm_with_env_and_inspector(db, evm_env.clone(), inspector);
        let res = evm.transact(tx_env.clone()).map_err(Self::Error::from_evm_err)?;
        Ok((res, (evm_env, tx_env)))
    }

    /// Executes the transaction on top of the given [`BlockId`] with a tracer configured by the
    /// config.
    ///
    /// The callback is then called with the [`TracingInspector`] and the [`ResultAndState`] after
    /// the configured [`reth_evm::EvmEnv`] was inspected.
    ///
    /// Caution: this is blocking
    fn trace_at<F, R>(
        &self,
        evm_env: EvmEnvFor<Self::Evm>,
        tx_env: TxEnvFor<Self::Evm>,
        config: TracingInspectorConfig,
        at: BlockId,
        f: F,
    ) -> Result<R, Self::Error>
    where
        Self: Call,
        F: FnOnce(
            TracingInspector,
            ResultAndState<HaltReasonFor<Self::Evm>>,
        ) -> Result<R, Self::Error>,
    {
        self.with_state_at_block(at, |state| {
            let mut db = CacheDB::new(StateProviderDatabase::new(state));
            let mut inspector = TracingInspector::new(config);
            let (res, _) = self.inspect(&mut db, evm_env, tx_env, &mut inspector)?;
            f(inspector, res)
        })
    }

    /// Same as [`trace_at`](Self::trace_at) but also provides the used database to the callback.
    ///
    /// Executes the transaction on top of the given [`BlockId`] with a tracer configured by the
    /// config.
    ///
    /// The callback is then called with the [`TracingInspector`] and the [`ResultAndState`] after
    /// the configured [`reth_evm::EvmEnv`] was inspected.
    fn spawn_trace_at_with_state<F, R>(
        &self,
        evm_env: EvmEnvFor<Self::Evm>,
        tx_env: TxEnvFor<Self::Evm>,
        config: TracingInspectorConfig,
        at: BlockId,
        f: F,
    ) -> impl Future<Output = Result<R, Self::Error>> + Send
    where
        Self: LoadPendingBlock + Call,
        F: FnOnce(
                TracingInspector,
                ResultAndState<HaltReasonFor<Self::Evm>>,
                StateCacheDb<'_>,
            ) -> Result<R, Self::Error>
            + Send
            + 'static,
        R: Send + 'static,
    {
        let this = self.clone();
        self.spawn_with_state_at_block(at, move |state| {
            let mut db = CacheDB::new(StateProviderDatabase::new(state));
            let mut inspector = TracingInspector::new(config);
            let (res, _) = this.inspect(&mut db, evm_env, tx_env, &mut inspector)?;
            f(inspector, res, db)
        })
    }

    /// Retrieves the transaction if it exists and returns its trace.
    ///
    /// Before the transaction is traced, all previous transaction in the block are applied to the
    /// state by executing them first.
    /// The callback `f` is invoked with the [`ResultAndState`] after the transaction was executed
    /// and the database that points to the beginning of the transaction.
    ///
    /// Note: Implementers should use a threadpool where blocking is allowed, such as
    /// [`BlockingTaskPool`](reth_tasks::pool::BlockingTaskPool).
    fn spawn_trace_transaction_in_block<F, R>(
        &self,
        hash: B256,
        config: TracingInspectorConfig,
        f: F,
    ) -> impl Future<Output = Result<Option<R>, Self::Error>> + Send
    where
        Self: LoadPendingBlock + LoadTransaction + Call,
        F: FnOnce(
                TransactionInfo,
                TracingInspector,
                ResultAndState<HaltReasonFor<Self::Evm>>,
                StateCacheDb<'_>,
            ) -> Result<R, Self::Error>
            + Send
            + 'static,
        R: Send + 'static,
    {
        self.spawn_trace_transaction_in_block_with_inspector(hash, TracingInspector::new(config), f)
    }

    /// Retrieves the transaction if it exists and returns its trace.
    ///
    /// Before the transaction is traced, all previous transaction in the block are applied to the
    /// state by executing them first.
    /// The callback `f` is invoked with the [`ResultAndState`] after the transaction was executed
    /// and the database that points to the beginning of the transaction.
    ///
    /// Note: Implementers should use a threadpool where blocking is allowed, such as
    /// [`BlockingTaskPool`](reth_tasks::pool::BlockingTaskPool).
    fn spawn_trace_transaction_in_block_with_inspector<Insp, F, R>(
        &self,
        hash: B256,
        mut inspector: Insp,
        f: F,
    ) -> impl Future<Output = Result<Option<R>, Self::Error>> + Send
    where
        Self: LoadPendingBlock + LoadTransaction + Call,
        F: FnOnce(
                TransactionInfo,
                Insp,
                ResultAndState<HaltReasonFor<Self::Evm>>,
                StateCacheDb<'_>,
            ) -> Result<R, Self::Error>
            + Send
            + 'static,
        Insp:
            for<'a, 'b> InspectorFor<Self::Evm, StateCacheDbRefMutWrapper<'a, 'b>> + Send + 'static,
        R: Send + 'static,
    {
        async move {
            let (transaction, block) = match self.transaction_and_block(hash).await? {
                None => return Ok(None),
                Some(res) => res,
            };
            let (tx, tx_info) = transaction.split();

            let (evm_env, _) = self.evm_env_at(block.hash().into()).await?;

            // we need to get the state of the parent block because we're essentially replaying the
            // block the transaction is included in
            let parent_block = block.parent_hash();

            let this = self.clone();
            self.spawn_with_state_at_block(parent_block.into(), move |state| {
                let mut db = CacheDB::new(StateProviderDatabase::new(state));
                let block_txs = block.transactions_recovered();

                this.apply_pre_execution_changes(&block, &mut db, &evm_env)?;

                // replay all transactions prior to the targeted transaction
                this.replay_transactions_until(&mut db, evm_env.clone(), block_txs, *tx.tx_hash())?;

                let tx_env = this.evm_config().tx_env(tx);
                let (res, _) = this.inspect(
                    StateCacheDbRefMutWrapper(&mut db),
                    evm_env,
                    tx_env,
                    &mut inspector,
                )?;
                f(tx_info, inspector, res, db)
            })
            .await
            .map(Some)
        }
    }

    /// Executes all transactions of a block up to a given index.
    ///
    /// If a `highest_index` is given, this will only execute the first `highest_index`
    /// transactions, in other words, it will stop executing transactions after the
    /// `highest_index`th transaction. If `highest_index` is `None`, all transactions
    /// are executed.
    fn trace_block_until<F, R>(
        &self,
        block_id: BlockId,
        block: Option<Arc<RecoveredBlock<ProviderBlock<Self::Provider>>>>,
        highest_index: Option<u64>,
        config: TracingInspectorConfig,
        f: F,
    ) -> impl Future<Output = Result<Option<Vec<R>>, Self::Error>> + Send
    where
        Self: LoadBlock,
        F: Fn(
                TransactionInfo,
                TracingCtx<
                    '_,
                    Recovered<&ProviderTx<Self::Provider>>,
                    EvmFor<Self::Evm, StateCacheDbRefMutWrapper<'_, '_>, TracingInspector>,
                >,
            ) -> Result<R, Self::Error>
            + Send
            + 'static,
        R: Send + 'static,
    {
        self.trace_block_until_with_inspector(
            block_id,
            block,
            highest_index,
            move || TracingInspector::new(config),
            f,
        )
    }

    /// Executes all transactions of a block.
    ///
    /// If a `highest_index` is given, this will only execute the first `highest_index`
    /// transactions, in other words, it will stop executing transactions after the
    /// `highest_index`th transaction.
    ///
    /// Note: This expect tx index to be 0-indexed, so the first transaction is at index 0.
    ///
    /// This accepts a `inspector_setup` closure that returns the inspector to be used for tracing
    /// the transactions.
    fn trace_block_until_with_inspector<Setup, Insp, F, R>(
        &self,
        block_id: BlockId,
        block: Option<Arc<RecoveredBlock<ProviderBlock<Self::Provider>>>>,
        highest_index: Option<u64>,
        mut inspector_setup: Setup,
        f: F,
    ) -> impl Future<Output = Result<Option<Vec<R>>, Self::Error>> + Send
    where
        Self: LoadBlock,
        F: Fn(
                TransactionInfo,
                TracingCtx<
                    '_,
                    Recovered<&ProviderTx<Self::Provider>>,
                    EvmFor<Self::Evm, StateCacheDbRefMutWrapper<'_, '_>, Insp>,
                >,
            ) -> Result<R, Self::Error>
            + Send
            + 'static,
        Setup: FnMut() -> Insp + Send + 'static,
        Insp: Clone + for<'a, 'b> InspectorFor<Self::Evm, StateCacheDbRefMutWrapper<'a, 'b>>,
        R: Send + 'static,
    {
        async move {
            let block = async {
                if block.is_some() {
                    return Ok(block)
                }
                self.recovered_block(block_id).await
            };

            let ((evm_env, _), block) = futures::try_join!(self.evm_env_at(block_id), block)?;

            let Some(block) = block else { return Ok(None) };

            if block.body().transactions().is_empty() {
                // nothing to trace
                return Ok(Some(Vec::new()))
            }

            // replay all transactions of the block
            self.spawn_tracing(move |this| {
                // we need to get the state of the parent block because we're replaying this block
                // on top of its parent block's state
                let state_at = block.parent_hash();
                let block_hash = block.hash();

                let block_number = evm_env.block_env.number.saturating_to();
                let base_fee = evm_env.block_env.basefee;

                // now get the state
                let state = this.state_at_block_id(state_at.into())?;
                let mut db =
                    CacheDB::new(StateProviderDatabase::new(StateProviderTraitObjWrapper(&state)));

                this.apply_pre_execution_changes(&block, &mut db, &evm_env)?;

                // prepare transactions, we do everything upfront to reduce time spent with open
                // state
                let max_transactions = highest_index.map_or_else(
                    || block.body().transaction_count(),
                    |highest| {
                        // we need + 1 because the index is 0-based
                        highest as usize + 1
                    },
                );

                let mut idx = 0;

                let results = this
                    .evm_config()
                    .evm_factory()
                    .create_tracer(StateCacheDbRefMutWrapper(&mut db), evm_env, inspector_setup())
                    .try_trace_many(block.transactions_recovered().take(max_transactions), |ctx| {
                        let tx_info = TransactionInfo {
                            hash: Some(*ctx.tx.tx_hash()),
                            index: Some(idx),
                            block_hash: Some(block_hash),
                            block_number: Some(block_number),
                            base_fee: Some(base_fee),
                        };
                        idx += 1;

                        f(tx_info, ctx)
                    })
                    .collect::<Result<_, _>>()?;

                Ok(Some(results))
            })
            .await
        }
    }

    /// Executes all transactions of a block and returns a list of callback results invoked for each
    /// transaction in the block.
    ///
    /// This
    /// 1. fetches all transactions of the block
    /// 2. configures the EVM env
    /// 3. loops over all transactions and executes them
    /// 4. calls the callback with the transaction info, the execution result, the changed state
    ///    _after_ the transaction [`StateProviderDatabase`] and the database that points to the
    ///    state right _before_ the transaction.
    fn trace_block_with<F, R>(
        &self,
        block_id: BlockId,
        block: Option<Arc<RecoveredBlock<ProviderBlock<Self::Provider>>>>,
        config: TracingInspectorConfig,
        f: F,
    ) -> impl Future<Output = Result<Option<Vec<R>>, Self::Error>> + Send
    where
        Self: LoadBlock,
        // This is the callback that's invoked for each transaction with the inspector, the result,
        // state and db
        F: Fn(
                TransactionInfo,
                TracingCtx<
                    '_,
                    Recovered<&ProviderTx<Self::Provider>>,
                    EvmFor<Self::Evm, StateCacheDbRefMutWrapper<'_, '_>, TracingInspector>,
                >,
            ) -> Result<R, Self::Error>
            + Send
            + 'static,
        R: Send + 'static,
    {
        self.trace_block_until(block_id, block, None, config, f)
    }

    /// Executes all transactions of a block and returns a list of callback results invoked for each
    /// transaction in the block.
    ///
    /// This
    /// 1. fetches all transactions of the block
    /// 2. configures the EVM env
    /// 3. loops over all transactions and executes them
    /// 4. calls the callback with the transaction info, the execution result, the changed state
    ///    _after_ the transaction `EvmState` and the database that points to the state right
    ///    _before_ the transaction, in other words the state the transaction was executed on:
    ///    `changed_state = tx(cached_state)`
    ///
    /// This accepts a `inspector_setup` closure that returns the inspector to be used for tracing
    /// a transaction. This is invoked for each transaction.
    fn trace_block_inspector<Setup, Insp, F, R>(
        &self,
        block_id: BlockId,
        block: Option<Arc<RecoveredBlock<ProviderBlock<Self::Provider>>>>,
        insp_setup: Setup,
        f: F,
    ) -> impl Future<Output = Result<Option<Vec<R>>, Self::Error>> + Send
    where
        Self: LoadBlock,
        // This is the callback that's invoked for each transaction with the inspector, the result,
        // state and db
        F: Fn(
                TransactionInfo,
                TracingCtx<
                    '_,
                    Recovered<&ProviderTx<Self::Provider>>,
                    EvmFor<Self::Evm, StateCacheDbRefMutWrapper<'_, '_>, Insp>,
                >,
            ) -> Result<R, Self::Error>
            + Send
            + 'static,
        Setup: FnMut() -> Insp + Send + 'static,
        Insp: Clone + for<'a, 'b> InspectorFor<Self::Evm, StateCacheDbRefMutWrapper<'a, 'b>>,
        R: Send + 'static,
    {
        self.trace_block_until_with_inspector(block_id, block, None, insp_setup, f)
    }

    /// Applies chain-specific state transitions required before executing a block.
    ///
    /// Note: This should only be called when tracing an entire block vs individual transactions.
    /// When tracing transaction on top of an already committed block state, those transitions are
    /// already applied.
    fn apply_pre_execution_changes<DB: Send + Database + DatabaseCommit>(
        &self,
        block: &RecoveredBlock<ProviderBlock<Self::Provider>>,
        db: &mut DB,
        evm_env: &EvmEnvFor<Self::Evm>,
    ) -> Result<(), Self::Error> {
        let mut system_caller = SystemCaller::new(self.provider().chain_spec());

        // apply relevant system calls
        let mut evm = self.evm_config().evm_with_env(db, evm_env.clone());
        system_caller.apply_pre_execution_changes(block.header(), &mut evm).map_err(|err| {
            EthApiError::EvmCustom(format!("failed to apply 4788 system call {err}"))
        })?;

        Ok(())
    }
}
