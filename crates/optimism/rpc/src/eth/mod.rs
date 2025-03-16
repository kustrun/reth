//! OP-Reth `eth_` endpoint implementation.

pub mod ext;
pub mod receipt;
pub mod transaction;

mod block;
mod call;
mod pending_block;

pub use receipt::{OpReceiptBuilder, OpReceiptFieldsBuilder};

use crate::{OpEthApiError, SequencerClient};
use alloy_consensus::BlockHeader;
use alloy_eips::BlockId;
use alloy_primitives::{Address, Bytes, U256};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use jsonrpsee_core::client::ClientT;
use op_alloy_network::Optimism;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_evm::ConfigureEvm;
use reth_network_api::NetworkInfo;
use reth_node_api::NodePrimitives;
use reth_node_builder::EthApiBuilderCtx;
use reth_optimism_forks::OpHardforks;
use reth_optimism_primitives::OpPrimitives;
use reth_primitives_traits::Block;
use reth_provider::{
    BlockNumReader, BlockReader, BlockReaderIdExt, CanonStateSubscriptions, ChainSpecProvider,
    NodePrimitivesProvider, ProviderBlock, ProviderHeader, ProviderReceipt, ProviderTx,
    StageCheckpointReader, StateProviderFactory,
};
use reth_rpc::eth::{core::EthApiInner, DevSigner};
use reth_rpc_eth_api::{
    helpers::{
        AddDevSigners, EthApiSpec, EthFees, EthSigner, EthState, LoadBlock, LoadFee, LoadState,
        SpawnBlocking, Trace,
    },
    EthApiTypes, FromEvmError, RpcNodeCore, RpcNodeCoreExt,
};
use reth_rpc_eth_types::{EthStateCache, FeeHistoryCache, GasPriceOracle};
use reth_tasks::{
    pool::{BlockingTaskGuard, BlockingTaskPool},
    TaskSpawner,
};
use reth_transaction_pool::TransactionPool;
use revm::primitives::bitvec::macros::internal::funty::Fundamental;
use std::future::Future;
use std::time::Duration;
use std::{fmt, sync::Arc};

/// Adapter for [`EthApiInner`], which holds all the data required to serve core `eth_` API.
pub type EthApiNodeBackend<N> = EthApiInner<
    <N as RpcNodeCore>::Provider,
    <N as RpcNodeCore>::Pool,
    <N as RpcNodeCore>::Network,
    <N as RpcNodeCore>::Evm,
>;

/// A helper trait with requirements for [`RpcNodeCore`] to be used in [`OpEthApi`].
pub trait OpNodeCore: RpcNodeCore<Provider: BlockReader> {}
impl<T> OpNodeCore for T where T: RpcNodeCore<Provider: BlockReader> {}

/// OP-Reth `Eth` API implementation.
///
/// This type provides the functionality for handling `eth_` related requests.
///
/// This wraps a default `Eth` implementation, and provides additional functionality where the
/// optimism spec deviates from the default (ethereum) spec, e.g. transaction forwarding to the
/// sequencer, receipts, additional RPC fields for transaction receipts.
///
/// This type implements the [`FullEthApi`](reth_rpc_eth_api::helpers::FullEthApi) by implemented
/// all the `Eth` helper traits and prerequisite traits.
#[derive(Clone)]
pub struct OpEthApi<N: OpNodeCore> {
    /// Gateway to node's core components.
    inner: Arc<OpEthApiInner<N>>,
}

impl<N> OpEthApi<N>
where
    N: OpNodeCore<
        Provider: BlockReaderIdExt
                      + ChainSpecProvider
                      + CanonStateSubscriptions<Primitives = OpPrimitives>
                      + Clone
                      + 'static,
    >,
{
    /// Returns a reference to the [`EthApiNodeBackend`].
    pub fn eth_api(&self) -> &EthApiNodeBackend<N> {
        self.inner.eth_api()
    }

    /// Returns the configured sequencer client, if any.
    pub fn sequencer_client(&self) -> Option<&SequencerClient> {
        self.inner.sequencer_client()
    }

    /// Returns the historical rpc provider, if any.
    pub fn historical_rpc_provider(&self) -> Option<&HttpClient> {
        self.inner.historical_rpc_provider()
    }

    /// Build a [`OpEthApi`] using [`OpEthApiBuilder`].
    pub const fn builder() -> OpEthApiBuilder {
        OpEthApiBuilder::new()
    }

    fn is_optimism_pre_bedrock(&self, block_id: BlockId) -> bool {
        let provider = self.eth_api().provider();
        let block_number = match block_id {
            BlockId::Hash(hash) => {
                let block = provider.block_by_hash(hash.block_hash);
                let block_number = match block {
                    Ok(Some(block)) => block.header().number().as_u64(),
                    _ => return false,
                };

                block_number
            }
            BlockId::Number(block_num_or_tag) => {
                let block_number = match block_num_or_tag.as_number() {
                    Some(block_number) => block_number,
                    None => return false,
                };

                block_number
            }
        };

        let config = self.eth_api().evm_config();
        config.is_optimism() && !config.is_bedrock_active_at_block(block_number)
    }
}

