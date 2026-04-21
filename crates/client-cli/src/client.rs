use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use common::proto::v1::MutationIntent;
use common::proto::v1::MutationStatus;
use common::proto::v1::ProposeMutationRequest;
use common::proto::v1::ProposeMutationResponse;
use common::proto::v1::QueryStateRequest;
use common::proto::v1::QueryStateResponse;
use common::proto::v1::QueryStatus;
use common::proto::v1::ingress_service_client::IngressServiceClient;
use common::types::ClientId;
use common::types::LogIndex;
use tokio::sync::RwLock;
use tonic::Request;
use tonic::transport::Channel;
use tonic::transport::Endpoint;

use crate::state::ClientState;
use crate::state::MAX_KNOWN_NODES;

/// Default timeout for mutation requests, accounting for AI Veto egress (5s)
/// and Raft consensus.
pub const DEFAULT_MUTATION_TIMEOUT: Duration = Duration::from_secs(10);
/// Default timeout for linearizable query requests.
pub const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// Default timeout for establishing a new gRPC connection.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// A resilient, high-performance client for interacting with a Lact-O-Sensus
/// cluster.
///
/// The `LactoClient` handles leader discovery, automatic redirection, and
/// ensures Exactly-Once Semantics (EOS) while minimizing synchronization
/// overhead.
pub struct LactoClient {
    /// Client session identity.
    client_id: ClientId,

    /// Configurable timeouts for different RPC classes.
    mutation_timeout: Duration,
    query_timeout: Duration,
    connect_timeout: Duration,

    /// Persistent state including sequence IDs and the node discovery list.
    state: Arc<RwLock<ClientState>>,
    /// Active gRPC channel, lazily initialized and refreshed upon redirection.
    client: RwLock<Option<IngressServiceClient<Channel>>>,
}

impl LactoClient {
    /// Creates a new `LactoClient` with default timeouts.
    pub fn new(state: ClientState) -> Self {
        // Safe to unwrap here because defaults are known constants,
        // but we'll use with_timeouts for consistency.
        Self::with_timeouts(
            state,
            DEFAULT_MUTATION_TIMEOUT,
            DEFAULT_QUERY_TIMEOUT,
            DEFAULT_CONNECT_TIMEOUT,
        )
        .expect("Default timeouts must be valid")
    }

    /// Creates a new `LactoClient` with explicit timeout configuration.
    pub fn with_timeouts(
        state: ClientState,
        mutation_timeout: Duration,
        query_timeout: Duration,
        connect_timeout: Duration,
    ) -> Result<Self> {
        if mutation_timeout.is_zero() || query_timeout.is_zero() || connect_timeout.is_zero() {
            anyhow::bail!("Client timeouts must be non-zero");
        }

        let client_id = state.client_id().clone();

        Ok(Self {
            client_id,
            mutation_timeout,
            query_timeout,
            connect_timeout,
            state: Arc::new(RwLock::new(state)),
            client: RwLock::new(None),
        })
    }

    /// Returns a reference to the underlying client state.
    pub fn state(&self) -> &Arc<RwLock<ClientState>> {
        &self.state
    }

    // --- High-Level API ---

    /// Proposes a grocery mutation to the cluster.
    ///
    /// Ensures Exactly-Once Semantics by incrementing and persisting the
    /// sequence ID *before* the RPC is dispatched.
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

        self.dispatch_mutation(request_payload).await
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

            let response = client
                .propose_mutation(request)
                .await
                .map(|r| r.into_inner());

            match response {
                Ok(res) => match MutationStatus::try_from(res.status) {
                    Ok(MutationStatus::Rejected) if !res.leader_hint.is_empty() => {
                        self.handle_redirection(&res.leader_hint).await?;
                        continue;
                    }
                    _ => {
                        self.record_current_node_success().await?;
                        return Ok(res);
                    }
                },
                Err(e) => {
                    self.handle_transport_error(e).await?;
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

            let response = client.query_state(request).await.map(|r| r.into_inner());

            match response {
                Ok(res) => match QueryStatus::try_from(res.status) {
                    Ok(QueryStatus::Rejected) if !res.leader_hint.is_empty() => {
                        self.handle_redirection(&res.leader_hint).await?;
                        continue;
                    }
                    _ => {
                        self.record_current_node_success().await?;
                        return Ok(res);
                    }
                },
                Err(e) => {
                    self.handle_transport_error(e).await?;
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
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use common::proto::v1::MutationIntent;
    use common::proto::v1::OperationType;
    use common::proto::v1::ingress_service_server::IngressService;
    use common::proto::v1::ingress_service_server::IngressServiceServer;
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
                ClusterId::try_new("test-cluster")?,
                vec![follower_addr],
            )?;
            let client = LactoClient::new(state);

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
                ClusterId::try_new("test-cluster")?,
                vec![follower_addr],
            )?;
            let client = LactoClient::new(state);

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
                ClusterId::try_new("test-cluster")?,
                vec![addr_a],
            )?;
            let client = LactoClient::new(state);

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
            for _ in 0..20 {
                mock.push_mutation_response(Ok(ProposeMutationResponse {
                    status: MutationStatus::Rejected as i32,
                    leader_hint: addr.clone(),
                    ..Default::default()
                }));
            }

            let dir = tempdir()?;
            let state = ClientState::load_or_init(
                dir.path().join("state.json"),
                ClusterId::try_new("test-cluster")?,
                vec![addr],
            )?;
            let client = LactoClient::new(state);

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
            let state =
                ClientState::load_or_init(&path, ClusterId::try_new("test-cluster")?, vec![addr])?;
            let client = LactoClient::new(state);

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
                ClusterId::try_new("test-cluster")?,
                vec![follower_addr],
            )?;
            let client = LactoClient::new(state);

            let res = client.query_state(None, None).await?;
            assert_eq!(res.status, QueryStatus::Success as i32);
            assert_eq!(res.current_state_version, 42);
            Ok(())
        }
    }
}
