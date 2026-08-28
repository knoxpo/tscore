//! tsr-task
//!
//! Task data transport: [`PortableValue`] structured clone — the ONLY way
//! values cross realm boundaries. Code (`FunctionProto`) is Arc-shared;
//! environments are deep-copied out of the source realm and rehydrated into
//! the destination realm. Mutations therefore never leak across realms.

pub mod portable;

pub use portable::PortableValue;
