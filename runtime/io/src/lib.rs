//! tsr-io
//!
//! M1 native surface: console, time. The M2 event loop (io_uring/kqueue)
//! lives here later.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};
use tsr_memory::Value;
use tsr_realm::{NativeArgs, Realm, RtError};

fn num_arg(realm: &Realm, args: NativeArgs, i: usize, who: &str) -> Result<f64, RtError> {
    let v = args.get(realm, i);
    if v.is_number() {
        Ok(v.as_number())
    } else {
        Err(RtError::new(format!("{who}: expected number, got {}", v.type_of())))
    }
}

/// Install console/Math/Date/performance and string helpers into a realm.
pub fn install(realm: &mut Realm) {
    let log = realm.add_native(|realm, args| {
        let line = (0..args.len())
            .map(|i| args.get(realm, i).display(&realm.heap))
            .collect::<Vec<_>>()
            .join(" ");
        // ponytail: lock stdout per call; enough until output-heavy workloads
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        Ok(Value::UNDEFINED)
    });
    realm.set_global_obj("console", vec![("log", log)]);

    macro_rules! math1 {
        ($realm:ident, $name:literal, $f:expr) => {
            ($name, $realm.add_native(move |realm, args| {
                Ok(Value::number($f(num_arg(realm, args, 0, $name)?)))
            }))
        };
    }
    macro_rules! math2 {
        ($realm:ident, $name:literal, $f:expr) => {
            ($name, $realm.add_native(move |realm, args| {
                Ok(Value::number($f(
                    num_arg(realm, args, 0, $name)?,
                    num_arg(realm, args, 1, $name)?,
                )))
            }))
        };
    }
    let members = vec![
        math1!(realm, "floor", f64::floor),
        math1!(realm, "sqrt", f64::sqrt),
        math1!(realm, "abs", f64::abs),
        math2!(realm, "min", f64::min),
        math2!(realm, "max", f64::max),
        math2!(realm, "imul", |x: f64, y: f64| {
            tsr_memory::to_int32(x).wrapping_mul(tsr_memory::to_int32(y)) as f64
        }),
    ];
    realm.set_global_obj("Math", members);

    let now_ms = realm.add_native(|_, _| {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as f64;
        Ok(Value::number(ms))
    });
    realm.set_global_obj("Date", vec![("now", now_ms)]);

    let perf_now = realm.add_native(|_, _| {
        // monotonic, ms with sub-ms fraction, like the web API
        use std::sync::OnceLock;
        use std::time::Instant;
        static START: OnceLock<Instant> = OnceLock::new();
        let start = *START.get_or_init(Instant::now);
        Ok(Value::number(start.elapsed().as_secs_f64() * 1000.0))
    });
    realm.set_global_obj("performance", vec![("now", perf_now)]);

    // sleep(ms) -> Promise<undefined>, via a single shared timer thread
    let sleep = realm.add_native(|realm, args| {
        let ms = num_arg(realm, args, 0, "sleep")?.max(0.0);
        let (promise, completer) = realm.promise_pair();
        timer_wheel().send((
            std::time::Instant::now()
                + std::time::Duration::from_micros((ms * 1000.0) as u64),
            completer,
        ));
        Ok(promise)
    });
    realm.set_global("sleep", sleep);

    // hidden helper: `s.charCodeAt(i)` compiles to `__charCodeAt(s, i)`
    let char_code_at = realm.add_native(|realm, args| {
        let sv = args.get(realm, 0);
        let i = num_arg(realm, args, 1, "charCodeAt")? as usize;
        let s = match sv.as_str_ref() {
            Some(r) => realm.heap.str_at(r),
            None => {
                return Err(RtError::new(format!("charCodeAt on {}", sv.type_of())))
            }
        };
        Ok(match s.chars().nth(i) {
            Some(c) => Value::number(c as u32 as f64),
            None => Value::number(f64::NAN),
        })
    });
    realm.set_global("__charCodeAt", char_code_at);
}

