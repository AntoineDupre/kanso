//! Optional OpenChamber integration.
//!
//! The HTTP contract lives in [`openchamber`] and the background status poller
//! in [`poller`], so a breaking change in OpenChamber stays in one place.

pub mod openchamber;
pub(crate) mod openchamber_pairing;
pub mod poller;