impl<N> EthApiTypes for OpEthApi<N>
where
    Self: Send + Sync,
    N: OpNodeCore,
{
    type Error = OpEthApiError;
    type NetworkTypes = Optimism;
    type TransactionCompat = Self;

    fn tx_resp_builder(&self) -> &Self::TransactionCompat {
        self
    }
}

impl<N> RpcNodeCore for OpEthApi<N>
where
    N: OpNodeCore,
{
    type Provider = N::Provider;
    type Pool = N::Pool;
    type Evm = <N as RpcNodeCore>::Evm;
    type Network = <N as RpcNodeCore>::Network;
    type PayloadBuilder = ();

    #[inline]
    fn pool(&self) -> &Self::Pool {
        self.inner.eth_api.pool()
    }

    #[inline]
    fn evm_config(&self) -> &Self::Evm {
        self.inner.eth_api.evm_config()
    }

    #[inline]
    fn network(&self) -> &Self::Network {
        self.inner.eth_api.network()
    }

    #[inline]
    fn payload_builder(&self) -> &Self::PayloadBuilder {
        &()
    }

    #[inline]
    fn provider(&self) -> &Self::Provider {
        self.inner.eth_api.provider()
    }
}

impl<N> RpcNodeCoreExt for OpEthApi<N>
where
    N: OpNodeCore,
{
    #[inline]
    fn cache(&self) -> &EthStateCache<ProviderBlock<N::Provider>, ProviderReceipt<N::Provider>> {
        self.inner.eth_api.cache()
    }
}

impl<N> EthApiSpec for OpEthApi<N>
where
    N: OpNodeCore<
        Provider: ChainSpecProvider<ChainSpec: EthereumHardforks>
                      + BlockNumReader
                      + StageCheckpointReader,
        Network: NetworkInfo,
    >,
{
    type Transaction = ProviderTx<Self::Provider>;

    #[inline]
    fn starting_block(&self) -> U256 {
        self.inner.eth_api.starting_block()
    }

    #[inline]
    fn signers(&self) -> &parking_lot::RwLock<Vec<Box<dyn EthSigner<ProviderTx<Self::Provider>>>>> {
        self.inner.eth_api.signers()
    }
}

impl<N> SpawnBlocking for OpEthApi<N>
where
    Self: Send + Sync + Clone + 'static,
    N: OpNodeCore,
{
    #[inline]
    fn io_task_spawner(&self) -> impl TaskSpawner {
        self.inner.eth_api.task_spawner()
    }

    #[inline]
    fn tracing_task_pool(&self) -> &BlockingTaskPool {
        self.inner.eth_api.blocking_task_pool()
    }

    #[inline]
    fn tracing_task_guard(&self) -> &BlockingTaskGuard {
        self.inner.eth_api.blocking_task_guard()
    }
}

impl<N> LoadFee for OpEthApi<N>
where
    Self: LoadBlock<Provider = N::Provider>,
    N: OpNodeCore<
        Provider: BlockReaderIdExt
                      + ChainSpecProvider<ChainSpec: EthChainSpec + EthereumHardforks>
                      + StateProviderFactory,
    >,
{
    #[inline]
    fn gas_oracle(&self) -> &GasPriceOracle<Self::Provider> {
        self.inner.eth_api.gas_oracle()
    }

    #[inline]
    fn fee_history_cache(&self) -> &FeeHistoryCache {
        self.inner.eth_api.fee_history_cache()
    }
}

impl<N> LoadState for OpEthApi<N> where
    N: OpNodeCore<
        Provider: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks>,
        Pool: TransactionPool,
    >
{
}

impl<N> EthState for OpEthApi<N>
where
    Self: LoadState + SpawnBlocking,
    N: OpNodeCore<
        Provider: BlockReaderIdExt
                      + CanonStateSubscriptions<Primitives = OpPrimitives>
                      + ChainSpecProvider
                      + NodePrimitivesProvider,
        Evm: EthChainSpec + OpHardforks,
    >,
{
    #[inline]
    fn max_proof_window(&self) -> u64 {
        self.inner.eth_api.eth_proof_window()
    }

    fn get_code(
        &self,
        address: Address,
        block_id: Option<BlockId>,
    ) -> impl Future<Output = Result<Bytes, Self::Error>> + Send {
        match block_id {
            Some(block_id) => {
                if self.is_optimism_pre_bedrock(block_id) {
                    if let Some(historical_rpc) = self.historical_rpc_provider() {
                        // return self.spawn_blocking_io(async move |this| {
                        //     let tmp: Result<Bytes, _> = tokio::time::timeout(
                        //         std::time::Duration::from_secs(5),
                        //         historical_rpc
                        //             .request("eth_getCode", rpc_params![address, block_id]),
                        //     )
                        //     .await
                        //     .unwrap_or_else(|_| Ok(Bytes::default()))
                        //     .and_then(|res| Ok(res));
                        //
                        //     return Ok(tmp.unwrap());
                        // });
                    } else {
                        // TODO: Return error
                    }
                }
            }
            _ => {}
        }

        LoadState::get_code(self, address, block_id)
    }
}

