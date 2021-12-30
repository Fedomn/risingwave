use std::collections::HashMap;
use std::convert::TryFrom;
use std::sync::{Arc, Mutex, MutexGuard};

use async_std::net::SocketAddr;
use futures::channel::mpsc::{channel, unbounded, Receiver, Sender, UnboundedSender};
use itertools::Itertools;
use risingwave_common::catalog::{Field, Schema, TableId};
use risingwave_common::error::{ErrorCode, Result, RwError};
use risingwave_common::expr::{build_from_prost as build_expr_from_prost, AggKind};
use risingwave_common::types::build_from_prost as build_type_from_prost;
use risingwave_common::util::addr::{get_host_port, is_local_address};
use risingwave_common::util::sort_util::{
    build_from_prost as build_order_type_from_prost, fetch_orders,
};
use risingwave_pb::{expr, stream_plan, stream_service};
use risingwave_storage::hummock::HummockStateStore;
use risingwave_storage::memory::MemoryStateStore;
use risingwave_storage::object::ConnectionInfo;
use risingwave_storage::tikv::TikvStateStore;
use risingwave_storage::{Keyspace, StateStore};
use tokio::task::JoinHandle;

use crate::executor::*;
use crate::task::StreamTaskEnv;

/// Default capacity of channel if two fragments are on the same node
pub const LOCAL_OUTPUT_CHANNEL_SIZE: usize = 16;

pub type ConsumableChannelPair = (Option<Sender<Message>>, Option<Receiver<Message>>);
pub type ConsumableChannelVecPair = (Vec<Sender<Message>>, Vec<Receiver<Message>>);
pub type UpDownFragmentIds = (u32, u32);

#[derive(Clone)]
pub enum StateStoreImpl {
    HummockStateStore(HummockStateStore),
    MemoryStateStore(MemoryStateStore),
    TikvStateStore(TikvStateStore),
}

#[macro_export]
macro_rules! dispatch_state_store {
    ($impl:expr, $store:ident, $body:tt) => {
        match $impl {
            StateStoreImpl::MemoryStateStore($store) => $body,
            StateStoreImpl::HummockStateStore($store) => $body,
            StateStoreImpl::TikvStateStore($store) => $body,
        }
    };
}

/// Stores the data which may be modified from the data plane.
pub struct SharedContext {
    /// Stores the senders and receivers for later `Processor`'s use.
    ///
    /// Each actor has several senders and several receivers. Senders and receivers are created
    /// during `update_fragment`. Upon `build_fragment`, all these channels will be taken out and
    /// built into the executors and outputs.
    /// One sender or one receiver can be uniquely determined by the upstream and downstream
    /// fragment id.
    ///
    /// There are three cases that we need local channels to pass around messages:
    /// 1. pass `Message` between two local fragments
    /// 2. The RPC client at the downstream fragment forwards received `Message` to one channel in
    /// `ReceiverExecutor` or `MergerExecutor`.
    /// 3. The RPC `Output` at the upstream fragment forwards received `Message` to
    /// `ExchangeServiceImpl`.    The channel servers as a buffer because `ExchangeServiceImpl` is
    /// on the server-side and we will    also introduce backpressure.
    pub(crate) channel_pool: Mutex<HashMap<UpDownFragmentIds, ConsumableChannelPair>>,

    /// The receiver is on the other side of rpc `Output`. The `ExchangeServiceImpl` take it
    /// when it receives request for streaming data from downstream clients.
    pub(crate) receivers_for_exchange_service: Mutex<HashMap<UpDownFragmentIds, Receiver<Message>>>,

    /// Stores the local address
    ///
    /// It is used to test whether an actor is local or not,
    /// thus determining whether we should setup channel or rpc connection between
    /// two actors/fragments.
    pub(crate) addr: SocketAddr,
}

