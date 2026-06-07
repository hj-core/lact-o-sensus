use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use common::app_api::InventoryReader;
use common::app_api::SessionProvider;
use common::proto::v1::app::GroceryItem;
use common::proto::v1::app::MutationIntent;
use common::proto::v1::app::SessionRecord;
use common::types::ClientId;
use common::types::LogIndex;
use common::types::SequenceId;
use common::types::errors::ConsensusError;
use common::types::errors::FsmError;
use common::types::trace::TraceId;
use raft_engine::ConsensusAuthority;
use raft_engine::ConsensusHandle;
use tonic::Request;

use crate::ingress::IngressConfig;
use crate::ingress::IngressDispatcher;
use crate::veto::VetoError;
use crate::veto::VetoOutcome;
use crate::veto::VetoRelay;

#[derive(Debug, Default)]
pub struct MockRaftHandle {
    pub is_leader: bool,
    pub is_poisoned: bool,
    pub last_committed: LogIndex,
    pub leader_hint: String,
    pub rejection_reason: String,
    pub proposals: Mutex<Vec<Vec<u8>>>,
    /// If set, await_commit will sleep for this duration.
    pub commit_delay: Option<Duration>,
    /// If set, await_apply will sleep for this duration.
    pub apply_delay: Option<Duration>,
}

#[async_trait]
impl ConsensusHandle for MockRaftHandle {
    async fn propose(&self, data: Vec<u8>) -> Result<LogIndex, ConsensusError> {
        if self.is_leader {
            self.proposals.lock().unwrap().push(data);
            Ok(LogIndex::new(1))
        } else {
            Err(ConsensusError::NotLeader)
        }
    }

    async fn await_commit(&self, _index: LogIndex) -> Result<(), ConsensusError> {
        if let Some(delay) = self.commit_delay {
            tokio::time::sleep(delay).await;
        }
        if self.is_leader {
            Ok(())
        } else {
            Err(ConsensusError::NotLeader)
        }
    }

    async fn await_apply(&self, _index: LogIndex) -> Result<(), ConsensusError> {
        if let Some(delay) = self.apply_delay {
            tokio::time::sleep(delay).await;
        }
        Ok(())
    }

    fn authority(&self) -> ConsensusAuthority {
        ConsensusAuthority {
            is_leader: self.is_leader,
            is_poisoned: self.is_poisoned,
            last_committed: self.last_committed,
            leader_hint: self.leader_hint.clone(),
            rejection_reason: self.rejection_reason.clone(),
        }
    }

    async fn verify_leadership(&self) -> Result<(), ConsensusError> {
        if self.is_leader {
            Ok(())
        } else {
            Err(ConsensusError::NotLeader)
        }
    }
}

#[derive(Debug, Default)]
pub struct MockVetoRelay {
    pub outcome: Option<VetoOutcome>,
    pub error: Option<VetoError>,
}

#[async_trait]
impl VetoRelay for MockVetoRelay {
    async fn evaluate(
        &self,
        _client_id: ClientId,
        _intent: &MutationIntent,
        _current_inventory: &[GroceryItem],
        _timeout: Duration,
        _max_justification_len: usize,
        _trace_id: TraceId,
    ) -> Result<VetoOutcome, VetoError> {
        if let Some(err) = &self.error {
            return Err(err.clone());
        }
        Ok(self.outcome.clone().unwrap_or_else(valid_outcome))
    }
}

#[derive(Debug, Default)]
pub struct FlakyVetoRelay {
    pub outcome: Option<VetoOutcome>,
    pub fail_count: Mutex<usize>,
    pub max_fails: usize,
}

#[async_trait]
impl VetoRelay for FlakyVetoRelay {
    async fn evaluate(
        &self,
        _client_id: ClientId,
        _intent: &MutationIntent,
        _current_inventory: &[GroceryItem],
        _timeout: Duration,
        _max_justification_len: usize,
        _trace_id: TraceId,
    ) -> Result<VetoOutcome, VetoError> {
        let mut count = self.fail_count.lock().unwrap();
        if *count < self.max_fails {
            *count += 1;
            return Err(VetoError::Timeout(Duration::from_secs(0)));
        }
        Ok(self.outcome.clone().unwrap_or_else(|| {
            let mut v = valid_outcome();
            v.moral_justification = "Recovered".to_string();
            v
        }))
    }
}

