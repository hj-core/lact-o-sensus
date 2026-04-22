use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use common::proto::v1::app::MutationIntent;
use common::proto::v1::app::MutationStatus;
use common::proto::v1::app::ProposeMutationRequest;
use common::proto::v1::app::ProposeMutationResponse;
use common::proto::v1::app::QueryStateRequest;
use common::proto::v1::app::QueryStateResponse;
use common::proto::v1::app::QueryStatus;
use common::proto::v1::app::ingress_service_client::IngressServiceClient;
use common::types::ClientId;
use common::types::ClusterId;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::SequenceId;
use rand::RngExt;
use tokio::sync::RwLock;
use tonic::Request;
use tonic::Status;
use tonic::transport::Channel;
use tonic::transport::Endpoint;

use crate::state::ClientState;
use crate::state::MAX_KNOWN_NODES;
use crate::wal::IntentWal;

/// Default timeout for mutation requests, accounting for AI Veto egress (5s)
/// and Raft consensus cycles as mandated by ADR 003.
pub const DEFAULT_MUTATION_TIMEOUT: Duration = Duration::from_secs(30);
/// Default timeout for linearizable query requests.
pub const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// Default timeout for establishing a new gRPC connection.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// Initial backoff delay for retries as mandated by ADR 003.
const INITIAL_BACKOFF: Duration = Duration::from_millis(100);
/// Maximum backoff delay for retries to prevent excessive wait times.
const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// ±20% jitter factor to disperse "thundering herd" retry waves.
const JITTER_FACTOR: f64 = 0.2;

/// Metadata header key for the cluster identifier.
pub const HEADER_CLUSTER_ID: &str = "x-cluster-id";
/// Metadata header key for the target node identifier.
pub const HEADER_TARGET_NODE_ID: &str = "x-target-node-id";

/// A resilient, high-performance client for interacting with a Lact-O-Sensus
/// cluster.
///
/// The `LactoClient` handles leader discovery, automatic redirection, and
/// ensures Exactly-Once Semantics (EOS) while minimizing synchronization
/// overhead and ensuring crash-durability via a WAL.
pub struct LactoClient {
    /// Target cluster identity for outbound header injection.
    cluster_id: ClusterId,
    /// Client session identity.
    client_id: ClientId,

    /// Configurable timeouts for different RPC classes.
    mutation_timeout: Duration,
    query_timeout: Duration,
    connect_timeout: Duration,

    /// Configurable backoff parameters for retries as mandated by ADR 003.
    initial_backoff: Duration,
    max_backoff: Duration,

    /// Persistent state including sequence IDs and the node discovery list.
    state: Arc<RwLock<ClientState>>,
    /// Durable Write-Ahead Log for mutation intents.
    wal: Arc<IntentWal>,
    /// Active gRPC channel, lazily initialized and refreshed upon redirection.
    client: RwLock<Option<IngressServiceClient<Channel>>>,
}

impl LactoClient {
    /// Creates a new `LactoClient` with default timeouts and backoff.
    pub fn new(state: ClientState, wal_path: impl AsRef<Path>) -> Result<Self> {
        Self::with_timeouts(
            state,
            wal_path,
            DEFAULT_MUTATION_TIMEOUT,
            DEFAULT_QUERY_TIMEOUT,
            DEFAULT_CONNECT_TIMEOUT,
        )
    }

    /// Creates a new `LactoClient` with explicit timeout configuration and
    /// default backoff.
    pub fn with_timeouts(
        state: ClientState,
        wal_path: impl AsRef<Path>,
        mutation_timeout: Duration,
        query_timeout: Duration,
        connect_timeout: Duration,
    ) -> Result<Self> {
        Self::with_config(
            state,
            wal_path,
            mutation_timeout,
            query_timeout,
            connect_timeout,
            INITIAL_BACKOFF,
            MAX_BACKOFF,
        )
    }

