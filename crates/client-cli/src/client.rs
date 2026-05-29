//! The core networking and orchestration logic for the Lact-O-Sensus client.
//!
//! This module implements the `LactoClient`, a resilient coordinator that
//! manages gRPC connections, performs leader discovery, handles automatic
//! redirection, and orchestrates Exactly-Once Semantics (EOS) using exponential
//! backoff as mandated by ADR 001 and ADR 003.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::proto::v1::app::MutationIntent;
use common::proto::v1::app::MutationStatus;
use common::proto::v1::app::ProposeMutationRequest;
use common::proto::v1::app::ProposeMutationResponse;
use common::proto::v1::app::QueryStateRequest;
use common::proto::v1::app::QueryStateResponse;
use common::proto::v1::app::QueryStatus;
use common::proto::v1::app::ingress_service_client::IngressServiceClient;
use common::rpc::IdentityInterceptor;
use common::rpc::TraceInterceptor;
use common::types::ClientId;
use common::types::ClusterId;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::SequenceId;
use common::types::errors::IdentityError;
use common::types::trace::TraceId;
use rand::RngExt;
use thiserror::Error;
use tokio::sync::RwLock;
use tonic::Code;
use tonic::Request;
use tonic::transport::Channel;
use tonic::transport::Endpoint;

use crate::state::ClientState;
use crate::state::ClientStateError;
use crate::state::MAX_KNOWN_NODES;
use crate::wal::IntentWal;
use crate::wal::WalError;

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

