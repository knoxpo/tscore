//! tscore — TypeScript Core CLI.
//!
//! Usage:
//!   tscore run <file.ts> [--workers N] [--max-heap MB] [--stats]
//!              [--dump-bytecode] [--no-stats-export]
//!   tscore top

mod help;

use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tsr_realm::StatsHub;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut it = args.iter();
    let out = help::Style::for_stdout();
    let err = help::Style::for_stderr();
    let wants_help = args.iter().skip(1).any(|a| a == "-h" || a == "--help");
    match it.next().map(String::as_str) {
        Some("run") if wants_help => {
            print!("{}", help::run(&out));
            return ExitCode::SUCCESS;
        }
        Some("run") => {}
        Some("top") if wants_help => {
            print!("{}", help::top(&out));
            return ExitCode::SUCCESS;
        }
        Some("top") => return top(),
        Some("--version") | Some("-V") => {
            println!("tscore {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        None | Some("-h") | Some("--help") => {
            print!("{}", help::root(&out));
            return ExitCode::SUCCESS;
        }
        Some("help") => {
            let page = match it.next().map(String::as_str) {
                None => help::root(&out),
                Some("run") => help::run(&out),
                Some("top") => help::top(&out),
                Some(other) => {
                    eprint!(
                        "{}",
                        help::usage_error(&err, &format!("no help for `{other}`"))
                    );
                    return ExitCode::from(2);
                }
            };
            print!("{page}");
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprint!(
                "{}",
                help::usage_error(
                    &err,
                    &format!("unknown command `{other}` (expected run, top, help)")
                )
            );
            return ExitCode::from(2);
        }
    }

    let mut file = None;
    let mut dump = false;
    let mut stats_summary = false;
    let mut stats_export = true;
    let mut workers: Option<usize> = None;
    let mut max_heap_mb: Option<usize> = None;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dump-bytecode" => dump = true,
            "--stats" => stats_summary = true,
            "--no-stats-export" => stats_export = false,
            "--workers" => {
                workers = it.next().and_then(|n| n.parse().ok());
                if workers.is_none() {
                    eprint!("{}", help::usage_error(&err, "--workers needs a number"));
                    return ExitCode::from(2);
                }
            }
            "--max-heap" => {
                max_heap_mb = it.next().and_then(|n| n.parse().ok());
                if max_heap_mb.is_none() {
                    eprint!(
                        "{}",
                        help::usage_error(&err, "--max-heap needs a number (MB)")
                    );
                    return ExitCode::from(2);
                }
            }
            f if !f.starts_with('-') && file.is_none() => file = Some(f.to_string()),
            other => {
                eprint!(
                    "{}",
                    help::usage_error(&err, &format!("unknown argument `{other}`"))
                );
                return ExitCode::from(2);
            }
        }
    }
    let Some(file) = file else {
        eprint!("{}", help::usage_error(&err, "run needs a file"));
        eprint!("{}", help::run(&err));
        return ExitCode::from(2);
    };

    let t_start = std::time::Instant::now();
    let phases = std::env::var_os("TSC_COMPILE_PHASES").is_some();
    let source = match std::fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tscore: cannot read {file}: {e}");
            return ExitCode::FAILURE;
        }
    };

    if phases {
        eprintln!("[cli] read={:?}", t_start.elapsed());
    }
    // module-syntax entries take the graph pipeline; plain scripts keep
    // the (parallel-frontend) single-file path untouched
    let program: Option<tsc_ir::Program> = if tsc_modules::looks_like_module(&source) {
        match tsc_modules::compile_graph(std::path::Path::new(&file)) {
            Ok(p) => Some(p),
            Err(e) => {
                let src = std::fs::read_to_string(&e.path).unwrap_or_default();
                let err = tsc_parser::CompileError {
                    msg: String::new(),
                    span_start: e.span_start,
                };
                let (line, col) = err.line_col(&src);
                eprintln!("{}:{line}:{col}: {}", e.path, e.msg);
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    let chunk = if program.is_some() {
        None
    } else {
        match tsc_parser::compile(&source, &file) {
            Ok(c) => Some(c),
            Err(e) => {
                let (line, col) = e.line_col(&source);
                eprintln!("{file}:{line}:{col}: {}", e.msg);
                return ExitCode::FAILURE;
            }
        }
    };

    if dump {
        let mut out = String::new();
        match (&program, &chunk) {
            (Some(p), _) => {
                for m in &p.modules {
                    out.push_str(&format!("== module {} ==\n", m.id));
                    tsc_ir::disassemble(&m.main, &mut out);
                }
            }
            (None, Some(c)) => tsc_ir::disassemble(&c.main, &mut out),
            _ => unreachable!(),
        }
        print!("{out}");
        return ExitCode::SUCCESS;
    }

    // fix the worker count BEFORE any thread can touch the shared pool
    tss_parallel::configure(workers);

    let hub = Arc::new(StatsHub::default());
    let _export = if stats_export {
        Some(StatsExport::start(hub.clone(), &file))
    } else {
        None
    };

    // dedicated 64MB-stack thread: 10k interpreter call depth needs more
    // Rust stack than the default main thread provides
    let realm_hub = hub.clone();
    let result = std::thread::Builder::new()
        .name("tscore-main".into())
        .stack_size(64 << 20)
        .spawn(move || {
            let mut realm = tsr_realm::Realm::new();
            realm.stats_hub = Some(realm_hub);
            realm.max_heap_bytes = max_heap_mb.map(|mb| mb * 1_000_000);
            tsr_io::install(&mut realm);
            tss_parallel::install(&mut realm, workers);
            tsr_actor::install(&mut realm);
            tss_async::install(&mut realm);
            tsr_channel::install(&mut realm);
            tsr_modules::install(&mut realm);
            tsr_fs::install(&mut realm);
            tsr_net::install(&mut realm);
            {
                if std::env::var_os("TSC_COMPILE_PHASES").is_some() {
                    eprintln!("[cli] to-exec={:?}", t_start.elapsed());
                }
                let r = match (&program, &chunk) {
                    (Some(p), _) => tsr_modules::run_program(&mut realm, p),
                    (None, Some(c)) => tsr_realm::interpreter::run_main(&mut realm, &c.main),
                    _ => unreachable!(),
                };
                if std::env::var_os("TSC_COMPILE_PHASES").is_some() {
                    use std::sync::atomic::Ordering::Relaxed;
                    eprintln!(
                        "[cli] filled={}/{} lazy protos",
                        tsc_ir::LAZY_FILLED.load(Relaxed),
                        tsc_ir::LAZY_TOTAL.load(Relaxed)
                    );
                }
                r
            }
        })
        .expect("spawn main realm thread")
        .join()
        .expect("main realm thread panicked");

    if stats_summary {
        print_summary(&hub);
    }

    match result {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            // attribute to the originating module file when known
            let (err_file, err_src) = match &e.source {
                Some(src) if !src.is_empty() && src.as_ref() != file => (
                    src.to_string(),
                    std::fs::read_to_string(src.as_ref()).unwrap_or_default(),
                ),
                _ => (file.clone(), source.clone()),
            };
            match e.span {
                Some(span) => {
                    let err = tsc_parser::CompileError {
                        msg: String::new(),
                        span_start: span,
                    };
                    let (line, col) = err.line_col(&err_src);
                    eprintln!("{err_file}:{line}:{col}: runtime error: {}", e.msg);
                }
                None => eprintln!("{err_file}: runtime error: {}", e.msg),
            }
            ExitCode::FAILURE
        }
    }
}

fn print_summary(hub: &StatsHub) {
    let ps = tss_parallel::pool_stats_if_started().unwrap_or_default();
    eprintln!("--- tscore stats ---");
    eprintln!("workers:        {}", ps.workers);
    eprintln!("tasks executed: {}", ps.tasks_executed);
    eprintln!("steals:         {}", ps.steals);
    eprintln!("parks:          {}", ps.parks);
    eprintln!("actors (live):  {}", tsr_actor::ACTOR_COUNT.load(Relaxed));
    eprintln!(
        "gc:             {} minor / {} major, last pause {}us, max {}us",
        hub.gc_minor.load(Relaxed),
        hub.gc_major.load(Relaxed),
        hub.gc_last_pause_us.load(Relaxed),
        hub.gc_max_pause_us.load(Relaxed),
    );
    eprintln!(
        "heap:           {} slots, ~{:.1} MB live",
        hub.heap_slots.load(Relaxed),
        hub.live_bytes.load(Relaxed) as f64 / 1e6
    );
}

/// Writes a key\tvalue snapshot to $TMPDIR/tscore-stats-<pid>.txt every
/// 500ms; the file is removed on drop (process exit).
struct StatsExport {
    path: std::path::PathBuf,
}

impl StatsExport {
    fn start(hub: Arc<StatsHub>, script: &str) -> StatsExport {
        let path = std::env::temp_dir().join(format!("tscore-stats-{}.txt", std::process::id()));
        let p = path.clone();
        let script = script.to_string();
        let started = Instant::now();
        std::thread::spawn(move || loop {
            let ps = tss_parallel::pool_stats_if_started().unwrap_or_default();
            let body = format!(
                "pid\t{}\nscript\t{}\nuptime_s\t{}\nworkers\t{}\ntasks\t{}\nsteals\t{}\n\
                 actors\t{}\ngc_minor\t{}\ngc_major\t{}\ngc_last_us\t{}\nheap_slots\t{}\n\
                 live_bytes\t{}\nstamp\t{}\n",
                std::process::id(),
                script,
                started.elapsed().as_secs(),
                ps.workers,
                ps.tasks_executed,
                ps.steals,
                tsr_actor::ACTOR_COUNT.load(Relaxed),
                hub.gc_minor.load(Relaxed),
                hub.gc_major.load(Relaxed),
                hub.gc_last_pause_us.load(Relaxed),
                hub.heap_slots.load(Relaxed),
                hub.live_bytes.load(Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            );
            let _ = std::fs::write(&p, body);
            std::thread::sleep(Duration::from_millis(500));
        });
        StatsExport { path }
    }
}

impl Drop for StatsExport {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `tscore top` — live table of running tscore processes.
fn top() -> ExitCode {
    let tty = std::io::stdout().is_terminal();
    let st = help::Style::for_stdout();
    loop {
        let mut rows: Vec<Vec<String>> = Vec::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !name.starts_with("tscore-stats-") || !name.ends_with(".txt") {
                    continue;
                }
                let Ok(body) = std::fs::read_to_string(e.path()) else {
                    continue;
                };
                let mut kv = std::collections::HashMap::new();
                for line in body.lines() {
                    if let Some((k, v)) = line.split_once('\t') {
                        kv.insert(k.to_string(), v.to_string());
                    }
                }
                let stamp: u64 = kv.get("stamp").and_then(|s| s.parse().ok()).unwrap_or(0);
                let age = now.saturating_sub(stamp);
                if age > 10 {
                    let _ = std::fs::remove_file(e.path()); // dead process leftover
                    continue;
                }
                let get = |k: &str| kv.get(k).cloned().unwrap_or_else(|| "-".into());
                let script = std::path::Path::new(&get("script"))
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| get("script"));
                let live_mb = kv
                    .get("live_bytes")
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|b| format!("{:.1}M", b / 1e6))
                    .unwrap_or_else(|| "-".into());
                rows.push(vec![
                    get("pid"),
                    if age > 3 {
                        format!("{script} (stale)")
                    } else {
                        script
                    },
                    format!("{}s", get("uptime_s")),
                    get("workers"),
                    get("tasks"),
                    get("steals"),
                    live_mb,
                    format!("{}/{}", get("gc_minor"), get("gc_major")),
                    format!("{}us", get("gc_last_us")),
                    get("actors"),
                ]);
            }
        }
        let frame = render_table(&st, &rows);
        let mut out = std::io::stdout();
        if tty {
            // home + erase-to-end-of-screen, then the whole frame in one write: no tearing
            let _ = out.write_all(b"\x1b[H\x1b[J");
        }
        let _ = out.write_all(frame.as_bytes());
        let _ = out.flush();
        if !tty {
            // piped or redirected: one plain frame, no escape codes, then exit
            return ExitCode::SUCCESS;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

const TOP_HEADERS: [&str; 10] = [
    "PID", "SCRIPT", "UP", "WRK", "TASKS", "STEALS", "HEAP", "GC m/M", "PAUSE", "ACTORS",
];

/// Renders `top` as a header line plus a box-drawn table.
fn render_table(st: &help::Style, rows: &[Vec<String>]) -> String {
    let w = |s: &str| s.chars().count(); // chars, not bytes: a non-ASCII script name would skew every column
    let mut widths: Vec<usize> = TOP_HEADERS.iter().map(|h| w(h)).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(w(c));
        }
    }
    // PID and SCRIPT are labels; the rest are numbers, right-aligned so digits line up.
    // Pad on the plain text, colour afterwards, so escapes never count toward width.
    let pad = |c: &str, i: usize| {
        let gap = " ".repeat(widths[i] - w(c));
        if i < 2 {
            format!("{c}{gap}")
        } else {
            format!("{gap}{c}")
        }
    };
    let edge = |s: &str| st.dim(&st.rgb(help::CORAL, s));
    let rule = |l: &str, m: &str, r: &str| {
        let mut s = String::from(l);
        for (i, width) in widths.iter().enumerate() {
            if i > 0 {
                s.push_str(m);
            }
            s.push_str(&"─".repeat(width + 2));
        }
        s.push_str(r);
        format!("  {}\n", edge(&s))
    };
    let sep = edge("│");
    let row = |cells: &[String], paint: &dyn Fn(usize, &str) -> String| {
        let mut s = format!("  {sep}");
        for (i, c) in cells.iter().enumerate() {
            s.push_str(&format!(" {} {sep}", paint(i, &pad(c, i))));
        }
        s.push('\n');
        s
    };

    let mut out = format!(
        "\n  {}  {}\n\n",
        st.bold(&st.rgb(help::CORAL, "tscore top")),
        st.dim(&format!(
            "{} process{} · refresh 1s · ctrl-c to quit",
            rows.len(),
            if rows.len() == 1 { "" } else { "es" }
        ))
    );
    out.push_str(&rule("╭", "┬", "╮"));
    let headers: Vec<String> = TOP_HEADERS.iter().map(|s| s.to_string()).collect();
    out.push_str(&row(&headers, &|_, c| st.bold(&st.rgb(help::CYAN, c))));
    out.push_str(&rule("├", "┼", "┤"));
    if rows.is_empty() {
        // ponytail: one spanning cell, so the frame keeps its shape when nothing is running
        let inner: usize = widths.iter().map(|x| x + 3).sum::<usize>() - 1;
        let msg = "no running tscore processes";
        out.push_str(&format!(
            "  {sep} {}{}{sep}\n",
            st.dim(msg),
            " ".repeat(inner.saturating_sub(w(msg) + 1))
        ));
    }
    for r in rows {
        let stale = r[1].ends_with("(stale)");
        let slow_pause = r[8].trim_end_matches("us").parse::<u64>().unwrap_or(0) > 10_000;
        out.push_str(&row(r, &|i, c| match i {
            _ if stale => st.dim(c),
            1 => st.bold(c),
            8 if slow_pause => st.rgb(help::YELLOW, c),
            _ => c.to_string(),
        }));
    }
    out.push_str(&rule("╰", "┴", "╯"));
    out
}