    /// Creates a new `LactoClient` with full configuration including backoff.
    /// Primarily used for zero-delay unit testing (ADR 003).
    pub fn with_config(
        state: ClientState,
        wal_path: impl AsRef<Path>,
        mutation_timeout: Duration,
        query_timeout: Duration,
        connect_timeout: Duration,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Result<Self> {
        if mutation_timeout.is_zero() || query_timeout.is_zero() || connect_timeout.is_zero() {
            anyhow::bail!("Client timeouts must be non-zero");
        }

        let cluster_id = state.cluster_id().clone();
        let client_id = state.client_id().clone();
        let wal = IntentWal::open(wal_path)?;

        Ok(Self {
            cluster_id,
            client_id,
            mutation_timeout,
            query_timeout,
            connect_timeout,
            initial_backoff,
            max_backoff,
            state: Arc::new(RwLock::new(state)),
            wal: Arc::new(wal),
            client: RwLock::new(None),
        })
    }

    /// Returns a reference to the underlying client state.
    pub fn state(&self) -> &Arc<RwLock<ClientState>> {
        &self.state
    }

    /// Returns a reference to the Write-Ahead Log.
    pub fn wal(&self) -> &Arc<IntentWal> {
        &self.wal
    }

    // --- High-Level API ---

    /// Proposes a grocery mutation to the cluster.
    ///
    /// Ensures Exactly-Once Semantics by:
    /// 1. Incrementing the sequence ID.
    /// 2. Persisting the intent to the WAL.
    /// 3. Dispatching the RPC.
    /// 4. Removing from WAL only on terminal response (COMMITTED/VETOED).
    pub async fn propose_mutation(
        &self,
        intent: MutationIntent,
    ) -> Result<ProposeMutationResponse> {
        let sequence_id = self
            .state
            .write()
            .await
            .next_sequence_id()
            .context("Failed to prepare session sequence for mutation")?;

        let request_payload = ProposeMutationRequest::new(&self.client_id, sequence_id, intent);

        // ADR 001: Persist before network egress
        self.wal.append(sequence_id, &request_payload)?;

        self.execute_mutation(sequence_id, request_payload).await
    }

    /// Re-proposes a recovered mutation intent from the WAL.
    ///
    /// This is used during the startup recovery phase to reconcile pending
    /// intents without generating new sequence IDs.
    pub async fn repropose_mutation(
        &self,
        sequence_id: SequenceId,
        payload: ProposeMutationRequest,
    ) -> Result<ProposeMutationResponse> {
        self.execute_mutation(sequence_id, payload).await
    }

    /// Orchestrates the mutation lifecycle from dispatch to WAL cleanup.
    async fn execute_mutation(
        &self,
        sequence_id: SequenceId,
        payload: ProposeMutationRequest,
    ) -> Result<ProposeMutationResponse> {
        let response = self.dispatch_mutation(payload).await?;

        // ADR 001: Only remove from WAL if the state is terminal.
        // REJECTED (redirection) is NOT terminal and will continue in the retry loop.
        match MutationStatus::try_from(response.status) {
            Ok(MutationStatus::Committed) | Ok(MutationStatus::Vetoed) => {
                self.wal.remove(sequence_id)?;
            }
            _ => {}
        }

        Ok(response)
    }

    /// Queries the current grocery ledger state.
    ///
    /// Following the linearizable read mandate, this query is directed to the
    /// current leader and will follow redirection hints if necessary.
    pub async fn query_state(
        &self,
        query_filter: Option<String>,
        min_state_version: Option<LogIndex>,
    ) -> Result<QueryStateResponse> {
        let request_payload = QueryStateRequest {
            query_filter,
            min_state_version: min_state_version.map(|v| v.value()),
        };

        self.dispatch_query(request_payload).await
    }

    // --- Private Dispatch Logic ---

    async fn dispatch_mutation(
        &self,
        payload: ProposeMutationRequest,
    ) -> Result<ProposeMutationResponse> {
        let mut retry_count = 0;
        // Retry budget: Current known nodes + a discovery buffer (MAX_KNOWN_NODES).
        let max_retries = self.state.read().await.known_nodes().len() + MAX_KNOWN_NODES;

        loop {
            if retry_count >= max_retries {
                anyhow::bail!(
                    "Mutation failed after {} attempts. Exhausted all known nodes and redirection \
                     hints.",
                    retry_count
                );
            }
            retry_count += 1;

            let mut client = self.get_or_connect().await?;

            let mut request = Request::new(payload.clone());
            request.set_timeout(self.mutation_timeout);

            // Inject identity headers for cluster isolation (ADR 004/005).
            request.metadata_mut().insert(
                HEADER_CLUSTER_ID,
                self.cluster_id.as_str().parse().map_err(|_| {
                    Status::internal("Failed to parse cluster_id for outbound header")
                })?,
            );
            if let Some(target_node_id) = self.current_node_id().await {
                request.metadata_mut().insert(
                    HEADER_TARGET_NODE_ID,
                    target_node_id.to_string().parse().map_err(|_| {
                        Status::internal("Failed to parse target_node_id for outbound header")
                    })?,
                );
            }

            let response = client
                .propose_mutation(request)
                .await
                .map(|r| r.into_inner());

            match response {
                Ok(res) => match MutationStatus::try_from(res.status) {
                    Ok(MutationStatus::Rejected) => {
                        self.reconcile_routing_failure(Some(res.leader_hint), retry_count)
                            .await?;
                        continue;
                    }
                    _ => {
                        self.record_current_node_success().await?;
                        return Ok(res);
                    }
                },
                Err(_) => {
                    self.reconcile_routing_failure(None, retry_count).await?;
                    continue;
                }
            }
        }
    }

    async fn dispatch_query(&self, payload: QueryStateRequest) -> Result<QueryStateResponse> {
        let mut retry_count = 0;
        // Retry budget: Current known nodes + a discovery buffer (MAX_KNOWN_NODES).
        let max_retries = self.state.read().await.known_nodes().len() + MAX_KNOWN_NODES;

        loop {
            if retry_count >= max_retries {
                anyhow::bail!(
                    "Query failed after {} attempts. Exhausted all known nodes and redirection \
                     hints.",
                    retry_count
                );
            }
            retry_count += 1;

            let mut client = self.get_or_connect().await?;

            let mut request = Request::new(payload.clone());
            request.set_timeout(self.query_timeout);

            // Inject identity headers for cluster isolation (ADR 004/005).
            request.metadata_mut().insert(
                HEADER_CLUSTER_ID,
                self.cluster_id.as_str().parse().map_err(|_| {
                    Status::internal("Failed to parse cluster_id for outbound header")
                })?,
            );
            if let Some(target_node_id) = self.current_node_id().await {
                request.metadata_mut().insert(
                    HEADER_TARGET_NODE_ID,
                    target_node_id.to_string().parse().map_err(|_| {
                        Status::internal("Failed to parse target_node_id for outbound header")
                    })?,
                );
            }

            let response = client.query_state(request).await.map(|r| r.into_inner());

            match response {
                Ok(res) => match QueryStatus::try_from(res.status) {
                    Ok(QueryStatus::Rejected) => {
                        self.reconcile_routing_failure(Some(res.leader_hint), retry_count)
                            .await?;
                        continue;
                    }
                    _ => {
                        self.record_current_node_success().await?;
                        return Ok(res);
                    }
                },
                Err(_) => {
                    self.reconcile_routing_failure(None, retry_count).await?;
                    continue;
                }
            }
        }
    }

    // --- Connection & Redirection Management ---

    async fn get_or_connect(&self) -> Result<IngressServiceClient<Channel>> {
        if let Some(client) = self.client.read().await.as_ref() {
            return Ok(client.clone());
        }

        let mut client_lock = self.client.write().await;
        if let Some(client) = client_lock.as_ref() {
            return Ok(client.clone());
        }

        let addr = {
            let state = self.state.read().await;
            state
                .known_nodes()
                .first()
                .cloned()
                .context("No known nodes available to connect")?
        };

        let uri = if addr.starts_with("http://") || addr.starts_with("https://") {
            addr.clone()
        } else {
            format!("http://{}", addr)
        };

        let endpoint = Endpoint::from_shared(uri)
            .context("Invalid node address format")?
            .connect_timeout(self.connect_timeout);

        let channel = endpoint
            .connect()
            .await
            .with_context(|| format!("Failed to connect to node at {}", addr))?;

        let new_client = IngressServiceClient::new(channel);
        *client_lock = Some(new_client.clone());
        Ok(new_client)
    }

    async fn handle_redirection(&self, leader_hint: &str) -> Result<()> {
        let mut state = self.state.write().await;
        state.record_hint(leader_hint.to_string())?;

        let mut client_lock = self.client.write().await;
        *client_lock = None;
        Ok(())
    }

    async fn handle_transport_error(&self, _error: tonic::Status) -> Result<()> {
        let state = self.state.write().await;
        let mut nodes = state.known_nodes().to_vec();

        if nodes.len() > 1 {
            let failed_node = nodes.remove(0);
            nodes.push(failed_node);
        }

        let mut client_lock = self.client.write().await;
        *client_lock = None;
        Ok(())
    }

    async fn record_current_node_success(&self) -> Result<()> {
        let addr_opt = self.state.read().await.known_nodes().first().cloned();
        if let Some(addr) = addr_opt {
            let mut state = self.state.write().await;
            state.record_success(&addr)?;
        }
        Ok(())
    }

    /// Reconciles a routing failure by either following a leader hint or
    /// rotating to the next known node and applying exponential backoff.
    ///
    /// This is a centralized orchestrator for the retry logic mandated by ADR
    /// 003.
    async fn reconcile_routing_failure(
        &self,
        leader_hint: Option<String>,
        retry_count: usize,
    ) -> Result<()> {
        if let Some(hint) = leader_hint {
            if !hint.is_empty() {
                return self.handle_redirection(&hint).await;
            }
        }

        // If no hint is available (Election in progress or transport error),
        // we rotate the nodes and back off to avoid thundering herd.
        self.handle_transport_error(Status::unavailable("Leader unavailable"))
            .await?;
        tokio::time::sleep(self.calculate_backoff(retry_count)).await;
        Ok(())
    }

    /// Helper to resolve the NodeId of the currently connected node.
    ///
    /// NOTE: In this phase, we use a heuristic based on the address string
    /// to avoid breaking ClientState persistence.
    async fn current_node_id(&self) -> Option<NodeId> {
        let state = self.state.read().await;
        let addr = state.known_nodes().first()?;

        // Example: "127.0.0.1:50051" -> node_1 is configured for 50051.
        // For tests, we use a simple mapping or just 0 if unknown.
        if addr.contains("50051") {
            Some(NodeId::new(1))
        } else if addr.contains("50052") {
            Some(NodeId::new(2))
        } else if addr.contains("50053") {
            Some(NodeId::new(3))
        } else {
            None
        }
    }

    /// Calculates the exponential backoff for a given retry attempt.
    ///
    /// The formula is: self.initial_backoff * 2^(attempt - 1) capped at
    /// self.max_backoff, with ±20% randomized jitter.
    fn calculate_backoff(&self, attempt: usize) -> Duration {
        if attempt == 0 || self.initial_backoff.is_zero() {
            return Duration::ZERO;
        }

        let exponent = (attempt - 1) as u32;
        let base_backoff_ms = self.initial_backoff.as_millis() as u64;

        // Calculate exponential part with saturation to prevent overflow before cap
        let exponential_backoff_ms = base_backoff_ms.saturating_mul(2u64.pow(exponent));
        let capped_backoff_ms = exponential_backoff_ms.min(self.max_backoff.as_millis() as u64);

        let mut rng = rand::rng();
        let jitter_range = capped_backoff_ms as f64 * JITTER_FACTOR;
        let jitter_ms = rng.random_range(-jitter_range..jitter_range);

        let final_backoff_ms = (capped_backoff_ms as f64 + jitter_ms).max(0.0) as u64;
        Duration::from_millis(final_backoff_ms)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use common::proto::v1::app::MutationIntent;
    use common::proto::v1::app::OperationType;
    use common::proto::v1::app::ingress_service_server::IngressService;
    use common::proto::v1::app::ingress_service_server::IngressServiceServer;
    use tempfile::tempdir;
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::Request;
    use tonic::Response;
    use tonic::Status;

    use super::*;

    /// A programmable mock for the Ingress gRPC service.
    struct MockIngressService {
        /// A queue of responses to return for each call.
        mutation_responses: Mutex<Vec<std::result::Result<ProposeMutationResponse, Status>>>,
        query_responses: Mutex<Vec<std::result::Result<QueryStateResponse, Status>>>,
        /// Counter for tracking calls.
        call_count: AtomicUsize,
    }

    impl MockIngressService {
        fn new() -> Self {
            Self {
                mutation_responses: Mutex::new(Vec::new()),
                query_responses: Mutex::new(Vec::new()),
                call_count: AtomicUsize::new(0),
            }
        }

        fn push_mutation_response(
            &self,
            res: std::result::Result<ProposeMutationResponse, Status>,
        ) {
            self.mutation_responses.lock().unwrap().push(res);
        }

        fn push_query_response(&self, res: std::result::Result<QueryStateResponse, Status>) {
            self.query_responses.lock().unwrap().push(res);
        }
    }

    #[tonic::async_trait]
    impl IngressService for MockIngressService {
        async fn propose_mutation(
            &self,
            _request: Request<ProposeMutationRequest>,
        ) -> std::result::Result<Response<ProposeMutationResponse>, Status> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let mut queue = self.mutation_responses.lock().unwrap();
            if queue.is_empty() {
                return Err(Status::internal("Mock queue empty"));
            }
            queue.remove(0).map(Response::new)
        }

        async fn query_state(
            &self,
            _request: Request<QueryStateRequest>,
        ) -> std::result::Result<Response<QueryStateResponse>, Status> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let mut queue = self.query_responses.lock().unwrap();
            if queue.is_empty() {
                return Err(Status::internal("Mock queue empty"));
            }
            queue.remove(0).map(Response::new)
        }
    }