impl SharedContext {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            channel_pool: Mutex::new(HashMap::new()),
            receivers_for_exchange_service: Mutex::new(HashMap::new()),
            addr,
        }
    }
    #[inline]
    pub fn lock_channel_pool(
        &self,
    ) -> MutexGuard<HashMap<UpDownFragmentIds, ConsumableChannelPair>> {
        self.channel_pool.lock().unwrap()
    }

    #[inline]
    pub fn lock_receivers_for_exchange_service(
        &self,
    ) -> MutexGuard<HashMap<UpDownFragmentIds, Receiver<Message>>> {
        self.receivers_for_exchange_service.lock().unwrap()
    }
}

pub struct StreamManagerCore {
    /// Each processor runs in a future. Upon receiving a `Terminate` message, they will exit.
    /// `handles` store join handles of these futures, and therefore we could wait their
    /// termination.
    handles: HashMap<u32, JoinHandle<Result<()>>>,

    context: Arc<SharedContext>,

    /// Stores all actor information.
    actors: HashMap<u32, stream_service::ActorInfo>,

    /// Stores all fragment information.
    fragments: HashMap<u32, stream_plan::StreamFragment>,

    sender_placeholder: Vec<UnboundedSender<Message>>,

    /// Mock source, `fragment_id = 0`.
    /// TODO: remove this
    mock_source: ConsumableChannelPair,

    /// The state store of Hummuck
    state_store: StateStoreImpl,

    /// Next executor id for mock purpose.
    // TODO: this should be replaced with real id from frontend
    next_mock_executor_id: u32,
}

/// `StreamManager` manages all stream executors in this project.
pub struct StreamManager {
    core: Mutex<StreamManagerCore>,
}

impl StreamManager {
    pub fn new(addr: SocketAddr, state_store: Option<&str>) -> Self {
        StreamManager {
            core: Mutex::new(StreamManagerCore::new(addr, state_store)),
        }
    }

    // TODO: We will refine this method when the meta service is ready.
    pub fn send_barrier(&self, epoch: u64) {
        let core = self.core.lock().unwrap();
        core.send_barrier(epoch);
    }

    // TODO: We will refine this method when the meta service is ready.
    pub fn send_stop_barrier(&self) {
        let core = self.core.lock().unwrap();
        core.send_stop_barrier()
    }

    pub fn drop_fragment(&self, fragments: &[u32]) -> Result<()> {
        let mut core = self.core.lock().unwrap();
        for id in fragments {
            core.drop_fragment(*id);
        }
        Ok(())
    }

    pub fn take_receiver(&self, ids: UpDownFragmentIds) -> Result<Receiver<Message>> {
        let core = self.core.lock().unwrap();
        let mut guard = core.context.lock_receivers_for_exchange_service();
        guard.remove(&ids).ok_or_else(|| {
            RwError::from(ErrorCode::InternalError(format!(
                "No receivers for rpc output from {} to {}",
                ids.0, ids.1
            )))
        })
    }

    pub fn update_fragment(&self, fragments: &[stream_plan::StreamFragment]) -> Result<()> {
        let mut core = self.core.lock().unwrap();
        core.update_fragment(fragments)
    }

    /// This function was called while [`StreamManager`] exited.
    ///
    /// Suspend was allowed here.
    pub async fn wait_all(self) -> Result<()> {
        let mut core = self.core.lock().unwrap();
        core.wait_all().await
    }

    #[cfg(test)]
    pub async fn wait_actors(&self, actor_ids: &[u32]) -> Result<()> {
        let handles = self
            .core
            .lock()
            .unwrap()
            .remove_actor_handles(actor_ids)
            .await?;
        for handle in handles {
            handle.await.unwrap()?
        }
        Ok(())
    }

    /// This function could only be called once during the lifecycle of `StreamManager` for now.
    pub fn update_actor_info(
        &self,
        req: stream_service::BroadcastActorInfoTableRequest,
    ) -> Result<()> {
        let mut core = self.core.lock().unwrap();
        core.update_actor_info(req)
    }