impl<N> EthFees for OpEthApi<N>
where
    Self: LoadFee,
    N: OpNodeCore,
{
}

impl<N> Trace for OpEthApi<N>
where
    Self: RpcNodeCore<Provider: BlockReader>
        + LoadState<
            Evm: ConfigureEvm<
                Header = ProviderHeader<Self::Provider>,
                Transaction = ProviderTx<Self::Provider>,
            >,
            Error: FromEvmError<Self::Evm>,
        >,
    N: OpNodeCore,
{
}

impl<N> AddDevSigners for OpEthApi<N>
where
    N: OpNodeCore,
{
    fn with_dev_accounts(&self) {
        *self.inner.eth_api.signers().write() = DevSigner::random_signers(20)
    }
}

impl<N: OpNodeCore> fmt::Debug for OpEthApi<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpEthApi").finish_non_exhaustive()
    }
}

/// Container type `OpEthApi`
#[allow(missing_debug_implementations)]
struct OpEthApiInner<N: OpNodeCore> {
    /// Gateway to node's core components.
    eth_api: EthApiNodeBackend<N>,
    /// Sequencer client, configured to forward submitted transactions to sequencer of given OP
    /// network.
    sequencer_client: Option<SequencerClient>,

    historical_rpc_provider: Option<HttpClient>,
}

impl<N: OpNodeCore> OpEthApiInner<N> {
    /// Returns a reference to the [`EthApiNodeBackend`].
    const fn eth_api(&self) -> &EthApiNodeBackend<N> {
        &self.eth_api
    }

    /// Returns the configured sequencer client, if any.
    const fn sequencer_client(&self) -> Option<&SequencerClient> {
        self.sequencer_client.as_ref()
    }

    /// Returns the historical rpc provider, if any.
    const fn historical_rpc_provider(&self) -> Option<&HttpClient> {
        self.historical_rpc_provider.as_ref()
    }
}

/// A type that knows how to build a [`OpEthApi`].
#[derive(Debug, Default)]
pub struct OpEthApiBuilder {
    /// Sequencer client, configured to forward submitted transactions to sequencer of given OP
    /// network.
    sequencer_client: Option<SequencerClient>,
}

impl OpEthApiBuilder {
    /// Creates a [`OpEthApiBuilder`] instance from [`EthApiBuilderCtx`].
    pub const fn new() -> Self {
        Self { sequencer_client: None }
    }

    /// With a [`SequencerClient`].
    pub fn with_sequencer(mut self, sequencer_client: Option<SequencerClient>) -> Self {
        self.sequencer_client = sequencer_client;
        self
    }
}

impl OpEthApiBuilder {
    /// Builds an instance of [`OpEthApi`]
    pub fn build<N>(self, ctx: &EthApiBuilderCtx<N>) -> OpEthApi<N>
    where
        N: OpNodeCore<
            Provider: BlockReaderIdExt<
                Block = <<N::Provider as NodePrimitivesProvider>::Primitives as NodePrimitives>::Block,
                Receipt = <<N::Provider as NodePrimitivesProvider>::Primitives as NodePrimitives>::Receipt,
            > + ChainSpecProvider
                          + CanonStateSubscriptions
                          + Clone
                          + 'static,
        >,
    {
        let blocking_task_pool =
            BlockingTaskPool::build().expect("failed to build blocking task pool");

        let eth_api = EthApiInner::new(
            ctx.provider.clone(),
            ctx.pool.clone(),
            ctx.network.clone(),
            ctx.cache.clone(),
            ctx.new_gas_price_oracle(),
            ctx.config.rpc_gas_cap,
            ctx.config.rpc_max_simulate_blocks,
            ctx.config.eth_proof_window,
            blocking_task_pool,
            ctx.new_fee_history_cache(),
            ctx.evm_config.clone(),
            Box::new(ctx.executor.clone()),
            ctx.config.proof_permits,
        );

        let historical_rpc_url = "".to_string();
        let historical_rpc_client = HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(60))
            .build(historical_rpc_url)
            .unwrap();

        OpEthApi {
            inner: Arc::new(OpEthApiInner {
                eth_api,
                sequencer_client: self.sequencer_client,
                historical_rpc_provider: Some(historical_rpc_client),
            }),
        }
    }
}
