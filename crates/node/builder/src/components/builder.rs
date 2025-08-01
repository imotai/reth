//! A generic [`NodeComponentsBuilder`]

use crate::{
    components::{
        Components, ConsensusBuilder, ExecutorBuilder, NetworkBuilder, NodeComponents,
        PayloadServiceBuilder, PoolBuilder,
    },
    BuilderContext, ConfigureEvm, FullNodeTypes,
};
use reth_consensus::{noop::NoopConsensus, ConsensusError, FullConsensus};
use reth_network::{types::NetPrimitivesFor, EthNetworkPrimitives, NetworkPrimitives};
use reth_network_api::{noop::NoopNetwork, FullNetwork};
use reth_node_api::{BlockTy, BodyTy, HeaderTy, NodeTypes, PrimitivesTy, ReceiptTy, TxTy};
use reth_payload_builder::PayloadBuilderHandle;
use reth_transaction_pool::{
    noop::NoopTransactionPool, EthPoolTransaction, EthPooledTransaction, PoolPooledTx,
    PoolTransaction, TransactionPool,
};
use std::{future::Future, marker::PhantomData};

/// A generic, general purpose and customizable [`NodeComponentsBuilder`] implementation.
///
/// This type is stateful and captures the configuration of the node's components.
///
/// ## Component dependencies:
///
/// The components of the node depend on each other:
/// - The payload builder service depends on the transaction pool.
/// - The network depends on the transaction pool.
///
/// We distinguish between different kind of components:
/// - Components that are standalone, such as the transaction pool.
/// - Components that are spawned as a service, such as the payload builder service or the network.
///
/// ## Builder lifecycle:
///
/// First all standalone components are built. Then the service components are spawned.
/// All component builders are captured in the builder state and will be consumed once the node is
/// launched.
#[derive(Debug)]
pub struct ComponentsBuilder<Node, PoolB, PayloadB, NetworkB, ExecB, ConsB> {
    pool_builder: PoolB,
    payload_builder: PayloadB,
    network_builder: NetworkB,
    executor_builder: ExecB,
    consensus_builder: ConsB,
    _marker: PhantomData<Node>,
}

impl<Node, PoolB, PayloadB, NetworkB, ExecB, ConsB>
    ComponentsBuilder<Node, PoolB, PayloadB, NetworkB, ExecB, ConsB>
{
    /// Configures the node types.
    pub fn node_types<Types>(
        self,
    ) -> ComponentsBuilder<Types, PoolB, PayloadB, NetworkB, ExecB, ConsB>
    where
        Types: FullNodeTypes,
    {
        let Self {
            pool_builder,
            payload_builder,
            network_builder,
            executor_builder: evm_builder,
            consensus_builder,
            _marker,
        } = self;
        ComponentsBuilder {
            executor_builder: evm_builder,
            pool_builder,
            payload_builder,
            network_builder,
            consensus_builder,
            _marker: Default::default(),
        }
    }

    /// Apply a function to the pool builder.
    pub fn map_pool(self, f: impl FnOnce(PoolB) -> PoolB) -> Self {
        Self {
            pool_builder: f(self.pool_builder),
            payload_builder: self.payload_builder,
            network_builder: self.network_builder,
            executor_builder: self.executor_builder,
            consensus_builder: self.consensus_builder,
            _marker: self._marker,
        }
    }

    /// Apply a function to the payload builder.
    pub fn map_payload(self, f: impl FnOnce(PayloadB) -> PayloadB) -> Self {
        Self {
            pool_builder: self.pool_builder,
            payload_builder: f(self.payload_builder),
            network_builder: self.network_builder,
            executor_builder: self.executor_builder,
            consensus_builder: self.consensus_builder,
            _marker: self._marker,
        }
    }

    /// Apply a function to the network builder.
    pub fn map_network(self, f: impl FnOnce(NetworkB) -> NetworkB) -> Self {
        Self {
            pool_builder: self.pool_builder,
            payload_builder: self.payload_builder,
            network_builder: f(self.network_builder),
            executor_builder: self.executor_builder,
            consensus_builder: self.consensus_builder,
            _marker: self._marker,
        }
    }

    /// Apply a function to the executor builder.
    pub fn map_executor(self, f: impl FnOnce(ExecB) -> ExecB) -> Self {
        Self {
            pool_builder: self.pool_builder,
            payload_builder: self.payload_builder,
            network_builder: self.network_builder,
            executor_builder: f(self.executor_builder),
            consensus_builder: self.consensus_builder,
            _marker: self._marker,
        }
    }

    /// Apply a function to the consensus builder.
    pub fn map_consensus(self, f: impl FnOnce(ConsB) -> ConsB) -> Self {
        Self {
            pool_builder: self.pool_builder,
            payload_builder: self.payload_builder,
            network_builder: self.network_builder,
            executor_builder: self.executor_builder,
            consensus_builder: f(self.consensus_builder),
            _marker: self._marker,
        }
    }
}