    /// This function could only be called once during the lifecycle of `StreamManager` for now.
    pub fn build_fragment(&self, fragments: &[u32], env: StreamTaskEnv) -> Result<()> {
        let mut core = self.core.lock().unwrap();
        core.build_fragment(fragments, env)
    }

    #[cfg(test)]
    pub fn take_source(&self) -> Sender<Message> {
        let mut core = self.core.lock().unwrap();
        core.mock_source.0.take().unwrap()
    }

    #[cfg(test)]
    pub fn take_sink(&self, ids: UpDownFragmentIds) -> Receiver<Message> {
        let core = self.core.lock().unwrap();
        let mut guard = core.context.lock_channel_pool();
        guard.get_mut(&ids).unwrap().1.take().unwrap()
    }
}

fn build_agg_call_from_prost(agg_call_proto: &expr::AggCall) -> Result<AggCall> {
    let args = {
        let args = &agg_call_proto.get_args()[..];
        match args {
            [] => AggArgs::None,
            [arg] => AggArgs::Unary(
                build_type_from_prost(arg.get_type())?,
                arg.get_input().column_idx as usize,
            ),
            _ => {
                return Err(RwError::from(ErrorCode::NotImplementedError(
                    "multiple aggregation args".to_string(),
                )))
            }
        }
    };
    Ok(AggCall {
        kind: AggKind::try_from(agg_call_proto.get_type())?,
        args,
        return_type: build_type_from_prost(agg_call_proto.get_return_type())?,
    })
}

impl StreamManagerCore {
    fn get_state_store_impl(state_store: Option<&str>) -> StateStoreImpl {
        match state_store {
            Some("in_memory") | Some("in-memory") | None => {
                StateStoreImpl::MemoryStateStore(MemoryStateStore::new())
            }
            Some(tikv) if tikv.starts_with("tikv") => {
                StateStoreImpl::TikvStateStore(TikvStateStore::new(vec![tikv
                    .strip_prefix("tikv://")
                    .unwrap()
                    .to_string()]))
            }

            Some(minio) if minio.starts_with("hummock+minio://") => {
                use risingwave_pb::hummock::checksum::Algorithm as ChecksumAlg;
                use risingwave_storage::hummock::{HummockOptions, HummockStorage};
                use risingwave_storage::object::S3ObjectStore;
                // TODO: initialize those settings in a yaml file or command line instead of
                // hard-coding (#2165).
                StateStoreImpl::HummockStateStore(HummockStateStore::new(HummockStorage::new(
                    Arc::new(S3ObjectStore::new_with_minio(
                        minio.strip_prefix("hummock+").unwrap(),
                    )),
                    HummockOptions {
                        table_size: 256 * (1 << 20),
                        block_size: 64 * (1 << 10),
                        bloom_false_positive: 0.1,
                        remote_dir: "hummock_001".to_string(),
                        checksum_algo: ChecksumAlg::Crc32c,
                        stats_enabled: false,
                    },
                    None,
                )))
            }
            Some(s3) if s3.starts_with("hummock+s3://") => {
                use risingwave_pb::hummock::checksum::Algorithm as ChecksumAlg;
                use risingwave_storage::hummock::{HummockOptions, HummockStorage};
                use risingwave_storage::object::S3ObjectStore;
                let s3_test_conn_info = ConnectionInfo::new();
                let s3_store = S3ObjectStore::new(
                    s3_test_conn_info,
                    s3.strip_prefix("hummock+s3://").unwrap().to_string(),
                );
                StateStoreImpl::HummockStateStore(HummockStateStore::new(HummockStorage::new(
                    Arc::new(s3_store),
                    HummockOptions {
                        table_size: 256 * (1 << 20),
                        block_size: 64 * (1 << 10),
                        bloom_false_positive: 0.1,
                        remote_dir: "hummock_001".to_string(),
                        checksum_algo: ChecksumAlg::Crc32c,
                        stats_enabled: true,
                    },
                    None,
                )))
            }
            Some(other) => unimplemented!("{} state store is not supported", other),
        }
    }

