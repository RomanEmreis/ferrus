pub mod approve;
pub mod check;
pub mod create_task;
pub mod next_task;
pub mod reject;
pub mod reset;
pub mod review_pending;
pub mod status;
pub mod submit;

use neva::prelude::*;

/// Convert an [`anyhow::Error`] into a neva tool error.
pub(super) fn tool_err(e: anyhow::Error) -> Error {
    Error::new(ErrorCode::InternalError, std::io::Error::other(e.to_string()))
}