#[cfg(test)]
mod tests {
    use super::{help, render_table};

    fn plain(rows: &[Vec<String>]) -> String {
        render_table(&help::Style::plain(), rows)
    }

    /// Every line of a frame must be the same display width, or the box is broken.
    fn assert_rectangular(frame: &str) {
        let lines: Vec<&str> = frame
            .lines()
            .filter(|l| l.trim_start().starts_with(['╭', '│', '├', '╰']))
            .collect();
        let w = lines[0].chars().count();
        for l in &lines {
            assert_eq!(l.chars().count(), w, "ragged line: {l:?}");
        }
    }

    #[test]
    fn empty_table_is_rectangular() {
        let f = plain(&[]);
        assert_rectangular(&f);
        assert!(f.contains("no running tscore processes"));
        assert!(f.contains("0 processes · refresh 1s"));
        assert!(!f.contains('\x1b'));
    }

    #[test]
    fn rows_widen_columns_and_stay_rectangular() {
        let mk = |pid: &str, script: &str| {
            vec![
                pid, script, "38s", "4", "10", "2", "1.5M", "3/1", "120us", "7",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        };
        let f = plain(&[mk("412", "a-very-long-script-name.ts"), mk("9", "b.ts")]);
        assert_rectangular(&f);
        assert!(f.contains("a-very-long-script-name.ts"));
        assert!(f.contains("2 processes · refresh 1s"));
    }

    #[test]
    fn one_process_is_singular() {
        let r = vec![vec![String::from("1"); 10]];
        assert!(plain(&r).contains("1 process · refresh 1s"));
    }
}
