//! Other platforms: the thread is always time-sharing.

use super::Scheduling;

pub(super) fn scheduling() -> Scheduling {
    Scheduling::TimeSharing
}
