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

/// One process-wide timer thread: min-heap of deadlines, parks until the
/// nearest one (or a new registration arrives).
struct TimerWheel {
    tx: std::sync::mpsc::Sender<(std::time::Instant, tsr_realm::Completer)>,
}

impl TimerWheel {
    fn send(&self, t: (std::time::Instant, tsr_realm::Completer)) {
        let _ = self.tx.send(t);
    }
}

fn timer_wheel() -> &'static TimerWheel {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Instant;
    static WHEEL: std::sync::OnceLock<TimerWheel> = std::sync::OnceLock::new();
    WHEEL.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<(Instant, tsr_realm::Completer)>();
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
                    // fire everything due
                    let now = Instant::now();
                    while heap.peek().is_some_and(|Reverse(e)| e.0 <= now) {
                        let Reverse(Entry(_, c)) = heap.pop().unwrap();
                        c.settle(Ok(tsr_task::PortableValue::Undefined));
                    }
                    // wait for the next deadline or a new timer
                    let wait = heap
                        .peek()
                        .map(|Reverse(e)| e.0.saturating_duration_since(Instant::now()));
                    match wait {
                        None => match rx.recv() {
                            Ok((t, c)) => heap.push(Reverse(Entry(t, c))),
                            Err(_) => return,
                        },
                        Some(d) => match rx.recv_timeout(d) {
                            Ok((t, c)) => heap.push(Reverse(Entry(t, c))),
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => {
                                // drain remaining deadlines, then exit
                                while let Some(Reverse(Entry(t, c))) = heap.pop() {
                                    let now = Instant::now();
                                    if t > now {
                                        std::thread::sleep(t - now);
                                    }
                                    c.settle(Ok(tsr_task::PortableValue::Undefined));
                                }
                                return;
                            }
                        },
                    }
                }
            })
            .expect("spawn timer thread");
        TimerWheel { tx }
    })
}