    fn new(addr: SocketAddr, state_store: Option<&str>) -> Self {
        let (tx, rx) = channel(LOCAL_OUTPUT_CHANNEL_SIZE);
        let state_store = Self::get_state_store_impl(state_store);
        Self {
            handles: HashMap::new(),
            context: Arc::new(SharedContext::new(addr)),
            actors: HashMap::new(),
            fragments: HashMap::new(),
            sender_placeholder: vec![],
            mock_source: (Some(tx), Some(rx)),
            state_store,
            next_mock_executor_id: 0,
        }
    }

    /// Create dispatchers with downstream information registered before
    fn create_dispatcher(
        &mut self,
        input: Box<dyn Executor>,
        dispatcher: &stream_plan::Dispatcher,
        fragment_id: u32,
        downstreams: &[u32],
    ) -> Result<Box<dyn StreamConsumer>> {
        // create downstream receivers
        let outputs = downstreams
            .iter()
            .map(|down_id| {
                let up_down_ids = (fragment_id, *down_id);
                let host_addr = self
                    .actors
                    .get(down_id)
                    .ok_or_else(|| {
                        RwError::from(ErrorCode::InternalError(format!(
                            "channel between {} and {} does not exist",
                            fragment_id, down_id
                        )))
                    })?
                    .get_host();
                let downstream_addr = format!("{}:{}", host_addr.get_host(), host_addr.get_port());
                if is_local_address(&get_host_port(&downstream_addr)?, &self.context.addr) {
                    // if this is a local downstream fragment
                    let mut guard = self.context.lock_channel_pool();
                    let tx = guard
                        .get_mut(&(fragment_id, *down_id))
                        .ok_or_else(|| {
                            RwError::from(ErrorCode::InternalError(format!(
                                "channel between {} and {} does not exist",
                                fragment_id, down_id
                            )))
                        })?
                        .0
                        .take()
                        .ok_or_else(|| {
                            RwError::from(ErrorCode::InternalError(format!(
                                "sender from {} to {} does no exist",
                                fragment_id, down_id
                            )))
                        })?;
                    Ok(Box::new(ChannelOutput::new(tx)) as Box<dyn Output>)
                } else {
                    // This channel is used for `RpcOutput` and `ExchangeServiceImpl`.
                    let (tx, rx) = channel(LOCAL_OUTPUT_CHANNEL_SIZE);
                    // later, `ExchangeServiceImpl` comes to get it
                    let mut guard = self.context.lock_receivers_for_exchange_service();

                    guard.insert(up_down_ids, rx);
                    Ok(Box::new(RemoteOutput::new(tx)) as Box<dyn Output>)
                }
            })
            .collect::<Result<Vec<_>>>()?;

        assert_eq!(downstreams.len(), outputs.len());

        use stream_plan::dispatcher::DispatcherType::*;
        let dispatcher: Box<dyn StreamConsumer> = match dispatcher.get_type() {
            Hash => {
                assert!(!outputs.is_empty());
                Box::new(DispatchExecutor::new(
                    input,
                    HashDataDispatcher::new(outputs, vec![dispatcher.get_column_idx() as usize]),
                    fragment_id,
                    self.context.clone(),
                ))
            }
            Broadcast => Box::new(DispatchExecutor::new(
                input,
                BroadcastDispatcher::new(outputs),
                fragment_id,
                self.context.clone(),
            )),
            Simple => {
                assert_eq!(outputs.len(), 1);
                let output = outputs.into_iter().next().unwrap();
                Box::new(DispatchExecutor::new(
                    input,
                    SimpleDispatcher::new(output),
                    fragment_id,
                    self.context.clone(),
                ))
            }
        };
        Ok(dispatcher)
    }

