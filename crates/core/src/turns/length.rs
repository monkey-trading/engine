//! A turn's length and the spans derived from it, the schedule's `turnLength`.
//!
//! A build with the `short-turns` feature, for development stacks only, has five-minute turns: a
//! boundary every five minutes, 22:30 UTC among them, a draw deadline a turn ahead, and one turn's
//! notice of a set's keys (`SET_NOTICE_MS`). Its logs replay only under the same feature, and a
//! `production` node build refuses it.

use crate::operator_bounds::NOTICE_MS;
use crate::types::Millis;

/// A day.
pub const DAY_MS: Millis = 24 * 60 * 60 * 1000;
/// A turn's length, the schedule's `turnLength`: a day.
#[cfg(not(feature = "short-turns"))]
pub const TURN_MS: Millis = DAY_MS;
/// A turn's length, the schedule's `turnLength`: five minutes, in a development build.
#[cfg(feature = "short-turns")]
pub const TURN_MS: Millis = 5 * 60 * 1000;
/// A turn's boundary past a multiple of the turn length, the schedule's `boundaryOffset`: 22:30 UTC
/// modulo it, so 22:30 UTC is a boundary.
pub const BOUNDARY_OFFSET_MS: Millis = ((22 * 60 + 30) * 60 * 1000) % TURN_MS;
/// How long before its boundary a turn's draw deadline falls, a turn: the turn is drawn from the set
/// in effect then.
pub const DRAW_LEAD_MS: Millis = TURN_MS;
/// How long before a turn applies a key the log must have named in a mirrored set: the seven days
/// every operator key's notice takes, or with turns shorter than a day, one turn, so a development
/// stack's engines on true time take their first turn minutes after they mirror its set.
pub const SET_NOTICE_MS: Millis = if TURN_MS == DAY_MS { NOTICE_MS } else { TURN_MS };
/// `SET_NOTICE_MS` in the words a refusal names it by.
pub(super) const SET_NOTICE: &str = if TURN_MS == DAY_MS { "seven days" } else { "one turn" };