/// Blocks until `deadline` or until `wake()` is called on the same
/// waiter, whichever comes first.
///
/// A condvar park is the obvious way to do this and it is what the timer
/// thread used to do, but on macOS it lands about 480us past a 1ms
/// deadline — most of the timer. `kevent` with a timespec lands about
/// 250us past it, which is the precision node and bun get by waiting
/// inside their event loops. Measured directly by `timer_precision`.
#[cfg(target_os = "macos")]
mod waiter {
    use std::time::Instant;

    /// EVFILT_USER identifier for the "a timer was registered" nudge.
    const WAKE_IDENT: usize = 1;

    pub struct Waiter(i32);

    // SAFETY: the kqueue fd is only read via kevent, which is thread-safe.
    unsafe impl Send for Waiter {}
    unsafe impl Sync for Waiter {}

    impl Waiter {
        pub fn new() -> Waiter {
            let kq = unsafe { libc::kqueue() };
            assert!(kq >= 0, "kqueue for the timer thread");
            let ev = libc::kevent {
                ident: WAKE_IDENT,
                filter: libc::EVFILT_USER,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: 0,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: one well-formed change entry, no event list requested.
            unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
            Waiter(kq)
        }

        /// Wait until `deadline`, or forever when there is none.
        pub fn wait(&self, deadline: Option<Instant>) {
            let mut out: libc::kevent = unsafe { std::mem::zeroed() };
            let ts = deadline.map(|d| {
                let left = d.saturating_duration_since(Instant::now());
                libc::timespec {
                    tv_sec: left.as_secs() as libc::time_t,
                    tv_nsec: left.subsec_nanos() as libc::c_long,
                }
            });
            let tsp = ts.as_ref().map_or(std::ptr::null(), |t| t as *const _);
            // SAFETY: `out` is one writable entry; `tsp` is null or borrowed
            // from `ts`, which outlives the call.
            unsafe { libc::kevent(self.0, std::ptr::null(), 0, &mut out, 1, tsp) };
        }

        /// Wake a thread blocked in `wait`.
        pub fn wake(&self) {
            let ev = libc::kevent {
                ident: WAKE_IDENT,
                filter: libc::EVFILT_USER,
                flags: 0,
                fflags: libc::NOTE_TRIGGER,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: as in `new`.
            unsafe { libc::kevent(self.0, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        }
    }

    impl Drop for Waiter {
        fn drop(&mut self) {
            unsafe { libc::close(self.0) };
        }
    }
}

/// Condvar fallback: same contract, no kqueue.
#[cfg(not(target_os = "macos"))]
mod waiter {
    use std::sync::{Condvar, Mutex};
    use std::time::Instant;

    pub struct Waiter(Mutex<bool>, Condvar);

    impl Waiter {
        pub fn new() -> Waiter {
            Waiter(Mutex::new(false), Condvar::new())
        }
        pub fn wait(&self, deadline: Option<Instant>) {
            let mut woken = self.0.lock().unwrap();
            while !*woken {
                match deadline {
                    None => woken = self.1.wait(woken).unwrap(),
                    Some(d) => {
                        let left = d.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            break;
                        }
                        let (w, t) = self.1.wait_timeout(woken, left).unwrap();
                        woken = w;
                        if t.timed_out() {
                            break;
                        }
                    }
                }
            }
            *woken = false;
        }
        pub fn wake(&self) {
            *self.0.lock().unwrap() = true;
            self.1.notify_one();
        }
    }
}

/// One process-wide timer thread: min-heap of deadlines, waiting until
/// the nearest one (or until a new registration nudges it).
struct TimerWheel {
    tx: crossbeam_channel::Sender<(std::time::Instant, tsr_realm::Completer)>,
    waiter: std::sync::Arc<waiter::Waiter>,
}

impl TimerWheel {
    fn send(&self, t: (std::time::Instant, tsr_realm::Completer)) {
        if self.tx.send(t).is_ok() {
            self.waiter.wake();
        }
    }
}

/// Env-gated once: reading it per fired timer would cost more than the
/// lateness being measured.
fn timer_debug() -> bool {
    static D: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *D.get_or_init(|| std::env::var_os("TSC_TIMER_DEBUG").is_some())
}

fn timer_wheel() -> &'static TimerWheel {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    use std::time::Instant;
    static WHEEL: std::sync::OnceLock<TimerWheel> = std::sync::OnceLock::new();
    WHEEL.get_or_init(|| {
        let (tx, rx) = crossbeam_channel::unbounded::<(Instant, tsr_realm::Completer)>();
        let waiter = std::sync::Arc::new(waiter::Waiter::new());
        let w = waiter.clone();
        std::thread::Builder::new()
            .name("tscore-timers".into())
            .spawn(move || {
                struct Entry(Instant, tsr_realm::Completer);
                impl PartialEq for Entry {
                    fn eq(&self, o: &Self) -> bool {
                        self.0 == o.0
                    }
                }
                impl Eq for Entry {}
                impl PartialOrd for Entry {
                    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
                        Some(self.cmp(o))
                    }
                }
                impl Ord for Entry {
                    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
                        self.0.cmp(&o.0)
                    }
                }
                let mut heap: BinaryHeap<Reverse<Entry>> = BinaryHeap::new();
                loop {
                    // take every registration that has arrived
                    loop {
                        match rx.try_recv() {
                            Ok((t, c)) => heap.push(Reverse(Entry(t, c))),
                            Err(crossbeam_channel::TryRecvError::Empty) => break,
                            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                                while let Some(Reverse(Entry(t, c))) = heap.pop() {
                                    let now = Instant::now();
                                    if t > now {
                                        std::thread::sleep(t - now);
                                    }
                                    c.settle(Ok(tsr_task::PortableValue::Undefined));
                                }
                                return;
                            }
                        }
                    }
                    // fire everything due
                    let now = Instant::now();
                    while heap.peek().is_some_and(|Reverse(e)| e.0 <= now) {
                        let Reverse(Entry(due, c)) = heap.pop().unwrap();
                        if timer_debug() {
                            eprintln!("late_us {}", now.saturating_duration_since(due).as_micros());
                        }
                        c.settle(Ok(tsr_task::PortableValue::Undefined));
                    }
                    w.wait(heap.peek().map(|Reverse(e)| e.0));
                }
            })
            .expect("spawn timer thread");
        TimerWheel { tx, waiter }
    })
}

