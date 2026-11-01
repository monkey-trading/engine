//! The TEE-hosted node around `engine-core`: the sequencer and public log, log replay, the client
//! API with its TLS and limits, enclave attestation, trusted time, custody, price feeds, and the
//! operator's metrics and alert routing; `engine admin`, the operator's client; and `engine ct-watch`,
//! the Certificate Transparency watcher.

mod access;
pub mod admin;
mod alerts;
mod api;
pub(crate) mod authenticated;
mod benchmarks;
mod checkpoints;
pub use checkpoints::{ApprovalId, ApprovalStatus, LifecycleEvidence};
mod clock;
pub mod config;
pub use config::NodeConfig;
pub mod ct_watch;
mod custody;
pub mod exit;
mod feeds;
mod handover;
mod history;
mod limits;
pub mod log;
mod metrics;
mod monitor;
mod node;
pub use node::{KeyStart, parse_launch_digest, run};
mod replay_audit;
mod roughtime;
pub use replay_audit::replay_path_with_custody;
pub mod sequencer;
mod sequencer_thread;
mod snapshot;
pub mod standby;
pub use sequencer::{
    replay, replay_path, replay_path_in_domain, replay_path_with_attested, replay_path_with_trust, replay_with_signer,
};
pub use sequencer_thread::run_dedicated;
mod telemetry;
pub use telemetry::{Telemetry, init_logging, init_telemetry};
mod tee;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod test_tracing;
mod throttle;
mod tls;
mod trusted_time;
mod verify;
mod withdrawals;
pub use verify::{
    Expected as ExpectedSigner, KDS_BASE_URL, Options as VerifyOptions, ReleaseManifest, SentRequest,
    VerifiedAttestation, VerifiedResponse, ensure_signers_verified, load_attestation, load_relayed_collateral,
    parse_challenge_arg, reconcile_signer, verify_attestation, verify_attestation_files, verify_frames_file,
    verify_response_files,
};
