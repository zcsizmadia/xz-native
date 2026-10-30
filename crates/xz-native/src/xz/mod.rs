//! The .xz container format.

pub(crate) mod encoder;
pub(crate) mod flat;
pub(crate) mod format;
#[cfg(feature = "std")]
pub(crate) mod mt;
#[cfg(feature = "std")]
pub(crate) mod mt_encoder;
pub(crate) mod stream;