impl<Node, PoolB, PayloadB, NetworkB, ExecB, ConsB>
    ComponentsBuilder<Node, PoolB, PayloadB, NetworkB, ExecB, ConsB>
where
    Node: FullNodeTypes,
{
    /// Configures the pool builder.
    ///
    /// This accepts a [`PoolBuilder`] instance that will be used to create the node's transaction
    /// pool.
    pub fn pool<PB>(
        self,
        pool_builder: PB,
    ) -> ComponentsBuilder<Node, PB, PayloadB, NetworkB, ExecB, ConsB>
    where
        PB: PoolBuilder<Node>,
    {
        let Self {
            pool_builder: _,
            payload_builder,
            network_builder,
            executor_builder: evm_builder,
            consensus_builder,
            _marker,
        } = self;
        ComponentsBuilder {
            pool_builder,
            payload_builder,
            network_builder,
            executor_builder: evm_builder,
            consensus_builder,
            _marker,
        }
    }

    /// Sets [`NoopTransactionPoolBuilder`].
    pub fn noop_pool<Tx>(
        self,
    ) -> ComponentsBuilder<Node, NoopTransactionPoolBuilder<Tx>, PayloadB, NetworkB, ExecB, ConsB>
    {
        ComponentsBuilder {
            pool_builder: NoopTransactionPoolBuilder::<Tx>::default(),
            payload_builder: self.payload_builder,
            network_builder: self.network_builder,
            executor_builder: self.executor_builder,
            consensus_builder: self.consensus_builder,
            _marker: self._marker,
        }
    }
}

impl<Node, PoolB, PayloadB, NetworkB, ExecB, ConsB>
    ComponentsBuilder<Node, PoolB, PayloadB, NetworkB, ExecB, ConsB>
where
    Node: FullNodeTypes,
    PoolB: PoolBuilder<Node>,
{
    /// Configures the network builder.
    ///
    /// This accepts a [`NetworkBuilder`] instance that will be used to create the node's network
    /// stack.
    pub fn network<NB>(
        self,
        network_builder: NB,
    ) -> ComponentsBuilder<Node, PoolB, PayloadB, NB, ExecB, ConsB>
    where
        NB: NetworkBuilder<Node, PoolB::Pool>,
    {
        let Self {
            pool_builder,
            payload_builder,
            network_builder: _,
            executor_builder: evm_builder,
            consensus_builder,
            _marker,
        } = self;
        ComponentsBuilder {
            pool_builder,
            payload_builder,
            network_builder,
            executor_builder: evm_builder,
            consensus_builder,
            _marker,
        }
    }

    /// Configures the payload builder.
    ///
    /// This accepts a [`PayloadServiceBuilder`] instance that will be used to create the node's
    /// payload builder service.
    pub fn payload<PB>(
        self,
        payload_builder: PB,
    ) -> ComponentsBuilder<Node, PoolB, PB, NetworkB, ExecB, ConsB>
    where
        ExecB: ExecutorBuilder<Node>,
        PB: PayloadServiceBuilder<Node, PoolB::Pool, ExecB::EVM>,
    {
        let Self {
            pool_builder,
            payload_builder: _,
            network_builder,
            executor_builder: evm_builder,
            consensus_builder,
            _marker,
        } = self;
        ComponentsBuilder {
            pool_builder,
            payload_builder,
            network_builder,
            executor_builder: evm_builder,
            consensus_builder,
            _marker,
        }
    }

    /// Configures the executor builder.
    ///
    /// This accepts a [`ExecutorBuilder`] instance that will be used to create the node's
    /// components for execution.
    pub fn executor<EB>(
        self,
        executor_builder: EB,
    ) -> ComponentsBuilder<Node, PoolB, PayloadB, NetworkB, EB, ConsB>
    where
        EB: ExecutorBuilder<Node>,
    {
        let Self {
            pool_builder,
            payload_builder,
            network_builder,
            executor_builder: _,
            consensus_builder,
            _marker,
        } = self;
        ComponentsBuilder {
            pool_builder,
            payload_builder,
            network_builder,
            executor_builder,
            consensus_builder,
            _marker,
        }
    }

    /// Configures the consensus builder.
    ///
    /// This accepts a [`ConsensusBuilder`] instance that will be used to create the node's
    /// components for consensus.
    pub fn consensus<CB>(
        self,
        consensus_builder: CB,
    ) -> ComponentsBuilder<Node, PoolB, PayloadB, NetworkB, ExecB, CB>
    where
        CB: ConsensusBuilder<Node>,
    {
        let Self {
            pool_builder,
            payload_builder,
            network_builder,
            executor_builder,
            consensus_builder: _,

            _marker,
        } = self;
        ComponentsBuilder {
            pool_builder,
            payload_builder,
            network_builder,
            executor_builder,
            consensus_builder,
            _marker,
        }
    }

    /// Sets [`NoopNetworkBuilder`].
    pub fn noop_network<Net>(
        self,
    ) -> ComponentsBuilder<Node, PoolB, PayloadB, NoopNetworkBuilder<Net>, ExecB, ConsB> {
        ComponentsBuilder {
            pool_builder: self.pool_builder,
            payload_builder: self.payload_builder,
            network_builder: NoopNetworkBuilder::<Net>::default(),
            executor_builder: self.executor_builder,
            consensus_builder: self.consensus_builder,
            _marker: self._marker,
        }
    }

    /// Sets [`NoopPayloadBuilder`].
    pub fn noop_payload(
        self,
    ) -> ComponentsBuilder<Node, PoolB, NoopPayloadBuilder, NetworkB, ExecB, ConsB> {
        ComponentsBuilder {
            pool_builder: self.pool_builder,
            payload_builder: NoopPayloadBuilder,
            network_builder: self.network_builder,
            executor_builder: self.executor_builder,
            consensus_builder: self.consensus_builder,
            _marker: self._marker,
        }
    }

    /// Sets [`NoopConsensusBuilder`].
    pub fn noop_consensus(
        self,
    ) -> ComponentsBuilder<Node, PoolB, PayloadB, NetworkB, ExecB, NoopConsensusBuilder> {
        ComponentsBuilder {
            pool_builder: self.pool_builder,
            payload_builder: self.payload_builder,
            network_builder: self.network_builder,
            executor_builder: self.executor_builder,
            consensus_builder: NoopConsensusBuilder,
            _marker: self._marker,
        }
    }
}

