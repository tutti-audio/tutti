//! The web: a thread has no priority to ask for, so every request is refused
//! and the thread keeps the browser's.

use super::{PriorityOutcome, PriorityRequest};

pub(super) fn request(request: PriorityRequest) -> PriorityOutcome {
    PriorityOutcome::Refused {
        requested: request,
        error: "the platform has no thread priorities".to_owned(),
    }
}
