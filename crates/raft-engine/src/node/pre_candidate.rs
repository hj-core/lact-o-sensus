//! PreCandidate role implementation for the Raft engine (Phase 8).
//!
//! This module implements the pre-election ("PreVote") dry-run role that
//! checks cluster eligibility without advancing the term. A PreCandidate
//! solicits PreVote grants from peers and only transitions to Candidate
//! upon receiving a quorum. If the pre-vote campaign times out, the node
//! returns to Follower without any term change.

use common::types::errors::NodeError;
use common::types::trace::ClinicalTarget;
use tracing::info;

use super::Follower;
use super::NodeState;
use super::RaftNode;
use super::TickAction;
use crate::tick::Tick;
use crate::tick::TickDuration;

/// Transient role during a pre-election dry-run campaign.
///
/// PreCandidate nodes solicit pre-votes from peers to determine if a real
/// election (with term advancement) has a chance to succeed. This prevents
/// unnecessary term disruptions in partitioned or isolated scenarios.
#[derive(Debug)]
pub struct PreCandidate {
    campaign_start: Tick,
    timeout: TickDuration,
}

impl PreCandidate {
    pub fn new(campaign_start: Tick, timeout: TickDuration) -> Self {
        Self {
            campaign_start,
            timeout,
        }
    }

    pub fn campaign_start(&self) -> Tick {
        self.campaign_start
    }

    pub fn timeout(&self) -> TickDuration {
        self.timeout
    }

    pub fn evaluate_tick(&self, now: Tick) -> TickAction {
        if now - self.campaign_start >= self.timeout {
            TickAction::StepDown
        } else {
            TickAction::None
        }
    }
}

impl NodeState for PreCandidate {}

impl RaftNode<PreCandidate> {
    pub fn campaign_start(&self) -> Tick {
        self.state.campaign_start
    }

    pub fn timeout(&self) -> TickDuration {
        self.state.timeout
    }
}

impl RaftNode<Follower> {
    pub fn try_into_pre_candidate(
        self,
        campaign_start: Tick,
        timeout: TickDuration,
    ) -> Result<RaftNode<PreCandidate>, NodeError> {
        let node = self.transition(PreCandidate::new(campaign_start, timeout));
        info!(
            target: ClinicalTarget::RaftFoundation.as_str(),
            "Role Transition: -> PreCandidate"
        );
        Ok(node)
    }
}