    /// Spawns a mock server and returns its address.
    async fn spawn_mock(mock: Arc<MockIngressService>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpListenerStream::new(listener);

        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(IngressServiceServer::from_arc(mock))
                .serve_with_incoming(stream)
                .await
                .unwrap();
        });

        addr.to_string()
    }

    fn test_intent() -> MutationIntent {
        MutationIntent {
            item_key: "milk".to_string(),
            quantity: "2".to_string(),
            unit: None,
            category: None,
            operation: OperationType::Add as i32,
        }
    }

    mod propose_mutation {
        use common::types::ClusterId;

        use super::*;

        fn mock_cluster_id() -> ClusterId {
            ClusterId::try_new("test-cluster").unwrap()
        }

        fn fast_client(state: ClientState, wal_path: impl AsRef<Path>) -> Result<LactoClient> {
            LactoClient::with_config(
                state,
                wal_path,
                DEFAULT_MUTATION_TIMEOUT,
                DEFAULT_QUERY_TIMEOUT,
                DEFAULT_CONNECT_TIMEOUT,
                Duration::ZERO,
                Duration::ZERO,
            )
        }

        #[tokio::test]
        async fn updates_state_on_redirection_hint() -> Result<()> {
            let mock_leader = Arc::new(MockIngressService::new());
            let mock_follower = Arc::new(MockIngressService::new());
            let leader_addr = spawn_mock(mock_leader.clone()).await;
            let follower_addr = spawn_mock(mock_follower.clone()).await;

            // Follower rejects and points to leader
            mock_follower.push_mutation_response(Ok(ProposeMutationResponse {
                status: MutationStatus::Rejected as i32,
                leader_hint: leader_addr.clone(),
                ..Default::default()
            }));
            // Leader succeeds
            mock_leader.push_mutation_response(Ok(ProposeMutationResponse {
                status: MutationStatus::Committed as i32,
                ..Default::default()
            }));

            let dir = tempdir()?;
            let state = ClientState::load_or_init(
                dir.path().join("state.json"),
                mock_cluster_id(),
                vec![follower_addr],
            )?;
            let client = fast_client(state, dir.path().join("wal"))?;

            client.propose_mutation(test_intent()).await?;

            // Assert: State now prioritizes leader
            let updated_state = client.state.read().await;
            assert_eq!(updated_state.known_nodes()[0], leader_addr);
            Ok(())
        }

        #[tokio::test]
        async fn retries_successfully_on_hint() -> Result<()> {
            let mock_leader = Arc::new(MockIngressService::new());
            let mock_follower = Arc::new(MockIngressService::new());
            let leader_addr = spawn_mock(mock_leader.clone()).await;
            let follower_addr = spawn_mock(mock_follower.clone()).await;

            mock_follower.push_mutation_response(Ok(ProposeMutationResponse {
                status: MutationStatus::Rejected as i32,
                leader_hint: leader_addr.clone(),
                ..Default::default()
            }));
            mock_leader.push_mutation_response(Ok(ProposeMutationResponse {
                status: MutationStatus::Committed as i32,
                ..Default::default()
            }));

            let dir = tempdir()?;
            let state = ClientState::load_or_init(
                dir.path().join("state.json"),
                mock_cluster_id(),
                vec![follower_addr],
            )?;
            let client = fast_client(state, dir.path().join("wal"))?;

            let res = client.propose_mutation(test_intent()).await?;

            assert_eq!(res.status, MutationStatus::Committed as i32);
            assert_eq!(mock_follower.call_count.load(Ordering::SeqCst), 1);
            assert_eq!(mock_leader.call_count.load(Ordering::SeqCst), 1);
            Ok(())
        }

        #[tokio::test]
        async fn handles_multiple_redirections() -> Result<()> {
            let mock_a = Arc::new(MockIngressService::new());
            let mock_b = Arc::new(MockIngressService::new());
            let mock_c = Arc::new(MockIngressService::new());
            let addr_a = spawn_mock(mock_a.clone()).await;
            let addr_b = spawn_mock(mock_b.clone()).await;
            let addr_c = spawn_mock(mock_c.clone()).await;

            mock_a.push_mutation_response(Ok(ProposeMutationResponse {
                status: MutationStatus::Rejected as i32,
                leader_hint: addr_b.clone(),
                ..Default::default()
            }));
            mock_b.push_mutation_response(Ok(ProposeMutationResponse {
                status: MutationStatus::Rejected as i32,
                leader_hint: addr_c.clone(),
                ..Default::default()
            }));
            mock_c.push_mutation_response(Ok(ProposeMutationResponse {
                status: MutationStatus::Committed as i32,
                ..Default::default()
            }));

            let dir = tempdir()?;
            let state = ClientState::load_or_init(
                dir.path().join("state.json"),
                mock_cluster_id(),
                vec![addr_a],
            )?;
            let client = fast_client(state, dir.path().join("wal"))?;

            let res = client.propose_mutation(test_intent()).await?;
            assert_eq!(res.status, MutationStatus::Committed as i32);
            assert_eq!(mock_c.call_count.load(Ordering::SeqCst), 1);
            Ok(())
        }

        #[tokio::test]
        async fn exhausts_retries_on_infinite_loop() -> Result<()> {
            let mock = Arc::new(MockIngressService::new());
            let addr = spawn_mock(mock.clone()).await;

            // Mock always redirects to itself
            for _ in 0..30 {
                mock.push_mutation_response(Ok(ProposeMutationResponse {
                    status: MutationStatus::Rejected as i32,
                    leader_hint: addr.clone(),
                    ..Default::default()
                }));
            }

            let dir = tempdir()?;
            let state = ClientState::load_or_init(
                dir.path().join("state.json"),
                mock_cluster_id(),
                vec![addr],
            )?;
            let client = fast_client(state, dir.path().join("wal"))?;

            let result = client.propose_mutation(test_intent()).await;
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("Exhausted"));
            Ok(())
        }

        #[tokio::test]
        async fn ensures_exactly_once_semantics() -> Result<()> {
            let mock = Arc::new(MockIngressService::new());
            let addr = spawn_mock(mock.clone()).await;

            mock.push_mutation_response(Ok(ProposeMutationResponse {
                status: MutationStatus::Committed as i32,
                ..Default::default()
            }));

            let dir = tempdir()?;
            let path = dir.path().join("state.json");
            let state = ClientState::load_or_init(&path, mock_cluster_id(), vec![addr])?;
            let client = fast_client(state, dir.path().join("wal"))?;

            client.propose_mutation(test_intent()).await?;

            // Verify disk state
            let disk_state_data = std::fs::read_to_string(&path)?;
            let disk_state: serde_json::Value = serde_json::from_str(&disk_state_data)?;
            assert_eq!(disk_state["sequence_id"], 1);
            Ok(())
        }
    }

    mod query_state {
        use common::types::ClusterId;

        use super::*;

        fn mock_cluster_id() -> ClusterId {
            ClusterId::try_new("test-cluster").unwrap()
        }

        fn fast_client(state: ClientState, wal_path: impl AsRef<Path>) -> Result<LactoClient> {
            LactoClient::with_config(
                state,
                wal_path,
                DEFAULT_MUTATION_TIMEOUT,
                DEFAULT_QUERY_TIMEOUT,
                DEFAULT_CONNECT_TIMEOUT,
                Duration::ZERO,
                Duration::ZERO,
            )
        }

        #[tokio::test]
        async fn follows_redirection_for_linearizable_read() -> Result<()> {
            let mock_leader = Arc::new(MockIngressService::new());
            let mock_follower = Arc::new(MockIngressService::new());
            let leader_addr = spawn_mock(mock_leader.clone()).await;
            let follower_addr = spawn_mock(mock_follower.clone()).await;

            mock_follower.push_query_response(Ok(QueryStateResponse {
                status: QueryStatus::Rejected as i32,
                leader_hint: leader_addr.clone(),
                ..Default::default()
            }));
            mock_leader.push_query_response(Ok(QueryStateResponse {
                status: QueryStatus::Success as i32,
                current_state_version: 42,
                ..Default::default()
            }));

            let dir = tempdir()?;
            let state = ClientState::load_or_init(
                dir.path().join("state.json"),
                mock_cluster_id(),
                vec![follower_addr],
            )?;
            let client = fast_client(state, dir.path().join("wal"))?;

            let res = client.query_state(None, None).await?;
            assert_eq!(res.status, QueryStatus::Success as i32);
            assert_eq!(res.current_state_version, 42);
            Ok(())
        }
    }

    mod calculate_backoff {
        use super::*;

        fn test_client(initial: Duration, max: Duration) -> LactoClient {
            let dir = tempdir().unwrap();
            let state = ClientState::load_or_init(
                dir.path().join("state.json"),
                ClusterId::try_new("test").unwrap(),
                vec!["127.0.0.1:1".to_string()],
            )
            .unwrap();
            LactoClient::with_config(
                state,
                dir.path().join("wal"),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                initial,
                max,
            )
            .unwrap()
        }

        #[test]
        fn scales_exponentially_within_jitter_bounds() {
            let client = test_client(INITIAL_BACKOFF, MAX_BACKOFF);
            // Attempt 1: 100ms base -> [80ms, 120ms]
            let b1 = client.calculate_backoff(1);
            assert!(b1 >= Duration::from_millis(80));
            assert!(b1 <= Duration::from_millis(120));

            // Attempt 3: 100 * 2^2 = 400ms base -> [320ms, 480ms]
            let b3 = client.calculate_backoff(3);
            assert!(b3 >= Duration::from_millis(320));
            assert!(b3 <= Duration::from_millis(480));
        }

        #[test]
        fn respects_maximum_configured_cap() {
            let client = test_client(INITIAL_BACKOFF, MAX_BACKOFF);
            // Attempt 10: 100 * 2^9 = 51200ms -> capped at 5000ms base -> [4000ms, 6000ms]
            let b10 = client.calculate_backoff(10);
            assert!(b10 >= Duration::from_millis(4000));
            assert!(b10 <= Duration::from_millis(6000));
        }

        #[test]
        fn provides_random_variance_between_calls() {
            let client = test_client(INITIAL_BACKOFF, MAX_BACKOFF);
            let b_a = client.calculate_backoff(5);
            let b_b = client.calculate_backoff(5);
            // Statistically, they should be different due to ±20% jitter
            assert_ne!(b_a, b_b);
        }

        #[test]
        fn returns_zero_when_configured_to_do_so() {
            let client = test_client(Duration::ZERO, Duration::ZERO);
            assert_eq!(client.calculate_backoff(5), Duration::ZERO);
        }
    }

    mod wal_integration {
        use common::types::ClusterId;

        use super::*;

        fn mock_cluster_id() -> ClusterId {
            ClusterId::try_new("test-cluster").unwrap()
        }

        fn fast_client(state: ClientState, wal_path: impl AsRef<Path>) -> Result<LactoClient> {
            LactoClient::with_config(
                state,
                wal_path,
                DEFAULT_MUTATION_TIMEOUT,
                DEFAULT_QUERY_TIMEOUT,
                DEFAULT_CONNECT_TIMEOUT,
                Duration::ZERO,
                Duration::ZERO,
            )
        }

        #[tokio::test]
        async fn removes_intent_from_wal_on_committed() -> Result<()> {
            let mock = Arc::new(MockIngressService::new());
            let addr = spawn_mock(mock.clone()).await;

            mock.push_mutation_response(Ok(ProposeMutationResponse {
                status: MutationStatus::Committed as i32,
                ..Default::default()
            }));

            let dir = tempdir()?;
            let state = ClientState::load_or_init(
                dir.path().join("state.json"),
                mock_cluster_id(),
                vec![addr],
            )?;
            let client = fast_client(state, dir.path().join("wal"))?;

            client.propose_mutation(test_intent()).await?;

            let recovered = client.wal().recover()?;
            assert!(recovered.is_empty(), "WAL should be empty after COMMITTED");
            Ok(())
        }

        #[tokio::test]
        async fn removes_intent_from_wal_on_vetoed() -> Result<()> {
            let mock = Arc::new(MockIngressService::new());
            let addr = spawn_mock(mock.clone()).await;

            mock.push_mutation_response(Ok(ProposeMutationResponse {
                status: MutationStatus::Vetoed as i32,
                ..Default::default()
            }));

            let dir = tempdir()?;
            let state = ClientState::load_or_init(
                dir.path().join("state.json"),
                mock_cluster_id(),
                vec![addr],
            )?;
            let client = fast_client(state, dir.path().join("wal"))?;

            client.propose_mutation(test_intent()).await?;

            let recovered = client.wal().recover()?;
            assert!(recovered.is_empty(), "WAL should be empty after VETOED");
            Ok(())
        }

        #[tokio::test]
        async fn preserves_intent_in_wal_on_transport_failure() -> Result<()> {
            let mock = Arc::new(MockIngressService::new());
            let addr = spawn_mock(mock.clone()).await;

            // Mock returns a transport-level error
            mock.push_mutation_response(Err(Status::unavailable("Service down")));

            let dir = tempdir()?;
            let state = ClientState::load_or_init(
                dir.path().join("state.json"),
                mock_cluster_id(),
                vec![addr],
            )?;
            let client = fast_client(state, dir.path().join("wal"))?;

            let result = client.propose_mutation(test_intent()).await;
            assert!(result.is_err());

            let recovered = client.wal().recover()?;
            assert_eq!(recovered.len(), 1, "WAL should preserve intent on failure");
            assert_eq!(recovered[0].0.value(), 1);
            Ok(())
        }

        #[tokio::test]
        async fn retries_on_empty_leader_hint_during_election() -> Result<()> {
            let mock = Arc::new(MockIngressService::new());
            let addr = spawn_mock(mock.clone()).await;

            // Mock: 3 Rejections (No Hint) followed by 1 Success
            for _ in 0..3 {
                mock.push_mutation_response(Ok(ProposeMutationResponse {
                    status: MutationStatus::Rejected as i32,
                    leader_hint: String::new(),
                    ..Default::default()
                }));
            }
            mock.push_mutation_response(Ok(ProposeMutationResponse {
                status: MutationStatus::Committed as i32,
                ..Default::default()
            }));

            let dir = tempdir()?;
            let state = ClientState::load_or_init(
                dir.path().join("state.json"),
                mock_cluster_id(),
                vec![addr],
            )?;
            let client = fast_client(state, dir.path().join("wal"))?;

            let res = client.propose_mutation(test_intent()).await?;
            assert_eq!(res.status, MutationStatus::Committed as i32);
            assert_eq!(mock.call_count.load(Ordering::SeqCst), 4);
            Ok(())
        }

        #[tokio::test]
        async fn test_startup_recovery_simulation() -> Result<()> {
            let mock = Arc::new(MockIngressService::new());
            let addr = spawn_mock(mock.clone()).await;

            // 1. Setup: Pre-populate WAL with a "crashed" intent
            let dir = tempdir()?;
            let wal_path = dir.path().join("wal");
            let wal = IntentWal::open(&wal_path)?;
            let seq = SequenceId::new(42);
            let intent = MutationIntent {
                item_key: "eggs".to_string(),
                quantity: "12".to_string(),
                unit: None,
                category: None,
                operation: OperationType::Add as i32,
            };
            let client_id = ClientId::generate();
            let req = ProposeMutationRequest::new(&client_id, seq, intent);
            wal.append(seq, &req)?;
            drop(wal); // Release lock so LactoClient can open it

            // 2. Mock server response for the recovery attempt
            mock.push_mutation_response(Ok(ProposeMutationResponse {
                status: MutationStatus::Committed as i32,
                ..Default::default()
            }));

            // 3. Initialize client (simulating restart)
            let state = ClientState::load_or_init(
                dir.path().join("state.json"),
                mock_cluster_id(),
                vec![addr],
            )?;
            let client = fast_client(state, wal_path)?;

            // 4. Perform recovery (simulating main.rs logic)
            let pending = client.wal().recover()?;
            assert_eq!(pending.len(), 1);
            for (s, r) in pending {
                client.repropose_mutation(s, r).await?;
            }

            // 5. Verification
            assert!(
                client.wal().recover()?.is_empty(),
                "WAL should be flushed after recovery"
            );
            assert_eq!(mock.call_count.load(Ordering::SeqCst), 1);
            Ok(())
        }
    }
}
