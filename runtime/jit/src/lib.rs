//! tsr-jit
//!
//! Hand-rolled ARM64 JIT: encoder, executable-page heap, baseline (Tier-1)
//! and typed (Tier-2) compilers. See docs/specifications/native-compilation-m5.md.

pub mod asm;
pub mod heap;
pub mod tier1;