    /// broadcast a barrier to all senders with specific epoch.
    // TODO: We will refine this method when the meta service is ready.
    fn send_barrier(&self, epoch: u64) {
        for sender in &self.sender_placeholder {
            sender
                .unbounded_send(Message::Barrier(Barrier {
                    epoch,
                    ..Barrier::default()
                }))
                .unwrap();
        }
    }

    // TODO: We will refine this method when the meta service is ready.
    fn send_stop_barrier(&self) {
        for sender in &self.sender_placeholder {
            sender
                .unbounded_send(Message::Barrier(Barrier {
                    epoch: 0,
                    mutation: Mutation::Stop,
                }))
                .unwrap();
        }
    }

    /// Create a chain(tree) of nodes, with given `store`.
    fn create_nodes_inner(
        &mut self,
        fragment_id: u32,
        node: &stream_plan::StreamNode,
        env: StreamTaskEnv,
        store: impl StateStore,
    ) -> Result<Box<dyn Executor>> {
        use stream_plan::stream_node::Node::*;

        // Create the input executor before creating itself
        // The node with no input must be a `MergeNode`
        let mut input: Vec<Box<dyn Executor>> = node
            .input
            .iter()
            .map(|input| self.create_nodes_inner(fragment_id, input, env.clone(), store.clone()))
            .collect::<Result<Vec<_>>>()?;

        let table_manager = env.table_manager();
        let source_manager = env.source_manager();

        let pk_indices = node
            .get_pk_indices()
            .iter()
            .map(|idx| *idx as usize)
            .collect::<Vec<_>>();

        let executor: Result<Box<dyn Executor>> = match node.get_node() {
            TableSourceNode(node) => {
                let source_id = TableId::from(&node.table_ref_id);
                let source_desc = source_manager.get_source(&source_id)?;

                // TODO: The channel pair should be created by the Checkpoint manger. So this line
                // may be removed later.
                let (sender, barrier_receiver) = unbounded();
                self.sender_placeholder.push(sender);

                let column_ids = node.get_column_ids().to_vec();
                let mut fields = Vec::with_capacity(column_ids.len());
                for &column_id in &column_ids {
                    let column_desc = source_desc
                        .columns
                        .iter()
                        .find(|c| c.column_id == column_id)
                        .unwrap();
                    fields.push(Field::new(column_desc.data_type.clone()));
                }
                let schema = Schema::new(fields);

                Ok(Box::new(StreamSourceExecutor::new(
                    source_id,
                    source_desc,
                    column_ids,
                    schema,
                    pk_indices,
                    barrier_receiver,
                )?))
            }
            ProjectNode(project_node) => {
                let project_exprs = project_node
                    .get_select_list()
                    .iter()
                    .map(build_expr_from_prost)
                    .collect::<Result<Vec<_>>>()?;
                Ok(Box::new(ProjectExecutor::new(
                    input.remove(0),
                    pk_indices,
                    project_exprs,
                )))
            }
            FilterNode(filter_node) => {
                let search_condition = build_expr_from_prost(filter_node.get_search_condition())?;
                Ok(Box::new(FilterExecutor::new(
                    input.remove(0),
                    search_condition,
                )))
            }
            SimpleAggNode(aggr_node) => {
                let agg_calls: Vec<AggCall> = aggr_node
                    .get_agg_calls()
                    .iter()
                    .map(build_agg_call_from_prost)
                    .try_collect()?;

                Ok(Box::new(SimpleAggExecutor::new(
                    input.remove(0),
                    agg_calls,
                    Keyspace::executor_root(store.clone(), self.generate_mock_executor_id()),
                    pk_indices,
                )))
            }
            HashAggNode(aggr_node) => {
                let keys = aggr_node
                    .get_group_keys()
                    .iter()
                    .map(|key| key.column_idx as usize)
                    .collect::<Vec<_>>();

                let agg_calls: Vec<AggCall> = aggr_node
                    .get_agg_calls()
                    .iter()
                    .map(build_agg_call_from_prost)
                    .try_collect()?;

                Ok(Box::new(HashAggExecutor::new(
                    input.remove(0),
                    agg_calls,
                    keys,
                    Keyspace::executor_root(store.clone(), self.generate_mock_executor_id()),
                    pk_indices,
                )))
            }
            AppendOnlyTopNNode(top_n_node) => {
                let order_types = top_n_node
                    .get_order_types()
                    .iter()
                    .map(build_order_type_from_prost)
                    .collect::<Result<Vec<_>>>()?;
                assert_eq!(order_types.len(), pk_indices.len());
                let limit = if top_n_node.limit == 0 {
                    None
                } else {
                    Some(top_n_node.limit as usize)
                };
                let cache_size = Some(1024);
                let total_count = (0, 0);
                Ok(Box::new(AppendOnlyTopNExecutor::new(
                    input.remove(0),
                    order_types,
                    (top_n_node.offset as usize, limit),
                    pk_indices,
                    Keyspace::executor_root(store.clone(), self.generate_mock_executor_id()),
                    cache_size,
                    total_count,
                )))
            }
            TopNNode(top_n_node) => {
                let order_types = top_n_node
                    .get_order_types()
                    .iter()
                    .map(build_order_type_from_prost)
                    .collect::<Result<Vec<_>>>()?;
                assert_eq!(order_types.len(), pk_indices.len());
                let limit = if top_n_node.limit == 0 {
                    None
                } else {
                    Some(top_n_node.limit as usize)
                };
                let cache_size = Some(1024);
                let total_count = (0, 0, 0);
                Ok(Box::new(TopNExecutor::new(
                    input.remove(0),
                    order_types,
                    (top_n_node.offset as usize, limit),
                    pk_indices,
                    Keyspace::executor_root(store.clone(), self.generate_mock_executor_id()),
                    cache_size,
                    total_count,
                )))
            }
            HashJoinNode(hash_join_node) => {
                let source_r = input.remove(1);
                let source_l = input.remove(0);
                let params_l = JoinParams::new(
                    hash_join_node
                        .get_left_key()
                        .iter()
                        .map(|key| *key as usize)
                        .collect::<Vec<_>>(),
                );
                let params_r = JoinParams::new(
                    hash_join_node
                        .get_right_key()
                        .iter()
                        .map(|key| *key as usize)
                        .collect::<Vec<_>>(),
                );
                Ok(Box::new(HashJoinExecutor::<_, { JoinType::Inner }>::new(
                    source_l,
                    source_r,
                    params_l,
                    params_r,
                    pk_indices,
                    Keyspace::executor_root(store.clone(), self.generate_mock_executor_id()),
                )))
            }
            MviewNode(materialized_view_node) => {
                let table_id = TableId::from(&materialized_view_node.table_ref_id);
                let columns = materialized_view_node.get_column_descs();
                let pks = materialized_view_node
                    .pk_indices
                    .iter()
                    .map(|key| *key as usize)
                    .collect::<Vec<_>>();

                let column_orders = materialized_view_node.get_column_orders();
                let order_pairs = fetch_orders(column_orders).unwrap();
                let orderings = order_pairs
                    .iter()
                    .map(|order| order.order_type)
                    .collect::<Vec<_>>();

                let keyspace = Keyspace::table_root(store, &table_id);
                table_manager.create_materialized_view(
                    &table_id,
                    columns,
                    pks.clone(),
                    orderings.clone(),
                    self.state_store.clone(),
                )?;

                let executor = Box::new(MViewSinkExecutor::new(
                    input.remove(0),
                    keyspace,
                    Schema::try_from(columns)?,
                    pks,
                    orderings,
                ));
                Ok(executor)
            }
            MergeNode(merge_node) => {
                let schema = Schema::try_from(merge_node.get_input_column_descs())?;
                let upstreams = merge_node.get_upstream_fragment_id();
                self.create_merge_node(fragment_id, schema, upstreams, pk_indices)
            }
            _ => Err(RwError::from(ErrorCode::InternalError(format!(
                "unsupported node:{:?}",
                node
            )))),
        };

        executor
    }

