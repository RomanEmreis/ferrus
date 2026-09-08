//! Opt-in inference transports. The first target is LM Studio Chat Completions.

pub(crate) mod openai;
mod sse;
#[cfg(test)]
mod tests;