#[cfg(test)]
mod timer_wait {
    use super::waiter::Waiter;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// A deadline wait sleeps roughly that long — it neither returns
    /// straight away nor overshoots the way a condvar park does.
    #[test]
    fn wait_honours_the_deadline() {
        let w = Waiter::new();
        let t = Instant::now();
        w.wait(Some(Instant::now() + Duration::from_millis(50)));
        let took = t.elapsed();
        assert!(took >= Duration::from_millis(45), "returned early: {took:?}");
        assert!(took < Duration::from_millis(200), "overshot: {took:?}");
    }

    /// A registration arriving mid-wait interrupts it, so a timer with a
    /// nearer deadline is not stuck behind the previous one.
    #[test]
    fn wake_interrupts_a_long_wait() {
        let w = Arc::new(Waiter::new());
        let w2 = w.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            w2.wake();
        });
        let t = Instant::now();
        w.wait(Some(Instant::now() + Duration::from_secs(30)));
        let took = t.elapsed();
        assert!(took < Duration::from_secs(5), "wake did not interrupt: {took:?}");
    }

    /// A wake that lands before the wait must not be missed, or a timer
    /// registered at just the wrong moment would wait out the old
    /// deadline instead of the new one.
    #[test]
    fn wake_before_wait_is_not_lost() {
        let w = Waiter::new();
        w.wake();
        let t = Instant::now();
        w.wait(Some(Instant::now() + Duration::from_secs(30)));
        assert!(t.elapsed() < Duration::from_secs(5), "lost wakeup");
    }
}