impl<Node, PoolB, PayloadB, NetworkB, ExecB, ConsB> NodeComponentsBuilder<Node>
    for ComponentsBuilder<Node, PoolB, PayloadB, NetworkB, ExecB, ConsB>
where
    Node: FullNodeTypes,
    PoolB: PoolBuilder<Node, Pool: TransactionPool>,
    NetworkB: NetworkBuilder<
        Node,
        PoolB::Pool,
        Network: FullNetwork<
            Primitives: NetPrimitivesFor<
                PrimitivesTy<Node::Types>,
                PooledTransaction = PoolPooledTx<PoolB::Pool>,
            >,
        >,
    >,
    PayloadB: PayloadServiceBuilder<Node, PoolB::Pool, ExecB::EVM>,
    ExecB: ExecutorBuilder<Node>,
    ConsB: ConsensusBuilder<Node>,
{
    type Components =
        Components<Node, NetworkB::Network, PoolB::Pool, ExecB::EVM, ConsB::Consensus>;

    async fn build_components(
        self,
        context: &BuilderContext<Node>,
    ) -> eyre::Result<Self::Components> {
        let Self {
            pool_builder,
            payload_builder,
            network_builder,
            executor_builder: evm_builder,
            consensus_builder,
            _marker,
        } = self;

        let evm_config = evm_builder.build_evm(context).await?;
        let pool = pool_builder.build_pool(context).await?;
        let network = network_builder.build_network(context, pool.clone()).await?;
        let payload_builder_handle = payload_builder
            .spawn_payload_builder_service(context, pool.clone(), evm_config.clone())
            .await?;
        let consensus = consensus_builder.build_consensus(context).await?;

        Ok(Components {
            transaction_pool: pool,
            evm_config,
            network,
            payload_builder_handle,
            consensus,
        })
    }
}

impl Default for ComponentsBuilder<(), (), (), (), (), ()> {
    fn default() -> Self {
        Self {
            pool_builder: (),
            payload_builder: (),
            network_builder: (),
            executor_builder: (),
            consensus_builder: (),
            _marker: Default::default(),
        }
    }
}

