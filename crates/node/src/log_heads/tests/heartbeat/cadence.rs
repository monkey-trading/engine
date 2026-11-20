//! How often the poster reads the chain with rotation on while no turn names the operator.

use super::*;

#[tokio::test(start_paused = true)]
async fn with_rotation_on_and_no_turn_the_poster_reads_every_30_s_whatever_its_read_interval() {
    let chain = FakeChain::new(None);
    let drive = spawn_retrying(chain.clone(), None, Duration::from_secs(300));
    drive.release(1);
    tokio::time::sleep(Duration::from_secs(301)).await;
    let reads = chain.reads();
    drive.stop().await;
    assert_eq!(reads, 11, "reads at 0 s to 300 s");
}
