mod approve;
mod check;
mod create_task;
mod next_task;
mod reject;
mod reset;
mod review_pending;
mod status;
mod submit;

use neva::prelude::*;

/// Convert an [`anyhow::Error`] into a neva tool error.
pub(super) fn tool_err(e: anyhow::Error) -> Error {
    Error::new(ErrorCode::InternalError, std::io::Error::other(e.to_string()))
}
