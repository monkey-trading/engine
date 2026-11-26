//! The stand-in lens's candidates and overview.

use alloy::primitives::{Address, B256, U256};

use crate::custody::operators::lens::{Election, Operator, Overview, Parameters, Row};
use crate::custody::operators::tests::FINAL_TIME;

/// `index`'s candidate: addresses and registration order run opposite ways, so a list in the lens's
/// order is not in registration order.
pub(super) fn row(index: usize, total: usize) -> Row {
    let byte = u8::try_from(index + 1).unwrap();
    Row {
        candidate: Address::repeat_byte(byte),
        adminKey: Address::repeat_byte(byte.wrapping_add(100)),
        enclave: Address::repeat_byte(byte.wrapping_add(50)),
        bond: U256::from(2_000u64) * U256::from(10).pow(U256::from(18)),
        release: B256::repeat_byte(byte),
        registration: (total - index) as u64,
        apiHostname: format!("api-{index}.example").into_bytes().into(),
        handoverEndpoint: format!("handover-{index}.example:9443").into_bytes().into(),
        votes: U256::from(index as u64 + 1) * U256::from(10).pow(U256::from(18)),
        eligible: index.is_multiple_of(2),
        role: u8::from(index == 1) * 2,
    }
}

pub(super) fn overview(count: usize) -> Overview {
    Overview {
        rotationEnabled: true,
        parameters: Parameters {
            epochLength: 86_400,
            finalityMargin: 600,
            eligibilityBps: 500,
            quorumBps: 2_000,
            maxCandidates: 100,
            minSelfBond: U256::from(1_000u64) * U256::from(10).pow(U256::from(18)),
            livenessTimeout: 259_200,
        },
        openEpoch: 7,
        openEpochStart: FINAL_TIME - 3_600,
        totalStaked: U256::from(9_000u64) * U256::from(10).pow(U256::from(18)),
        candidateCount: U256::from(count),
        inEffect: Operator {
            candidate: Address::repeat_byte(2),
            adminKey: Address::repeat_byte(102),
            release: B256::repeat_byte(2),
            since: FINAL_TIME - 86_400,
        },
        named: Operator {
            candidate: Address::repeat_byte(2),
            adminKey: Address::repeat_byte(102),
            release: B256::repeat_byte(2),
            since: FINAL_TIME - 86_400,
        },
        pending: Election {
            epoch: 6,
            candidate: Address::repeat_byte(3),
            adminKey: Address::repeat_byte(103),
            release: B256::repeat_byte(3),
            effectiveAt: FINAL_TIME + 604_800,
            promoted: false,
        },
        pendingPromotion: false,
    }
}