    /// Create a chain(tree) of nodes and return the head executor.
    fn create_nodes(
        &mut self,
        fragment_id: u32,
        node: &stream_plan::StreamNode,
        env: StreamTaskEnv,
    ) -> Result<Box<dyn Executor>> {
        dispatch_state_store!(self.state_store.clone(), store, {
            self.create_nodes_inner(fragment_id, node, env, store)
        })
    }

    fn create_merge_node(
        &mut self,
        fragment_id: u32,
        schema: Schema,
        upstreams: &[u32],
        pk_indices: PkIndices,
    ) -> Result<Box<dyn Executor>> {
        assert!(!upstreams.is_empty());

        let mut rxs = upstreams
            .iter()
            .map(|up_id| {
                if *up_id == 0 {
                    Ok(self.mock_source.1.take().unwrap())
                } else {
                    let host_addr = self
                        .actors
                        .get(up_id)
                        .ok_or_else(|| {
                            RwError::from(ErrorCode::InternalError(
                                "upstream actor not found in info table".into(),
                            ))
                        })?
                        .get_host();
                    let upstream_addr =
                        format!("{}:{}", host_addr.get_host(), host_addr.get_port());
                    let upstream_socket_addr = get_host_port(&upstream_addr)?;
                    if !is_local_address(&upstream_socket_addr, &self.context.addr) {
                        // Get the sender for `RemoteInput` to forward received messages to
                        // receivers in `ReceiverExecutor` or
                        // `MergerExecutor`.
                        let mut guard = self.context.lock_channel_pool();
                        let sender = guard
                            .get_mut(&(*up_id, fragment_id))
                            .ok_or_else(|| {
                                RwError::from(ErrorCode::InternalError(format!(
                                    "channel between {} and {} does not exist",
                                    up_id, fragment_id
                                )))
                            })?
                            .0
                            .take()
                            .ok_or_else(|| {
                                RwError::from(ErrorCode::InternalError(format!(
                                    "sender from {} to {} does no exist",
                                    up_id, fragment_id
                                )))
                            })?;
                        // spawn the `RemoteInput`
                        let up_id = *up_id;
                        tokio::spawn(async move {
                            let remote_input_res = RemoteInput::create(
                                upstream_socket_addr,
                                (up_id, fragment_id),
                                sender,
                            )
                            .await;
                            match remote_input_res {
                                Ok(mut remote_input) => remote_input.run().await,
                                Err(e) => {
                                    info!("Spawn remote input fails:{}", e);
                                }
                            }
                        });
                    }
                    let mut guard = self.context.lock_channel_pool();
                    Ok(guard
                        .get_mut(&(*up_id, fragment_id))
                        .ok_or_else(|| {
                            RwError::from(ErrorCode::InternalError(format!(
                                "channel between {} and {} does not exist",
                                up_id, fragment_id
                            )))
                        })?
                        .1
                        .take()
                        .ok_or_else(|| {
                            RwError::from(ErrorCode::InternalError(format!(
                                "receiver from {} to {} does no exist",
                                up_id, fragment_id
                            )))
                        })?)
                }
            })
            .collect::<Result<Vec<_>>>()?;

        assert_eq!(
            rxs.len(),
            upstreams.len(),
            "upstreams are not fully available: {} registered while {} required, fragment_id={}",
            rxs.len(),
            upstreams.len(),
            fragment_id
        );

        if upstreams.len() == 1 {
            // Only one upstream, use `ReceiverExecutor`.
            // FIXME: after merger is refactored in proto, put pk_indices into it.
            Ok(Box::new(ReceiverExecutor::new(
                schema,
                pk_indices,
                rxs.remove(0),
            )))
        } else {
            Ok(Box::new(MergeExecutor::new(schema, pk_indices, rxs)))
        }
    }

