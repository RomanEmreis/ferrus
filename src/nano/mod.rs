//! Native agent core and managed Ferrus adapter; frontend and provider wiring are separate.

pub(crate) mod engine;
pub(crate) mod ferrus;
pub(crate) mod journal;
mod private;
pub(crate) mod provider;
pub(crate) mod replay;
pub(crate) mod session;
pub(crate) mod tools;

#[cfg(test)]
mod core_tests;
#[cfg(test)]
mod journal_tests;
