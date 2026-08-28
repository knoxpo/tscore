# TypeScript Core — Architecture Overview

**Status:** M0 accepted · **Targets:** ARM64 macOS/Linux userspace first, RISC-V later.

TypeScript Core is a TypeScript/ECMAScript-compatible runtime designed around
multicore execution from its foundation. The central objective:

> Make parallel execution in TypeScript as natural as asynchronous execution
> became with async/await.

The developer expresses *intent* (`parallel.map`, `actor`, `Channel`); the
runtime decides *execution* (placement, partitioning, stealing, migration,
memory locality, GC scheduling, backpressure).

## Runtime architecture

```text
TypeScript Source
      │
      ▼
OXC parse + type-strip ──► Resolver (scope + M1 subset gate)
      │
      ▼
Bytecode (tsc-ir: FunctionProto / Chunk)          ← compiler↔runtime contract
      │
      ▼
Register-machine interpreter (tsr-realm)
      │
      ▼
Work-stealing scheduler (tsr-scheduler)
      │
 ┌────┴─────┬─────────┐
 ▼          ▼         ▼
Worker 0  Worker 1 … Worker N     (one pinned OS thread per core)
 │          │         │
Realm/arena heaps (isolated, per task/actor)
      │
PortableValue structured clone      ← the only way data crosses realms
```

## Module map

| Area | Crates | M1 |
|---|---|---|
| compiler | tsc-parser, tsc-ast, tsc-ir (+ types/optimizer stubs) | real |
| runtime | tsr-scheduler, tsr-task, tsr-realm, tsr-memory, tsr-gc, tsr-io (+ actor/channel stubs) | real |
| platform | tsp-arm64, tsp-linux, tsp-macos (+ riscv64 stub) | tiny |
| std | tss-parallel (+ stubs) | real |
| cli | tscore | real |
| benchmarks | tsb-harness + programs | real |

Dependency direction: `cli → std → runtime → tsc-ir → platform`.
`tsc-ir` is the only crate shared by compiler and runtime.

## Milestones

M0 architecture spec → **M1 Multicore Proof** → M2 actors + heap isolation →
M3 structured concurrency → M4 GC/memory runtime → M5 typed IR + native
compilation → M6 production runtime → M7 AI sandbox → M8 RISC-V → M9
bare-metal → M10 OS.

**M1 answers one question:** can TypeScript Core provide a significantly
cleaner and effective multicore execution model? Success = `parallel.map`
saturates all cores with speedups ≥1.7×/3×/5.5× at 2/4/8 cores on
embarrassingly parallel CPU work, and 10k+ lightweight tasks without 10k OS
threads. M1 explicitly excludes: JIT, advanced GC, NUMA, async I/O beyond
console/time, actors, Node compatibility, full ECMAScript.

## Specifications

- [language-subset-m1.md](../specifications/language-subset-m1.md)
- [actors-m2.md](../specifications/actors-m2.md)
- [bytecode.md](../specifications/bytecode.md)
- [concurrency-semantics.md](../specifications/concurrency-semantics.md)
- [memory-ownership.md](../specifications/memory-ownership.md)
- [benchmark-methodology.md](benchmark-methodology.md)