#[derive(Debug)]
pub struct HallucinatingVetoRelay {
    pub success_outcome: VetoOutcome,
    pub hallucination_outcome: VetoOutcome,
    pub call_count: Mutex<usize>,
}

#[async_trait]
impl VetoRelay for HallucinatingVetoRelay {
    async fn evaluate(
        &self,
        _client_id: ClientId,
        _intent: &MutationIntent,
        _current_inventory: &[GroceryItem],
        _timeout: Duration,
        _max_justification_len: usize,
        _trace_id: TraceId,
    ) -> Result<VetoOutcome, VetoError> {
        let mut count = self.call_count.lock().unwrap();
        if *count == 0 {
            *count += 1;
            return Ok(self.hallucination_outcome.clone());
        }
        Ok(self.success_outcome.clone())
    }
}

#[derive(Debug)]
pub struct MixedFailureVetoRelay {
    pub hallucination_outcome: VetoOutcome,
    pub call_count: Mutex<usize>,
}

#[async_trait]
impl VetoRelay for MixedFailureVetoRelay {
    async fn evaluate(
        &self,
        _client_id: ClientId,
        _intent: &MutationIntent,
        _current_inventory: &[GroceryItem],
        _timeout: Duration,
        _max_justification_len: usize,
        _trace_id: TraceId,
    ) -> Result<VetoOutcome, VetoError> {
        let mut count = self.call_count.lock().unwrap();
        let current = *count;
        *count += 1;

        match current {
            0 => Err(VetoError::Timeout(Duration::from_secs(0))),
            _ => Ok(self.hallucination_outcome.clone()),
        }
    }
}

pub fn valid_outcome() -> VetoOutcome {
    VetoOutcome {
        is_approved: true,
        category_assignment: "Primary Flora".to_string(),
        moral_justification: "Mock justification".to_string(),
        resolved_item_key: "milk".to_string(),
        suggested_display_name: "Milk".to_string(),
        resolved_unit: "ml".to_string(),
        conversion_multiplier_to_base: "1".to_string(),
    }
}

#[derive(Debug, Default)]
pub struct MockInventorySource {
    pub items: Vec<GroceryItem>,
    pub version: LogIndex,
}

impl SessionProvider for MockInventorySource {
    fn check_session(
        &self,
        _client_id: &ClientId,
        _sequence_id: SequenceId,
    ) -> Result<Option<SessionRecord>, FsmError> {
        Ok(None)
    }
}

impl InventoryReader for MockInventorySource {
    fn get_inventory(&self) -> Vec<GroceryItem> {
        self.items.clone()
    }

    fn current_version(&self) -> LogIndex {
        self.version
    }
}

pub fn mock_dispatcher(
    raft_handle: Arc<dyn ConsensusHandle>,
    session_provider: Arc<dyn SessionProvider>,
    inventory_reader: Arc<dyn InventoryReader>,
    veto_relay: Arc<dyn VetoRelay>,
) -> IngressDispatcher {
    IngressDispatcher::new(
        raft_handle,
        session_provider,
        inventory_reader,
        veto_relay,
        IngressConfig {
            veto_timeout: Duration::from_secs(1),
            consensus_timeout: Duration::from_secs(1),
            veto_max_retries: 1,
            max_justification_len: 512,
        },
    )
}

pub fn successful_raft() -> Arc<MockRaftHandle> {
    Arc::new(MockRaftHandle {
        is_leader: true,
        ..Default::default()
    })
}

pub fn successful_inventory() -> Arc<MockInventorySource> {
    Arc::new(MockInventorySource::default())
}

pub fn successful_veto() -> Arc<MockVetoRelay> {
    Arc::new(MockVetoRelay {
        outcome: Some(valid_outcome()),
        ..Default::default()
    })
}

/// Helper to create a Request with a mandatory TraceId extension for
/// telemetry-guarded handlers.
pub fn make_request<T>(payload: T) -> Request<T> {
    let mut req = Request::new(payload);
    req.extensions_mut().insert(TraceId::generate());
    req
}