/// Errors associated with the LactoClient orchestration.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("Configuration Error: {0}")]
    Config(String),

    #[error("State Persistence Error: {0}")]
    State(#[from] ClientStateError),

    #[error("Write-Ahead Log Error: {0}")]
    Wal(#[from] WalError),

    #[error("Network Transport Error: {0}")]
    Transport(String),

    #[error("Identity Injection Error: {0}")]
    Identity(#[from] IdentityError),

    #[error("Request rejected by Leader: {0}")]
    Rejected(String),

    #[error("gRPC Error: {0}")]
    Grpc(#[from] tonic::Status),

    #[error("Request failed after {attempts} attempts. Exhausted all known nodes and hints.")]
    RetryExhausted { attempts: usize },
}

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
    pub fn new(state: ClientState, wal_path: impl AsRef<Path>) -> Result<Self, ClientError> {
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
    ) -> Result<Self, ClientError> {
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
    ) -> Result<Self, ClientError> {
        if mutation_timeout.is_zero() || query_timeout.is_zero() || connect_timeout.is_zero() {
            return Err(ClientError::Config(
                "Client timeouts must be non-zero".into(),
            ));
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
    ) -> Result<(ProposeMutationResponse, Option<TraceId>), ClientError> {
        let sequence_id = self.state.write().await.next_sequence_id()?;

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
    ) -> Result<(ProposeMutationResponse, Option<TraceId>), ClientError> {
        self.execute_mutation(sequence_id, payload).await
    }

    /// Orchestrates the mutation lifecycle from dispatch to WAL cleanup.
    async fn execute_mutation(
        &self,
        sequence_id: SequenceId,
        payload: ProposeMutationRequest,
    ) -> Result<(ProposeMutationResponse, Option<TraceId>), ClientError> {
        let (response, trace_id) = self.dispatch_mutation(payload).await?;

        // ADR 001: Only remove from WAL if the state is terminal.
        // REJECTED (redirection) is NOT terminal and will continue in the retry loop.
        match MutationStatus::try_from(response.status) {
            Ok(MutationStatus::Committed) | Ok(MutationStatus::Vetoed) => {
                self.wal.remove(sequence_id)?;
            }
            _ => {}
        }

        Ok((response, trace_id))
    }

    /// Queries the current grocery ledger state.
    ///
    /// Following the linearizable read mandate, this query is directed to the
    /// current leader and will follow redirection hints if necessary.
    pub async fn query_state(
        &self,
        query_filter: Option<String>,
        min_state_version: Option<LogIndex>,
    ) -> Result<(QueryStateResponse, Option<TraceId>), ClientError> {
        let request_payload = QueryStateRequest::new(query_filter, min_state_version);

        self.dispatch_query(request_payload).await
    }

    // --- Private Dispatch Logic ---

    /// Unifies the retry loop, connection management, and telemetry extraction
    /// for all outbound gRPC requests.
    async fn execute_with_retry<Req, Res, F, Fut, R>(
        &self,
        payload: Req,
        rpc_fn: F,
        get_rejection: R,
        timeout: Duration,
    ) -> Result<(Res, Option<TraceId>), ClientError>
    where
        Req: Clone,
        F: Fn(IngressServiceClient<Channel>, Request<Req>) -> Fut,
        Fut: Future<Output = Result<tonic::Response<Res>, tonic::Status>>,
        R: Fn(&Res) -> (bool, String),
    {
        let mut retry_count = 0;
        let max_retries = self.state.read().await.known_nodes().len() + MAX_KNOWN_NODES;

        loop {
            if retry_count >= max_retries {
                return Err(ClientError::RetryExhausted {
                    attempts: retry_count,
                });
            }
            retry_count += 1;

            let client = self.get_or_connect().await?;
            let mut request = Request::new(payload.clone());
            request.set_timeout(timeout);

            // Inject identity headers (ADR 004/005).
            if let Some(target_node_id) = self.current_node_id().await {
                IdentityInterceptor::inject_identity_into_request(
                    &mut request,
                    &self.cluster_id,
                    target_node_id,
                )?;
            }

            let r = rpc_fn(client, request).await;

            let trace_id = if let Ok(ref response) = r {
                TraceInterceptor::extract_trace_id_from_response(response)
            } else {
                None
            };

            if let Some(ref tid) = trace_id {
                tracing::info!(trace_id = %tid, "RPC acknowledged with clinical trace.");
            }

            let response = r.map(|r| r.into_inner());

            match response {
                Ok(res) => {
                    let (rejected, hint) = get_rejection(&res);
                    if rejected {
                        self.reconcile_routing_failure(Some(hint), retry_count)
                            .await?;
                        continue;
                    }
                    self.record_current_node_success().await?;
                    return Ok((res, trace_id));
                }
                Err(status) => match status.code() {
                    Code::InvalidArgument | Code::FailedPrecondition => {
                        return Err(ClientError::Rejected(status.message().to_string()));
                    }
                    _ => {
                        self.reconcile_routing_failure(None, retry_count).await?;
                        continue;
                    }
                },
            }
        }
    }

    async fn dispatch_mutation(
        &self,
        payload: ProposeMutationRequest,
    ) -> Result<(ProposeMutationResponse, Option<TraceId>), ClientError> {
        self.execute_with_retry(
            payload,
            |mut client, req| async move { client.propose_mutation(req).await },
            |res| {
                let rejected = res.status == MutationStatus::Rejected as i32;
                (rejected, res.leader_hint.clone())
            },
            self.mutation_timeout,
        )
        .await
    }

    async fn dispatch_query(
        &self,
        payload: QueryStateRequest,
    ) -> Result<(QueryStateResponse, Option<TraceId>), ClientError> {
        self.execute_with_retry(
            payload,
            |mut client, req| async move { client.query_state(req).await },
            |res| {
                let rejected = res.status == QueryStatus::Rejected as i32;
                (rejected, res.leader_hint.clone())
            },
            self.query_timeout,
        )
        .await
    }

    // --- Connection & Redirection Management ---

    async fn get_or_connect(&self) -> Result<IngressServiceClient<Channel>, ClientError> {
        if let Some(client) = self.client.read().await.as_ref() {
            return Ok(client.clone());
        }

        let mut client_lock = self.client.write().await;
        if let Some(client) = client_lock.as_ref() {
            return Ok(client.clone());
        }

        let addr = {
            let state = self.state.read().await;
            state.known_nodes().first().cloned().ok_or_else(|| {
                ClientError::Transport("No known nodes available to connect".into())
            })?
        };

        let uri = if addr.starts_with("http://") || addr.starts_with("https://") {
            addr.clone()
        } else {
            format!("http://{}", addr)
        };

        let endpoint = Endpoint::from_shared(uri)
            .map_err(|_| ClientError::Transport("Invalid node address format".into()))?
            .connect_timeout(self.connect_timeout);

        let channel = endpoint
            .connect()
            .await
            .map_err(|e| ClientError::Transport(format!("Failed to connect to {}: {}", addr, e)))?;

        let new_client = IngressServiceClient::new(channel);
        *client_lock = Some(new_client.clone());
        Ok(new_client)
    }

    async fn handle_redirection(&self, leader_hint: &str) -> Result<(), ClientError> {
        let mut state = self.state.write().await;
        state.record_hint(leader_hint.to_string())?;

        let mut client_lock = self.client.write().await;
        *client_lock = None;
        Ok(())
    }

    async fn handle_transport_error(&self) -> Result<(), ClientError> {
        let mut state = self.state.write().await;
        state.rotate_nodes()?;

        let mut client_lock = self.client.write().await;
        *client_lock = None;
        Ok(())
    }

    async fn record_current_node_success(&self) -> Result<(), ClientError> {
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
    ) -> Result<(), ClientError> {
        if let Some(hint) = leader_hint
            && !hint.is_empty()
        {
            return self.handle_redirection(&hint).await;
        }

        // If no hint is available (Election in progress or transport error),
        // we rotate the nodes and back off to avoid thundering herd.
        self.handle_transport_error().await?;
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
            NodeId::try_new(1).ok()
        } else if addr.contains("50052") {
            NodeId::try_new(2).ok()
        } else if addr.contains("50053") {
            NodeId::try_new(3).ok()
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
    use std::fs;
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
        mutation_responses: Mutex<Vec<Result<ProposeMutationResponse, Status>>>,
        query_responses: Mutex<Vec<Result<QueryStateResponse, Status>>>,
        /// Optional trace ID to return in headers.
        trace_id_to_return: Arc<Mutex<Option<TraceId>>>,
        /// Counter for tracking calls.
        call_count: AtomicUsize,
    }

    impl MockIngressService {
        fn new() -> Self {
            Self {
                mutation_responses: Mutex::new(Vec::new()),
                query_responses: Mutex::new(Vec::new()),
                trace_id_to_return: Arc::new(Mutex::new(None)),
                call_count: AtomicUsize::new(0),
            }
        }

        fn push_mutation_response(&self, res: Result<ProposeMutationResponse, Status>) {
            self.mutation_responses.lock().unwrap().push(res);
        }

        fn push_query_response(&self, res: Result<QueryStateResponse, Status>) {
            self.query_responses.lock().unwrap().push(res);
        }

        fn set_trace_id(&self, tid: TraceId) {
            *self.trace_id_to_return.lock().unwrap() = Some(tid);
        }
    }

    #[tonic::async_trait]
    impl IngressService for MockIngressService {
        async fn propose_mutation(
            &self,
            _request: Request<ProposeMutationRequest>,
        ) -> Result<Response<ProposeMutationResponse>, Status> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let mut queue = self.mutation_responses.lock().unwrap();
            if queue.is_empty() {
                return Err(Status::internal("Mock queue empty"));
            }
            let res = queue.remove(0)?;
            let mut response = Response::new(res);
            if let Some(ref tid) = *self.trace_id_to_return.lock().unwrap() {
                let _ = TraceInterceptor::inject_trace_id_into_response(&mut response, *tid);
            }
            Ok(response)
        }

        async fn query_state(
            &self,
            _request: Request<QueryStateRequest>,
        ) -> Result<Response<QueryStateResponse>, Status> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let mut queue = self.query_responses.lock().unwrap();
            if queue.is_empty() {
                return Err(Status::internal("Mock queue empty"));
            }
            let res = queue.remove(0)?;
            let mut response = Response::new(res);
            if let Some(ref tid) = *self.trace_id_to_return.lock().unwrap() {
                let _ = TraceInterceptor::inject_trace_id_into_response(&mut response, *tid);
            }
            Ok(response)
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
        MutationIntent::new(
            "milk".to_string(),
            Some("2".to_string()),
            None,
            None,
            OperationType::Add,
        )
    }

    mod propose_mutation {
        use common::types::ClusterId;

        use super::*;

        fn mock_cluster_id() -> ClusterId {
            ClusterId::try_new("test-cluster").unwrap()
        }

        fn fast_client(
            state: ClientState,
            wal_path: impl AsRef<Path>,
        ) -> Result<LactoClient, ClientError> {
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

        mod network_routing {
            use super::*;

            #[tokio::test]
            async fn updates_state_when_connected_to_follower_with_hint()
            -> Result<(), Box<dyn std::error::Error>> {
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

                client.propose_mutation(test_intent()).await?;

                let updated_state = client.state.read().await;
                assert_eq!(updated_state.known_nodes()[0], leader_addr);
                Ok(())
            }

            #[tokio::test]
            async fn retries_successfully_when_connected_to_follower_with_hint()
            -> Result<(), Box<dyn std::error::Error>> {
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

                let (res, _) = client.propose_mutation(test_intent()).await?;

                assert_eq!(res.status, MutationStatus::Committed as i32);
                assert_eq!(mock_follower.call_count.load(Ordering::SeqCst), 1);
                assert_eq!(mock_leader.call_count.load(Ordering::SeqCst), 1);
                Ok(())
            }

            #[tokio::test]
            async fn follows_path_to_leader_when_multiple_redirections_occur()
            -> Result<(), Box<dyn std::error::Error>> {
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

                let (res, _) = client.propose_mutation(test_intent()).await?;
                assert_eq!(res.status, MutationStatus::Committed as i32);
                assert_eq!(mock_c.call_count.load(Ordering::SeqCst), 1);
                Ok(())
            }

            #[tokio::test]
            async fn exhausts_retries_and_returns_error_when_infinite_redirection_loop_detected()
            -> Result<(), Box<dyn std::error::Error>> {
                let mock = Arc::new(MockIngressService::new());
                let addr = spawn_mock(mock.clone()).await;

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
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("Request failed after")
                );
                Ok(())
            }
        }

        mod success_path {
            use super::*;

            #[tokio::test]
            async fn ensures_exactly_once_semantics_when_executing_successfully()
            -> Result<(), Box<dyn std::error::Error>> {
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

                let disk_state_data = fs::read_to_string(&path)?;
                let disk_state: serde_json::Value = serde_json::from_str(&disk_state_data)?;
                assert_eq!(disk_state["sequence_id"], 1);
                Ok(())
            }

            #[tokio::test]
            async fn extracts_and_returns_trace_id_when_executing_successfully()
            -> Result<(), Box<dyn std::error::Error>> {
                let mock = Arc::new(MockIngressService::new());
                let addr = spawn_mock(mock.clone()).await;
                let expected_tid = TraceId::generate();
                mock.set_trace_id(expected_tid);

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

                let (_, tid) = client.propose_mutation(test_intent()).await?;
                assert_eq!(tid, Some(expected_tid));
                Ok(())
            }
        }
    }

    mod query_state {
        use common::types::ClusterId;

        use super::*;

        fn mock_cluster_id() -> ClusterId {
            ClusterId::try_new("test-cluster").unwrap()
        }

        fn fast_client(
            state: ClientState,
            wal_path: impl AsRef<Path>,
        ) -> Result<LactoClient, ClientError> {
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

        mod network_routing {
            use super::*;

            #[tokio::test]
            async fn follows_redirection_for_linearizable_read_when_connected_to_follower()
            -> Result<(), Box<dyn std::error::Error>> {
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

                let (res, _) = client.query_state(None, None).await?;
                assert_eq!(res.status, QueryStatus::Success as i32);
                assert_eq!(res.current_state_version, 42);
                Ok(())
            }
        }

        mod success_path {
            use super::*;

            #[tokio::test]
            async fn extracts_and_returns_trace_id_when_executing_successfully()
            -> Result<(), Box<dyn std::error::Error>> {
                let mock = Arc::new(MockIngressService::new());
                let addr = spawn_mock(mock.clone()).await;
                let expected_tid = TraceId::generate();
                mock.set_trace_id(expected_tid);

                mock.push_query_response(Ok(QueryStateResponse {
                    status: QueryStatus::Success as i32,
                    ..Default::default()
                }));

                let dir = tempdir()?;
                let state = ClientState::load_or_init(
                    dir.path().join("state.json"),
                    mock_cluster_id(),
                    vec![addr],
                )?;
                let client = fast_client(state, dir.path().join("wal"))?;

                let (_, tid) = client.query_state(None, None).await?;
                assert_eq!(tid, Some(expected_tid));
                Ok(())
            }
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

        mod duration_calculation {
            use super::*;

            #[test]
            fn scales_exponentially_within_jitter_bounds_when_calculating_delays() {
                let client = test_client(INITIAL_BACKOFF, MAX_BACKOFF);
                let b1 = client.calculate_backoff(1);
                assert!(b1 >= Duration::from_millis(80));
                assert!(b1 <= Duration::from_millis(120));

                let b3 = client.calculate_backoff(3);
                assert!(b3 >= Duration::from_millis(320));
                assert!(b3 <= Duration::from_millis(480));
            }

            #[test]
            fn respects_maximum_configured_cap_when_calculating_delays() {
                let client = test_client(INITIAL_BACKOFF, MAX_BACKOFF);
                let b10 = client.calculate_backoff(10);
                assert!(b10 >= Duration::from_millis(4000));
                assert!(b10 <= Duration::from_millis(6000));
            }

            #[test]
            fn provides_random_variance_between_calls_when_calculating_delays() {
                let client = test_client(INITIAL_BACKOFF, MAX_BACKOFF);
                let b_a = client.calculate_backoff(5);
                let b_b = client.calculate_backoff(5);
                assert_ne!(b_a, b_b);
            }

            #[test]
            fn returns_zero_when_configured_to_do_so_when_calculating_delays() {
                let client = test_client(Duration::ZERO, Duration::ZERO);
                assert_eq!(client.calculate_backoff(5), Duration::ZERO);
            }
        }
    }

    mod wal_integration {
        use common::types::ClusterId;

        use super::*;

        fn mock_cluster_id() -> ClusterId {
            ClusterId::try_new("test-cluster").unwrap()
        }

        fn fast_client(
            state: ClientState,
            wal_path: impl AsRef<Path>,
        ) -> Result<LactoClient, ClientError> {
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

        mod intent_cleanup {
            use super::*;

            #[tokio::test]
            async fn removes_intent_from_wal_on_committed_when_mutation_is_terminal()
            -> Result<(), Box<dyn std::error::Error>> {
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
            async fn removes_intent_from_wal_on_vetoed_when_mutation_is_terminal()
            -> Result<(), Box<dyn std::error::Error>> {
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
        }

        mod network_failure {
            use super::*;

            #[tokio::test]
            async fn preserves_intent_in_wal_when_transport_failure_occurs()
            -> Result<(), Box<dyn std::error::Error>> {
                let mock = Arc::new(MockIngressService::new());
                let addr = spawn_mock(mock.clone()).await;

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
                assert_eq!(recovered[0].0.as_u64(), 1);
                Ok(())
            }

            #[tokio::test]
            async fn retries_on_empty_leader_hint_when_election_in_progress()
            -> Result<(), Box<dyn std::error::Error>> {
                let mock = Arc::new(MockIngressService::new());
                let addr = spawn_mock(mock.clone()).await;

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

                let (res, _) = client.propose_mutation(test_intent()).await?;
                assert_eq!(res.status, MutationStatus::Committed as i32);
                assert_eq!(mock.call_count.load(Ordering::SeqCst), 4);
                Ok(())
            }
        }

        mod startup_recovery {
            use super::*;

            #[tokio::test]
            async fn performs_successful_recovery_when_starting_up_with_pending_intents()
            -> Result<(), Box<dyn std::error::Error>> {
                let mock = Arc::new(MockIngressService::new());
                let addr = spawn_mock(mock.clone()).await;

                let dir = tempdir()?;
                let wal_path = dir.path().join("wal");
                let wal = IntentWal::open(&wal_path)?;
                let seq = SequenceId::new(42);
                let intent = MutationIntent::new(
                    "eggs".to_string(),
                    Some("12".to_string()),
                    None,
                    None,
                    OperationType::Add,
                );
                let client_id = ClientId::generate();
                let req = ProposeMutationRequest::new(&client_id, seq, intent);
                wal.append(seq, &req)?;
                drop(wal);

                mock.push_mutation_response(Ok(ProposeMutationResponse {
                    status: MutationStatus::Committed as i32,
                    ..Default::default()
                }));

                let state = ClientState::load_or_init(
                    dir.path().join("state.json"),
                    mock_cluster_id(),
                    vec![addr],
                )?;
                let client = fast_client(state, wal_path)?;

                let pending = client.wal().recover()?;
                assert_eq!(pending.len(), 1);
                for (s, r) in pending {
                    client.repropose_mutation(s, r).await?;
                }

                assert!(
                    client.wal().recover()?.is_empty(),
                    "WAL should be flushed after recovery"
                );
                assert_eq!(mock.call_count.load(Ordering::SeqCst), 1);
                Ok(())
            }
        }
    }
}
