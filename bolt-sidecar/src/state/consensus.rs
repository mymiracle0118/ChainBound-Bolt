#![allow(missing_docs)]
#![allow(unused_variables)]
#![allow(missing_debug_implementations)]

use std::time::{Duration, Instant};

use beacon_api_client::{mainnet::Client, BlockId, ProposerDuty};
use ethereum_consensus::{deneb::BeaconBlockHeader, phase0::mainnet::SLOTS_PER_EPOCH};
use reqwest::Url;

use super::CommitmentDeadline;
use crate::primitives::{CommitmentRequest, Slot};

#[derive(Debug, thiserror::Error)]
pub enum ConsensusError {
    #[error("Beacon API error: {0}")]
    BeaconApiError(#[from] beacon_api_client::Error),
    #[error("Invalid slot: {0}")]
    InvalidSlot(Slot),
    #[error("Inclusion deadline exceeded")]
    DeadlineExceeded,
    #[error("Validator not found in the slot")]
    ValidatorNotFound,
}

#[derive(Debug)]
pub struct Epoch {
    pub value: u64,
    pub start_slot: Slot,
    pub proposer_duties: Vec<ProposerDuty>,
}

pub struct ConsensusState {
    beacon_api_client: Client,
    header: BeaconBlockHeader,
    epoch: Epoch,
    validator_indexes: Vec<u64>,
    // Timestamp of when the latest slot was received
    latest_slot_timestamp: Instant,
    /// The deadline (expressed in seconds) in the slot for which to
    /// stop accepting commitments.
    ///
    /// This is used to prevent the sidecar from accepting commitments
    /// which won't have time to be included by the PBS pipeline.
    // commitment_deadline: u64,
    pub commitment_deadline: CommitmentDeadline,
    pub commitment_deadline_duration: Duration,
}

impl ConsensusState {
    /// Create a new `ConsensusState` with the given configuration.
    pub fn new(
        beacon_api_url: &str,
        validator_indexes: &[u64],
        commitment_deadline_duration: Duration,
    ) -> Self {
        let url = Url::parse(beacon_api_url).expect("valid beacon client URL");
        let beacon_api_client = Client::new(url);

        ConsensusState {
            beacon_api_client,
            header: BeaconBlockHeader::default(),
            epoch: Epoch {
                value: 0,
                start_slot: 0,
                proposer_duties: vec![],
            },
            validator_indexes: validator_indexes.to_vec(),
            latest_slot_timestamp: Instant::now(),
            commitment_deadline: CommitmentDeadline::new(0, commitment_deadline_duration),
            commitment_deadline_duration,
        }
    }

    /// This function validates the state of the chain against a block. It checks 2 things:
    /// 1. The target slot is one of our proposer slots. (TODO)
    /// 2. The request hasn't passed the slot deadline.
    ///
    /// TODO: Integrate with the registry to check if we are registered.
    pub fn validate_request(&self, request: &CommitmentRequest) -> Result<u64, ConsensusError> {
        let CommitmentRequest::Inclusion(req) = request;

        // Check if the slot is in the current epoch
        if req.slot < self.epoch.start_slot || req.slot >= self.epoch.start_slot + SLOTS_PER_EPOCH {
            return Err(ConsensusError::InvalidSlot(req.slot));
        }

        // Check if the request is within the slot commitment deadline
        if self.latest_slot_timestamp + self.commitment_deadline_duration < Instant::now() {
            return Err(ConsensusError::DeadlineExceeded);
        }

        // Find the validator index for the given slot
        let validator_index = self.find_validator_index_for_slot(req.slot)?;

        Ok(validator_index)
    }

    /// Update the latest head and fetch the relevant data from the beacon chain.
    pub async fn update_head(&mut self, head: u64) -> Result<(), ConsensusError> {
        // Reset the commitment deadline to start counting for the current slot
        self.commitment_deadline = CommitmentDeadline::new(head, self.commitment_deadline_duration);

        let update = self
            .beacon_api_client
            .get_beacon_header(BlockId::Slot(head))
            .await?;

        self.header = update.header.message;

        // Update the timestamp with current time
        self.latest_slot_timestamp = Instant::now();

        // Get the current value of slot and epoch
        let slot = self.header.slot;
        let epoch = slot / SLOTS_PER_EPOCH;

        // If the epoch has changed, update the proposer duties
        if epoch != self.epoch.value {
            self.epoch.value = epoch;
            self.epoch.start_slot = epoch * SLOTS_PER_EPOCH;

            self.fetch_proposer_duties(epoch).await?;
        }

        Ok(())
    }

    /// Fetch proposer duties for the given epoch.
    async fn fetch_proposer_duties(&mut self, epoch: u64) -> Result<(), ConsensusError> {
        let duties = self.beacon_api_client.get_proposer_duties(epoch).await?;

        self.epoch.proposer_duties = duties.1;
        Ok(())
    }

    /// Filters the proposer duties and returns the validator index for a given slot
    /// if it doesn't exists then returns error.
    fn find_validator_index_for_slot(&self, slot: u64) -> Result<u64, ConsensusError> {
        self.epoch
            .proposer_duties
            .iter()
            .find(|&duty| {
                duty.slot == slot
                    && self
                        .validator_indexes
                        .contains(&(duty.validator_index as u64))
            })
            .map(|duty| duty.validator_index as u64)
            .ok_or(ConsensusError::ValidatorNotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beacon_api_client::ProposerDuty;

    #[tokio::test]
    async fn test_find_validator_index_for_slot() {
        // Sample proposer duties
        let proposer_duties = vec![
            ProposerDuty {
                public_key: Default::default(),
                slot: 1,
                validator_index: 100,
            },
            ProposerDuty {
                public_key: Default::default(),
                slot: 2,
                validator_index: 101,
            },
            ProposerDuty {
                public_key: Default::default(),
                slot: 3,
                validator_index: 102,
            },
        ];

        // Validator indexes that we are interested in
        let validator_indexes = vec![100, 102];

        // Create a ConsensusState with the sample proposer duties and validator indexes
        let state = ConsensusState {
            beacon_api_client: Client::new(Url::parse("http://localhost").unwrap()),
            header: BeaconBlockHeader::default(),
            epoch: Epoch {
                value: 0,
                start_slot: 0,
                proposer_duties,
            },
            latest_slot_timestamp: Instant::now(),
            commitment_deadline: CommitmentDeadline::new(0, Duration::from_secs(1)),
            validator_indexes,
            commitment_deadline_duration: Duration::from_secs(1),
        };

        // Test finding a valid slot
        assert_eq!(state.find_validator_index_for_slot(1).unwrap(), 100);
        assert_eq!(state.find_validator_index_for_slot(3).unwrap(), 102);

        // Test finding an invalid slot (not in proposer duties)
        assert!(matches!(
            state.find_validator_index_for_slot(4),
            Err(ConsensusError::ValidatorNotFound)
        ));
    }
}