    fn build_fragment(&mut self, fragments: &[u32], env: StreamTaskEnv) -> Result<()> {
        for fragment_id in fragments {
            let fragment = self.fragments.remove(fragment_id).unwrap();
            let executor = self.create_nodes(*fragment_id, fragment.get_nodes(), env.clone())?;
            let dispatcher = self.create_dispatcher(
                executor,
                fragment.get_dispatcher(),
                *fragment_id,
                fragment.get_downstream_fragment_id(),
            )?;

            let actor = Actor::new(dispatcher);
            self.handles.insert(*fragment_id, tokio::spawn(actor.run()));
        }

        Ok(())
    }

    pub async fn wait_all(&mut self) -> Result<()> {
        for (_sid, handle) in std::mem::take(&mut self.handles) {
            handle.await.unwrap()?;
        }
        Ok(())
    }

    pub async fn remove_actor_handles(
        &mut self,
        actor_ids: &[u32],
    ) -> Result<Vec<JoinHandle<Result<()>>>> {
        actor_ids
            .iter()
            .map(|actor_id| {
                self.handles.remove(actor_id).ok_or_else(|| {
                    RwError::from(ErrorCode::InternalError(format!(
                        "No such actor with actor id:{}",
                        actor_id
                    )))
                })
            })
            .collect::<Result<Vec<_>>>()
    }

