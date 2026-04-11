use neva::prelude::*;

use crate::state::store;

fn to_err(e: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorCode::InternalError,
        std::io::Error::other(e.to_string()),
    )
}

/// Handler for `ferrus://{file}` resource reads.
pub async fn read(file: String) -> Result<ReadResourceResult, Error> {
    let (mime, content) = match file.as_str() {
        "task" => ("text/markdown", store::read_task().await.map_err(to_err)?),
        "feedback" => (
            "text/markdown",
            store::read_feedback().await.map_err(to_err)?,
        ),
        "review" => ("text/markdown", store::read_review().await.map_err(to_err)?),
        "submission" => (
            "text/markdown",
            store::read_submission().await.map_err(to_err)?,
        ),
        "question" => (
            "text/markdown",
            tokio::fs::read_to_string(".ferrus/QUESTION.md")
                .await
                .unwrap_or_default(),
        ),
        "consult_request" => (
            "text/markdown",
            store::read_consult_request().await.map_err(to_err)?,
        ),
        "consult_response" => (
            "text/markdown",
            store::read_consult_response().await.map_err(to_err)?,
        ),
        "state" => {
            let state = store::read_state().await.map_err(to_err)?;
            let json = serde_json::to_string_pretty(&state).map_err(to_err)?;
            ("application/json", json)
        }
        _ => {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                std::io::Error::other(format!("Unknown ferrus resource: {file}")),
            ))
        }
    };

    let uri = format!("ferrus://{file}");
    Ok(ReadResourceResult::from(
        TextResourceContents::new(uri, content).with_mime(mime),
    ))
}
