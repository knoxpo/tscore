//! tsr-scheduler
//!
//! Work-stealing CPU pool: one OS thread per core, Chase-Lev deque per
//! worker (crossbeam-deque), global injector, batch scopes where the caller
//! blocks *and helps execute* until the batch drains. Tasks may spawn
//! subtasks into their own deque (lazy splitting lives on top of this in
//! tss-parallel).

use crossbeam_deque::{Injector, Stealer, Worker};
use parking_lot::{Condvar, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

type JobFn = Box<dyn FnOnce(&Ctx) + Send>;

struct JobUnit {
    batch: Arc<BatchState>,
    f: JobFn,
}

struct BatchState {
    remaining: AtomicUsize,
    m: Mutex<()>,
    cv: Condvar,
}

struct Shared {
    injector: Injector<JobUnit>,
    stealers: Vec<Stealer<JobUnit>>,
    sleep_m: Mutex<()>,
    sleep_cv: Condvar,
    shutdown: AtomicBool,
}

/// Execution context handed to every job; lets a job spawn subtasks into
/// the same batch (pushed to the running worker's own deque when on a
/// worker thread, the injector otherwise).
pub struct Ctx<'a> {
    batch: &'a Arc<BatchState>,
    local: Option<&'a Worker<JobUnit>>,
    shared: &'a Shared,
}

impl Ctx<'_> {
    pub fn spawn(&self, f: impl FnOnce(&Ctx) + Send + 'static) {
        self.batch.remaining.fetch_add(1, Ordering::SeqCst);
        let unit = JobUnit { batch: self.batch.clone(), f: Box::new(f) };
        match self.local {
            Some(w) => w.push(unit),
            None => self.shared.injector.push(unit),
        }
        self.shared.sleep_cv.notify_all();
    }
}

pub struct Pool {
    shared: Arc<Shared>,
    handles: Vec<std::thread::JoinHandle<()>>,
    n_workers: usize,
}

impl Pool {
    /// `n` total executors: n-1 pool threads plus the batch caller, which
    /// blocks *and helps* in `run_batch`. n=1 therefore runs truly serial —
    /// the honest baseline for speedup(N) = t(1)/t(N).
    pub fn new(n: usize) -> Pool {
        let n = n.max(1);
        let workers: Vec<Worker<JobUnit>> =
            (0..n - 1).map(|_| Worker::new_lifo()).collect();
        let stealers = workers.iter().map(|w| w.stealer()).collect();
        let shared = Arc::new(Shared {
            injector: Injector::new(),
            stealers,
            sleep_m: Mutex::new(()),
            sleep_cv: Condvar::new(),
            shutdown: AtomicBool::new(false),
        });
        let handles = workers
            .into_iter()
            .enumerate()
            .map(|(i, w)| {
                let shared = shared.clone();
                std::thread::Builder::new()
                    .name(format!("tscore-worker-{i}"))
                    .spawn(move || worker_loop(w, i, &shared))
                    .expect("spawn worker")
            })
            .collect();
        Pool { shared, handles, n_workers: n }
    }

    pub fn workers(&self) -> usize {
        self.n_workers
    }