    fn update_actor_info(
        &mut self,
        req: stream_service::BroadcastActorInfoTableRequest,
    ) -> Result<()> {
        for actor in req.get_info() {
            let ret = self.actors.insert(actor.get_fragment_id(), actor.clone());
            if ret.is_some() {
                return Err(ErrorCode::InternalError(format!(
                    "duplicated actor {}",
                    actor.get_fragment_id()
                ))
                .into());
            }
        }
        Ok(())
    }

    /// `drop_fragment` is invoked by the leader node via RPC once the stop barrier arrives at the
    ///  sink. All the actors in the fragments should stop themselves before this method is invoked.
    fn drop_fragment(&mut self, fragment_id: u32) {
        let handle = self.handles.remove(&fragment_id).unwrap();
        let mut channel_pool_guard = self.context.lock_channel_pool();
        let mut exhange_guard = self.context.lock_receivers_for_exchange_service();
        channel_pool_guard.retain(|(x, _), _| *x != fragment_id);
        exhange_guard.retain(|(x, _), _| *x != fragment_id);

        self.actors.remove(&fragment_id);
        self.fragments.remove(&fragment_id);
        // Task should have already stopped when this method is invoked.
        handle.abort();
    }

    fn update_fragment(&mut self, fragments: &[stream_plan::StreamFragment]) -> Result<()> {
        for fragment in fragments {
            let ret = self
                .fragments
                .insert(fragment.get_fragment_id(), fragment.clone());
            if ret.is_some() {
                return Err(ErrorCode::InternalError(format!(
                    "duplicated fragment {}",
                    fragment.get_fragment_id()
                ))
                .into());
            }
        }

        for (current_id, fragment) in &self.fragments {
            for downstream_id in fragment.get_downstream_fragment_id() {
                // At this time, the graph might not be complete, so we do not check if downstream
                // has `current_id` as upstream.
                let (tx, rx) = channel(LOCAL_OUTPUT_CHANNEL_SIZE);
                let up_down_ids = (*current_id, *downstream_id);
                let mut guard = self.context.lock_channel_pool();
                guard.insert(up_down_ids, (Some(tx), Some(rx)));
            }
        }
        Ok(())
    }

    fn generate_mock_executor_id(&mut self) -> u32 {
        let id = self.next_mock_executor_id;
        self.next_mock_executor_id += 1;
        id
    }
}