//! The collateral relay's dev scenario, over the node's API route and the actual binary.

use std::time::Duration;

use tokio::time::timeout;

use super::super::chain::Collateral;
use super::super::refresh::Relay;
use super::super::relay::RelayedCollateral;
use super::fixtures::*;
use super::relay::{collateral_of, tee};
use super::stub::kds_stub;

/// The dev scenario: the node relays the stand-in service's collateral over its API route, the
/// stand-in then stops, and `engine verify-attestation --collateral <the node's route>` still
/// verifies the quote; a VCEK for another chip and a stale revocation list in the served document
/// each fail. `scripts/dev-attestation-verifier.sh` runs it, and CI runs the script.
#[tokio::test]
#[ignore = "dev scenario, run by scripts/dev-attestation-verifier.sh"]
async fn dev_scenario_relayed_collateral() -> anyhow::Result<()> {
    use anyhow::{Context as _, ensure};

    let (chain, enclave, attestation) = fixtures();
    let stub = kds_stub(&chain).await;
    let dir = std::env::temp_dir().join(format!("relayed-collateral-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let (attestation_path, release_path, root_path) =
        (dir.join("attestation.json"), dir.join("release.toml"), dir.join("root.pem"));
    std::fs::write(&attestation_path, serde_json::to_string(&attestation)?)?;
    std::fs::write(&release_path, release_toml())?;
    std::fs::write(&root_path, pem(&chain.ark))?;

    // The node's API route, over the relay's served collateral.
    let relay = Relay::new(&tee(&stub.base, "12h"), &attestation)?.context("an SNP quote has a relay")?;
    let (mut state, _rx) = crate::api::tests::api_state(true);
    state.collateral = Some(relay.served());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let route = format!("http://{}/v1/attestation/collateral", listener.local_addr()?);
    let app = crate::api::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await });

    let client = Collateral::new(&stub.base, PROCESSOR)?;
    ensure!(client.document(&route).await.is_err(), "the route served before the first fetch succeeded");
    let mut served = relay.served();
    tokio::spawn(relay.run());
    timeout(Duration::from_secs(20), served.wait_for(Option::is_some)).await??;
    let route_bytes = client.document(&route).await?;
    let relayed = RelayedCollateral::from_json(&route_bytes)?;
    ensure!(
        relayed.vcek[..] == chain.vcek.der()[..] && relayed.root[..] == chain.ark.der()[..],
        "the route serves the four items"
    );

    // The vendor's service is gone; verification must need only the node.
    let dead = stub.base.clone();
    stub.stop().await;
    ensure!(tokio::net::TcpStream::connect(dead.trim_start_matches("http://")).await.is_err());
    let verify = |collateral: &str| {
        let mut command = tokio::process::Command::new(super::replay_dispatch::engine_binary().unwrap());
        command
            .args(["verify-attestation", "--processor", PROCESSOR, "--kds", &dead, "--collateral", collateral])
            .arg("--attestation")
            .arg(&attestation_path)
            .arg("--release")
            .arg(&release_path)
            .arg("--root")
            .arg(&root_path);
        async move { command.output().await }
    };
    let verified = verify(&route).await?;
    ensure!(verified.status.success(), "relayed collateral must verify: {}", String::from_utf8_lossy(&verified.stderr));
    println!("{}", String::from_utf8_lossy(&verified.stdout).trim());
    // The documented way to verify a self-signed node: save the route's response, then verify the file.
    let saved_path = dir.join("saved.json");
    std::fs::write(&saved_path, &route_bytes)?;
    let from_file = verify(saved_path.to_str().unwrap()).await?;
    ensure!(
        from_file.status.success(),
        "a saved collateral file must verify: {}",
        String::from_utf8_lossy(&from_file.stderr)
    );

    // The same document with a VCEK for another chip, and with a stale list, each fail.
    let mut other_chip = enclave.chip_id;
    other_chip[0] ^= 1;
    let foreign =
        RelayedCollateral { vcek: vcek_for(&chain, other_chip, REPORTED_TCB).der().to_vec(), ..relayed.clone() };
    let stale = collateral_of(&chain, &crl_signed(&chain, &[], (2024, 1, 1), (2025, 1, 1), None));
    let stale = RelayedCollateral { vcek: relayed.vcek.clone(), intermediate: relayed.intermediate.clone(), ..stale };
    for (what, document, reason) in [
        ("a VCEK for another chip", foreign, "VCEK hardware id does not match"),
        ("a stale revocation list", stale, "revocation list is expired"),
    ] {
        let path = dir.join("served.json");
        std::fs::write(&path, document.to_json())?;
        let refused = verify(path.to_str().unwrap()).await?;
        ensure!(!refused.status.success(), "verified with {what}");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        ensure!(stderr.contains(reason), "{what} was refused for another reason than {reason:?}: {stderr}");
    }
    std::fs::remove_dir_all(&dir).ok();
    println!("PASS: relayed collateral verifies from the route and from a saved file; foreign VCEK and stale CRL fail");
    Ok(())
}
