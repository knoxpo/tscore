//! Executable memory: one process-global MAP_JIT reservation, bump
//! allocated. W^X discipline: write only inside `publish`, per-thread
//! toggle, icache flush before the pointer escapes.

use parking_lot::Mutex;

const RESERVE: usize = 64 << 20;

struct Inner {
    base: *mut u8,
    used: usize,
}
unsafe impl Send for Inner {}

static HEAP: Mutex<Option<Inner>> = Mutex::new(None);

/// Copy `code` into executable memory; returns the entry pointer.
/// Emit a symbol line for a published region when `TSC_JIT_MAP` names a
/// file. Format: `<hex addr> <hex size> <name>` — the perf-map convention
/// `sample`/`perf` consumers and our own symbolizer understand.
pub fn note_symbol(addr: *const u8, bytes: usize, name: &str) {
    use std::io::Write;
    // TSC_JIT_DUMP=<dir>: raw code per region, for offset-level disassembly
    if let Some(dir) = std::env::var_os("TSC_JIT_DUMP") {
        let safe: String = name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
            .collect();
        let path = std::path::PathBuf::from(dir).join(format!("{safe}.bin"));
        let slice = unsafe { std::slice::from_raw_parts(addr, bytes) };
        let _ = std::fs::write(path, slice);
    }
    static MAP: std::sync::OnceLock<Option<std::sync::Mutex<std::fs::File>>> =
        std::sync::OnceLock::new();
    let f = MAP.get_or_init(|| {
        std::env::var_os("TSC_JIT_MAP").map(|p| {
            let path = std::path::PathBuf::from(p);
            std::sync::Mutex::new(
                std::fs::File::create(path).expect("create TSC_JIT_MAP file"),
            )
        })
    });
    if let Some(f) = f {
        let mut g = f.lock().unwrap();
        let _ = writeln!(g, "{:x} {:x} {}", addr as usize, bytes, name);
        let _ = g.flush();
    }
}

pub fn publish(code: &[u32]) -> *const u8 {
    let bytes = code.len() * 4;
    let mut g = HEAP.lock();
    let inner = g.get_or_insert_with(|| Inner {
        #[cfg(target_os = "macos")]
        base: tsp_macos::jit::map(RESERVE),
        #[cfg(not(target_os = "macos"))]
        base: unsafe {
            libc_mmap(RESERVE)
        },
        used: 0,
    });
    let aligned = (inner.used + 15) & !15;
    assert!(aligned + bytes <= RESERVE, "JIT heap exhausted");
    let dst = unsafe { inner.base.add(aligned) };
    #[cfg(target_os = "macos")]
    {
        tsp_macos::jit::writable(true);
        unsafe {
            std::ptr::copy_nonoverlapping(code.as_ptr() as *const u8, dst, bytes);
        }
        tsp_macos::jit::writable(false);
        tsp_macos::jit::flush(dst, bytes);
    }
    #[cfg(not(target_os = "macos"))]
    unsafe {
        std::ptr::copy_nonoverlapping(code.as_ptr() as *const u8, dst, bytes);
    }
    inner.used = aligned + bytes;
    dst
}

#[cfg(not(target_os = "macos"))]
unsafe fn libc_mmap(len: usize) -> *mut u8 {
    // Linux: plain RWX pages (hardened W^X split later)
    let p = libc::mmap(
        std::ptr::null_mut(),
        len,
        libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
        -1,
        0,
    );
    assert!(p != libc::MAP_FAILED);
    p as *mut u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::{Asm, Cond};

    #[test]
    fn execute_f64_kernel() {
        // fn(x: f64) -> f64 { x * 2.0 + 1.0 }  — args/ret in d0
        let mut a = Asm::new();
        a.fadd(0, 0, 0); // x*2 == x+x
        a.mov_imm64(9, 1.0f64.to_bits());
        a.fmov_dx(1, 9);
        a.fadd(0, 0, 1);
        a.ret();
        let ptr = publish(&a.finish());
        let f: extern "C" fn(f64) -> f64 = unsafe { std::mem::transmute(ptr) };
        assert_eq!(f(21.0), 43.0);
        assert_eq!(f(-0.5), 0.0);
    }

    #[test]
    fn execute_branchy_kernel() {
        // fn(n: u64) -> u64 { let mut s = 0; for i in 0..n { s += i } s }
        // x0 = n, x1 = s, x2 = i
        let mut a = Asm::new();
        let loop_top = a.new_label();
        let done = a.new_label();
        a.movz(1, 0, 0);
        a.movz(2, 0, 0);
        a.bind(loop_top);
        a.cmp_reg(2, 0);
        a.b_cond(Cond::Hs, done);
        a.add_reg(1, 1, 2);
        a.add_imm(2, 2, 1);
        a.b(loop_top);
        a.bind(done);
        a.mov(0, 1);
        a.ret();
        let ptr = publish(&a.finish());
        let f: extern "C" fn(u64) -> u64 = unsafe { std::mem::transmute(ptr) };
        assert_eq!(f(10), 45);
        assert_eq!(f(0), 0);
        assert_eq!(f(1000), 499500);
    }
}
