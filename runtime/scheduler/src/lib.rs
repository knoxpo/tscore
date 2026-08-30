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

pub type JobFn = Box<dyn FnOnce(&Ctx) + Send>;

struct JobUnit {
    batch: Arc<Batch>,
    f: JobFn,
}

/// A group of jobs with a shared completion counter. `Pool::wait` blocks
/// (and helps) until every job — including nested spawns — finished.
pub struct Batch {
    remaining: AtomicUsize,
}

impl Batch {
    pub fn new() -> Arc<Batch> {
        Arc::new(Batch { remaining: AtomicUsize::new(0) })
    }

    pub fn is_done(&self) -> bool {
        self.remaining.load(Ordering::SeqCst) == 0
    }
}

struct Shared {
    injector: Injector<JobUnit>,
    stealers: Vec<Stealer<JobUnit>>,
    sleep_m: Mutex<()>,
    sleep_cv: Condvar,
    /// Threads currently parked (or about to park) — completion only
    /// notifies when someone is listening.
    sleepers: AtomicUsize,
    shutdown: AtomicBool,
    // observability (relaxed; approximate is fine)
    tasks_executed: AtomicUsize,
    steals: AtomicUsize,
    submits: AtomicUsize,
    parks: AtomicUsize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PoolStats {
    pub workers: usize,
    pub tasks_executed: usize,
    pub steals: usize,
    pub submits: usize,
    pub parks: usize,
}

impl Shared {
    fn notify(&self) {
        if self.sleepers.load(Ordering::SeqCst) > 0 {
            self.sleep_cv.notify_all();
        }
    }

    fn park(&self, timeout: Duration, recheck: impl Fn() -> bool) {
        self.parks.fetch_add(1, Ordering::Relaxed);
        self.sleepers.fetch_add(1, Ordering::SeqCst);
        let mut g = self.sleep_m.lock();
        if !recheck() {
            self.sleep_cv.wait_for(&mut g, timeout);
        }
        drop(g);
        self.sleepers.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Execution context handed to every job; lets a job spawn subtasks into
/// the same batch (pushed to the running worker's own deque when on a
/// worker thread, the injector otherwise).
pub struct Ctx<'a> {
    batch: &'a Arc<Batch>,
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
        self.shared.notify();
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
            sleepers: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
            tasks_executed: AtomicUsize::new(0),
            steals: AtomicUsize::new(0),
            submits: AtomicUsize::new(0),
            parks: AtomicUsize::new(0),
        });
        let handles = workers
            .into_iter()
            .enumerate()
            .map(|(i, w)| {
                let shared = shared.clone();
                std::thread::Builder::new()
                    .name(format!("tscore-worker-{i}"))
                    .stack_size(32 << 20)
                    .spawn(move || {
                        #[cfg(target_os = "macos")]
                        tsp_macos::prefer_performance_cores();
                        #[cfg(target_os = "linux")]
                        tsp_linux::prefer_performance_cores();
                        worker_loop(w, i, &shared)
                    })
                    .expect("spawn worker")
            })
            .collect();
        Pool { shared, handles, n_workers: n }
    }

    pub fn workers(&self) -> usize {
        self.n_workers
    }

    pub fn stats(&self) -> PoolStats {
        PoolStats {
            workers: self.n_workers,
            tasks_executed: self.shared.tasks_executed.load(Ordering::Relaxed),
            steals: self.shared.steals.load(Ordering::Relaxed),
            submits: self.shared.submits.load(Ordering::Relaxed),
            parks: self.shared.parks.load(Ordering::Relaxed),
        }
    }

    /// Submit one job into a batch; it starts as soon as a worker is free.
    pub fn submit(&self, batch: &Arc<Batch>, f: JobFn) {
        batch.remaining.fetch_add(1, Ordering::SeqCst);
        self.shared.injector.push(JobUnit { batch: batch.clone(), f });
        self.shared.submits.fetch_add(1, Ordering::Relaxed);
        self.shared.notify();
    }

    /// Help execute pool work until `done()` — used by batch waits and by
    /// join-style blocking. Parks briefly when no work is visible.
    pub fn help_until(&self, done: impl Fn() -> bool) {
        while !done() {
            if let Some(unit) = find_job(&self.shared, None, usize::MAX) {
                run_unit(unit, None, &self.shared);
            } else {
                // timeout: work may sit in a worker deque between steals
                self.shared.park(Duration::from_micros(200), &done);
            }
        }
    }

    /// Block (helping) until the batch — including nested spawns — drains.
    pub fn wait(&self, batch: &Arc<Batch>) {
        self.help_until(|| batch.is_done());
    }

    /// Convenience: submit all jobs, wait for completion.
    pub fn run_batch<I>(&self, jobs: I)
    where
        I: IntoIterator<Item = JobFn>,
    {
        let batch = Batch::new();
        for f in jobs {
            self.submit(&batch, f);
        }
        self.wait(&batch);
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.shared.sleep_cv.notify_all(); // wake parked workers for shutdown
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

fn run_unit(unit: JobUnit, local: Option<&Worker<JobUnit>>, shared: &Shared) {
    shared.tasks_executed.fetch_add(1, Ordering::Relaxed);
    let ctx = Ctx { batch: &unit.batch, local, shared };
    (unit.f)(&ctx);
    unit.batch.remaining.fetch_sub(1, Ordering::SeqCst);
    shared.notify();
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
                crossbeam_deque::Steal::Success(u) => {
                    shared.steals.fetch_add(1, Ordering::Relaxed);
                    return Some(u);
                }
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
                if shared.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                // ponytail: timed park instead of Rayon's sleep/wake
                // protocol; bounded 1ms staleness, upgrade if profiles show
                // idle burn
                shared.park(Duration::from_millis(1), || {
                    shared.shutdown.load(Ordering::SeqCst)
                });
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
