//! tsr-jit
//!
//! Hand-rolled ARM64 JIT: encoder, executable-page heap, and two compilers:
//! `baseline` (one native template per bytecode op) and `optimizing` (typed,
//! unboxed). See docs/specifications/native-compilation-m5.md.

pub mod asm;
pub mod heap;
pub mod baseline;
pub mod optimizing;