/// A type that configures all the customizable components of the node and knows how to build them.
///
/// Implementers of this trait are responsible for building all the components of the node: See
/// [`NodeComponents`].
///
/// The [`ComponentsBuilder`] is a generic, general purpose implementation of this trait that can be
/// used to customize certain components of the node using the builder pattern and defaults, e.g.
/// Ethereum and Optimism.
/// A type that's responsible for building the components of the node.
pub trait NodeComponentsBuilder<Node: FullNodeTypes>: Send {
    /// The components for the node with the given types
    type Components: NodeComponents<Node>;

    /// Consumes the type and returns the created components.
    fn build_components(
        self,
        ctx: &BuilderContext<Node>,
    ) -> impl Future<Output = eyre::Result<Self::Components>> + Send;
}

impl<Node, Net, F, Fut, Pool, EVM, Cons> NodeComponentsBuilder<Node> for F
where
    Net: FullNetwork<
        Primitives: NetPrimitivesFor<
            PrimitivesTy<Node::Types>,
            PooledTransaction = PoolPooledTx<Pool>,
        >,
    >,
    Node: FullNodeTypes,
    F: FnOnce(&BuilderContext<Node>) -> Fut + Send,
    Fut: Future<Output = eyre::Result<Components<Node, Net, Pool, EVM, Cons>>> + Send,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
        + Unpin
        + 'static,
    EVM: ConfigureEvm<Primitives = PrimitivesTy<Node::Types>> + 'static,
    Cons:
        FullConsensus<PrimitivesTy<Node::Types>, Error = ConsensusError> + Clone + Unpin + 'static,
{
    type Components = Components<Node, Net, Pool, EVM, Cons>;

    fn build_components(
        self,
        ctx: &BuilderContext<Node>,
    ) -> impl Future<Output = eyre::Result<Self::Components>> + Send {
        self(ctx)
    }
}

/// Builds [`NoopTransactionPool`].
#[derive(Debug, Clone)]
pub struct NoopTransactionPoolBuilder<Tx = EthPooledTransaction>(PhantomData<Tx>);

impl<N, Tx> PoolBuilder<N> for NoopTransactionPoolBuilder<Tx>
where
    N: FullNodeTypes,
    Tx: EthPoolTransaction<Consensus = TxTy<N::Types>> + Unpin,
{
    type Pool = NoopTransactionPool<Tx>;

    async fn build_pool(self, _ctx: &BuilderContext<N>) -> eyre::Result<Self::Pool> {
        Ok(NoopTransactionPool::<Tx>::new())
    }
}

impl<Tx> Default for NoopTransactionPoolBuilder<Tx> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

/// Builds [`NoopNetwork`].
#[derive(Debug, Clone)]
pub struct NoopNetworkBuilder<Net = EthNetworkPrimitives>(PhantomData<Net>);

impl<N, Pool, Net> NetworkBuilder<N, Pool> for NoopNetworkBuilder<Net>
where
    N: FullNodeTypes,
    Pool: TransactionPool,
    Net: NetworkPrimitives<
        BlockHeader = HeaderTy<N::Types>,
        BlockBody = BodyTy<N::Types>,
        Block = BlockTy<N::Types>,
        Receipt = ReceiptTy<N::Types>,
    >,
{
    type Network = NoopNetwork<Net>;

    async fn build_network(
        self,
        _ctx: &BuilderContext<N>,
        _pool: Pool,
    ) -> eyre::Result<Self::Network> {
        Ok(NoopNetwork::new())
    }
}

impl<Net> Default for NoopNetworkBuilder<Net> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

/// Builds [`NoopConsensus`].
#[derive(Debug, Clone, Default)]
pub struct NoopConsensusBuilder;

impl<N> ConsensusBuilder<N> for NoopConsensusBuilder
where
    N: FullNodeTypes,
{
    type Consensus = NoopConsensus;

    async fn build_consensus(self, _ctx: &BuilderContext<N>) -> eyre::Result<Self::Consensus> {
        Ok(NoopConsensus::default())
    }
}

/// Builds [`PayloadBuilderHandle::noop`].
#[derive(Debug, Clone, Default)]
pub struct NoopPayloadBuilder;

impl<N, Pool, EVM> PayloadServiceBuilder<N, Pool, EVM> for NoopPayloadBuilder
where
    N: FullNodeTypes,
    Pool: TransactionPool,
    EVM: ConfigureEvm<Primitives = PrimitivesTy<N::Types>> + 'static,
{
    async fn spawn_payload_builder_service(
        self,
        _ctx: &BuilderContext<N>,
        _pool: Pool,
        _evm_config: EVM,
    ) -> eyre::Result<PayloadBuilderHandle<<N::Types as NodeTypes>::Payload>> {
        Ok(PayloadBuilderHandle::<<N::Types as NodeTypes>::Payload>::noop())
    }
}
