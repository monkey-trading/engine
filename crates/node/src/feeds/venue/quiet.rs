//! The read deadline of a venue that sends nothing while its feed is quiet (see
//! `Adapter::may_go_quiet`).

use tokio::time::{Duration, Instant};

use super::{Adapter, VenueFeed};
use crate::clock::now_ms;

impl VenueFeed {
    /// Whether the read deadline may lapse: the connection has its subscription on a venue that may
    /// go quiet, and no market it quotes is open. The login and the subscription stay bound by the
    /// deadline, and pings still end a dead transport at any hour.
    fn quiet_allowed(&self, adapter: &Adapter) -> bool {
        adapter.may_go_quiet() && self.subscribed && !self.expects_quotes(now_ms())
    }

    /// Moves the read deadline when `quiet_allowed` changes: a day out while silence is allowed, and
    /// a whole `read_timeout` from now once a quoted market opens, so a stream quiet all night is not
    /// ended at the first check, yet one silent after the open is.
    pub(super) fn allow_quiet(
        &self,
        adapter: &Adapter,
        quiet: &mut bool,
        deadline: &mut Instant,
        read_timeout: Duration,
    ) {
        let allowed = self.quiet_allowed(adapter);
        if allowed != *quiet {
            *quiet = allowed;
            *deadline = Instant::now() + if allowed { Duration::from_secs(24 * 60 * 60) } else { read_timeout };
        }
    }
}
