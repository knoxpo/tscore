//! tsr-io
//!
//! M1 native surface: console, time. The M2 event loop (io_uring/kqueue)
//! lives here later.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};
use tsr_memory::Value;
use tsr_realm::{Realm, RtError};

fn num_arg(args: &[Value], i: usize, who: &str) -> Result<f64, RtError> {
    match args.get(i) {
        Some(v) if v.is_number() => Ok(v.as_number()),
        v => Err(RtError::new(format!(
            "{who}: expected number, got {}",
            v.map_or("nothing", |v| v.type_of())
        ))),
    }
}

/// Install console/Math/Date/performance and string helpers into a realm.
pub fn install(realm: &mut Realm) {
    let log = realm.add_native(|realm, args| {
        let line = args
            .iter()
            .map(|v| v.display(&realm.heap))
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
            ($name, $realm.add_native(move |_, args| {
                Ok(Value::number($f(num_arg(args, 0, $name)?)))
            }))
        };
    }
    macro_rules! math2 {
        ($realm:ident, $name:literal, $f:expr) => {
            ($name, $realm.add_native(move |_, args| {
                Ok(Value::number($f(
                    num_arg(args, 0, $name)?,
                    num_arg(args, 1, $name)?,
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

    // hidden helper: `s.charCodeAt(i)` compiles to `__charCodeAt(s, i)`
    let char_code_at = realm.add_native(|realm, args| {
        let s = match args.first().and_then(|v| v.as_str_ref()) {
            Some(r) => realm.heap.str_at(r).clone(),
            None => {
                let v = args.first();
                return Err(RtError::new(format!(
                    "charCodeAt on {}",
                    v.map_or("nothing", |v| v.type_of())
                )))
            }
        };
        let i = num_arg(args, 1, "charCodeAt")? as usize;
        Ok(match s.chars().nth(i) {
            Some(c) => Value::number(c as u32 as f64),
            None => Value::number(f64::NAN),
        })
    });
    realm.set_global("__charCodeAt", char_code_at);
}
