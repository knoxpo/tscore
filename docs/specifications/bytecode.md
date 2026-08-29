# Bytecode Specification (v0)

## Machine model

**Register machine.** Registers are slots in the current call frame
(max 255 per function). Fixed-width 32-bit instructions, Lua-style encoding:

```text
| op: u8 | A: u8 | B: u8 | C: u8 |      ABC form
| op: u8 | A: u8 |   Bx: u16     |      ABx form (constants, jumps use sBx)
```

Rationale: fewer dispatched instructions than a stack machine; dispatch
dominates a simple interpreter.

## Values

```rust
struct Value(u64);   // NaN-boxed
```

**NaN-boxed, 8 bytes.** Real doubles use every bit pattern whose top 16
bits are ≤ 0xFFF8; tags 0xFFF9..=0xFFFF encode null/undefined/bool, string,
object, array, closure, cell, and native-fn values with a 32-bit payload
(heap ref / native index). `Value::number` canonicalizes NaN inputs, so
payload NaNs cannot collide with tags. Off-hot-path code decodes through
`Value::kind() -> Kind` (an enum view); hot paths use `is_number`/`as_*`
accessors. Heap refs are valid only within their owning realm.

## Functions and closures

`FunctionProto` = { bytecode, constants, arity, register count, upvalue
descriptors, name/span table }. Immutable, `Arc`-shared across all workers —
**code is shared, environments are realm-local.**

Captured variables use always-boxed cells (Python-style): the resolver marks
any binding referenced from a nested function, its register holds a
`Value::Cell` from declaration on (`NewCell`), and access goes through
`LoadCell`/`StoreCell` locally, `GetUpval`/`SetUpval` from inside a closure.
Identical semantics to Lua open/closed upvalues with far less machinery;
open-upvalue optimization is a possible later upgrade if capture-heavy code
shows up in profiles.

## Instruction set (~40 ops)

| Group | Ops |
|---|---|
| load/move | `LoadConst A,Bx` · `LoadInt A,sBx` · `LoadBool` · `LoadNull` · `LoadUndef` · `Move A,B` |
| arithmetic | `Add` `Sub` `Mul` `Div` `Mod` `Pow` `Neg` (A,B,C) |
| bitwise | `BitAnd` `BitOr` `BitXor` `Shl` `Shr` `UShr` `BitNot` |
| compare | `Eq` `Ne` `Lt` `Le` `Gt` `Ge` (A,B,C) · `Not A,B` |
| control | `Jump sBx` · `JumpIfFalse A,sBx` · `JumpIfTrue A,sBx` |
| calls | `Call A,B,C` · `CallNative A,B,C` · `Return A` · `Halt` |
| closures/cells | `Closure A,Bx` · `NewCell A` · `LoadCell A,B` · `StoreCell A,B` · `GetUpval A,B` · `SetUpval A,B` |
| heap | `NewObject A` · `NewArray A,B` · `GetField A,B,C` · `SetField A,B,C` · `GetIndex A,B,C` · `SetIndex A,B,C` · `Len A,B` · `ArrayPush A,B` |
| globals | `GetGlobal A,Bx` · `SetGlobal A,Bx` |
| misc | `Concat A,B,C` · `TypeOf A,B` |

`tscore run file.ts --dump-bytecode` prints the disassembly (lives in
`tsc-ir`).

## Errors

No exceptions in TS-M1. A runtime error (type error, undefined global, OOB
slow path) aborts the current task with a Rust-side error carrying the source
span; `parallel.map` propagates the first error to the caller.