    /// Run a batch of jobs to completion. The calling thread helps execute
    /// (steals) until every job — including nested spawns — has finished.
    pub fn run_batch<I>(&self, jobs: I)
    where
        I: IntoIterator<Item = JobFn>,
    {
        let batch = Arc::new(BatchState {
            remaining: AtomicUsize::new(0),
            m: Mutex::new(()),
            cv: Condvar::new(),
        });
        let mut n = 0usize;
        for f in jobs {
            batch.remaining.fetch_add(1, Ordering::SeqCst);
            self.shared.injector.push(JobUnit { batch: batch.clone(), f });
            n += 1;
        }
        if n == 0 {
            return;
        }
        self.shared.sleep_cv.notify_all();
        // caller helps: pull from injector / steal from workers
        while batch.remaining.load(Ordering::SeqCst) != 0 {
            if let Some(unit) = find_job(&self.shared, None, usize::MAX) {
                run_unit(unit, None, &self.shared);
            } else {
                let mut g = batch.m.lock();
                if batch.remaining.load(Ordering::SeqCst) != 0 {
                    // timeout: work may sit in a worker deque we cannot see
                    // between steal attempts
                    batch.cv.wait_for(&mut g, Duration::from_micros(200));
                }
            }
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.shared.sleep_cv.notify_all();
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

fn run_unit(unit: JobUnit, local: Option<&Worker<JobUnit>>, shared: &Shared) {
    let ctx = Ctx { batch: &unit.batch, local, shared };
    (unit.f)(&ctx);
    if unit.batch.remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
        let _g = unit.batch.m.lock();
        unit.batch.cv.notify_all();
    }
}

fn find_job(
    shared: &Shared,
    local: Option<&Worker<JobUnit>>,
    self_idx: usize,
) -> Option<JobUnit> {
    if let Some(w) = local {
        if let Some(u) = w.pop() {
            return Some(u);
        }
    }
    // injector first (fresh batches), then steal victims round-robin
    loop {
        match shared.injector.steal() {
            crossbeam_deque::Steal::Success(u) => return Some(u),
            crossbeam_deque::Steal::Retry => continue,
            crossbeam_deque::Steal::Empty => break,
        }
    }
    let n = shared.stealers.len();
    // start at a pseudo-random victim to avoid thundering herd
    let start = self_idx.wrapping_mul(31).wrapping_add(7) % n.max(1);
    for k in 0..n {
        let i = (start + k) % n;
        if i == self_idx {
            continue;
        }
        loop {
            match shared.stealers[i].steal() {
                crossbeam_deque::Steal::Success(u) => return Some(u),
                crossbeam_deque::Steal::Retry => continue,
                crossbeam_deque::Steal::Empty => break,
            }
        }
    }
    None
}

fn worker_loop(w: Worker<JobUnit>, idx: usize, shared: &Shared) {
    loop {
        if shared.shutdown.load(Ordering::SeqCst) {
            return;
        }
        match find_job(shared, Some(&w), idx) {
            Some(unit) => run_unit(unit, Some(&w), shared),
            None => {
                let mut g = shared.sleep_m.lock();
                if shared.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                // ponytail: timed park instead of Rayon's sleep/wake
                // protocol; bounded 1ms staleness, upgrade if profiles show
                // idle burn
                shared
                    .sleep_cv
                    .wait_for(&mut g, Duration::from_millis(1));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn ten_thousand_tasks_checksum() {
        let pool = Pool::new(8);
        let sum = Arc::new(AtomicU64::new(0));
        let jobs: Vec<_> = (0..10_000u64)
            .map(|i| {
                let sum = sum.clone();
                Box::new(move |_: &Ctx| {
                    sum.fetch_add(i, Ordering::Relaxed);
                }) as _
            })
            .collect();
        pool.run_batch(jobs);
        assert_eq!(sum.load(Ordering::SeqCst), 10_000 * 9_999 / 2);
    }

    #[test]
    fn nested_spawns_complete_before_batch_returns() {
        let pool = Pool::new(4);
        let count = Arc::new(AtomicU64::new(0));
        let c = count.clone();
        pool.run_batch(vec![Box::new(move |ctx: &Ctx| {
            for _ in 0..100 {
                let c = c.clone();
                ctx.spawn(move |ctx| {
                    let c = c.clone();
                    ctx.spawn(move |_| {
                        c.fetch_add(1, Ordering::Relaxed);
                    });
                });
            }
        }) as _]);
        assert_eq!(count.load(Ordering::SeqCst), 100);
    }

    #[test]
    fn scaling_smoke() {
        // not a benchmark — just proves distinct threads pick up work
        let pool = Pool::new(4);
        let seen = Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new()));
        let jobs: Vec<_> = (0..64)
            .map(|_| {
                let seen = seen.clone();
                Box::new(move |_: &Ctx| {
                    std::thread::sleep(Duration::from_millis(2));
                    seen.lock().insert(std::thread::current().id());
                }) as _
            })
            .collect();
        pool.run_batch(jobs);
        assert!(seen.lock().len() >= 2, "work never spread across threads");
    }
}
