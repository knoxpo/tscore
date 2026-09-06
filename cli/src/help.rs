//! Banner, help pages and styled messages for the `tscore` CLI.
//!
//! Color is 24-bit SGR, on only when the target stream is a terminal and
//! `NO_COLOR` is unset. Every page renders the same layout without color.

use std::io::IsTerminal;

pub(crate) type Rgb = (u8, u8, u8);

pub(crate) const CORAL: Rgb = (218, 119, 86);
const ORANGE: Rgb = (235, 150, 75);
const GOLD: Rgb = (245, 190, 90);
pub(crate) const CYAN: Rgb = (99, 200, 220);
pub(crate) const YELLOW: Rgb = (240, 200, 100);
const GREEN: Rgb = (120, 200, 120);
const RED: Rgb = (230, 90, 90);

const LOGO: [&str; 6] = [
    "████████╗███████╗ ██████╗ ██████╗ ██████╗ ███████╗",
    "╚══██╔══╝██╔════╝██╔════╝██╔═══██╗██╔══██╗██╔════╝",
    "   ██║   ███████╗██║     ██║   ██║██████╔╝█████╗",
    "   ██║   ╚════██║██║     ██║   ██║██╔══██╗██╔══╝",
    "   ██║   ███████║╚██████╗╚██████╔╝██║  ██║███████╗",
    "   ╚═╝   ╚══════╝ ╚═════╝ ╚═════╝ ╚═╝  ╚═╝╚══════╝",
];
const WIDTH: usize = 50; // logo and box share one width
const TAGLINE: &str = "parallel TypeScript runtime";

pub struct Style {
    on: bool,
}

impl Style {
    pub fn for_stdout() -> Self {
        Self::new(std::io::stdout().is_terminal())
    }
    pub fn for_stderr() -> Self {
        Self::new(std::io::stderr().is_terminal())
    }
    #[cfg(test)]
    pub(crate) fn plain() -> Self {
        Style { on: false }
    }
    fn new(tty: bool) -> Self {
        Style {
            on: tty && std::env::var_os("NO_COLOR").is_none(),
        }
    }
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub(crate) fn rgb(&self, (r, g, b): Rgb, s: &str) -> String {
        self.wrap(&format!("38;2;{r};{g};{b}"), s)
    }
    pub(crate) fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }
    pub(crate) fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }
    pub(crate) fn header(&self, s: &str) -> String {
        self.bold(&self.rgb(CORAL, s))
    }
}

fn lerp(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

/// Logo with a vertical coral→orange→gold gradient, then the welcome box.
pub fn banner(st: &Style) -> String {
    let mut out = String::from("\n");
    for (i, line) in LOGO.iter().enumerate() {
        // two segments: rows 0-2 coral→orange, rows 3-5 orange→gold
        let color = if i < 3 {
            lerp(CORAL, ORANGE, i as f32 / 2.0)
        } else {
            lerp(ORANGE, GOLD, (i - 3) as f32 / 2.0)
        };
        out.push_str(&format!("  {}\n", st.rgb(color, line)));
    }
    out.push('\n');

    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cwd = match std::env::var("HOME") {
        Ok(h) if cwd.starts_with(&h) => format!("~{}", &cwd[h.len()..]),
        _ => cwd,
    };
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    // (plain text for width, styled text for display)
    let rows: [(String, String); 4] = [
        (
            format!("TypeScript Core  {version}"),
            format!(
                "{}  {}",
                st.bold("TypeScript Core"),
                st.rgb(GREEN, &version)
            ),
        ),
        (TAGLINE.into(), st.dim(TAGLINE)),
        (String::new(), String::new()),
        (format!("cwd: {cwd}"), st.dim(&format!("cwd: {cwd}"))),
    ];
    let bar = |l: &str, r: &str| st.rgb(CORAL, &format!("{l}{}{r}", "─".repeat(WIDTH)));
    let side = st.rgb(CORAL, "│");
    out.push_str(&format!("  {}\n", bar("╭", "╮")));
    for (plain, styled) in &rows {
        let pad = WIDTH.saturating_sub(plain.chars().count() + 2);
        out.push_str(&format!("  {side}  {styled}{}{side}\n", " ".repeat(pad)));
    }
    out.push_str(&format!("  {}\n", bar("╰", "╯")));
    out
}

/// `  TITLE\n    left    right\n...` — left column padded to the widest entry + 4.
fn section(st: &Style, title: &str, rows: &[(&str, &str)], left: Rgb) -> String {
    let w = rows
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0)
        + 4;
    let mut out = format!("\n  {}\n", st.header(title));
    for (l, r) in rows {
        if r.is_empty() {
            out.push_str(&format!("    {}\n", st.rgb(left, l)));
            continue;
        }
        let pad = " ".repeat(w - l.chars().count());
        out.push_str(&format!("    {}{pad}{r}\n", st.rgb(left, l)));
    }
    out
}

pub fn root(st: &Style) -> String {
    let mut out = banner(st);
    out.push_str(&section(
        st,
        "USAGE",
        &[("tscore <command> [options]", "")],
        CYAN,
    ));
    out.push_str(&section(
        st,
        "COMMANDS",
        &[
            ("run <file.ts>", "Execute a TypeScript file"),
            ("top", "Live table of running tscore processes"),
            ("help [command]", "Show help for a command"),
        ],
        CYAN,
    ));
    out.push_str(&section(
        st,
        "OPTIONS",
        &[
            ("-h, --help", "Show this help"),
            ("-V, --version", "Print version"),
        ],
        YELLOW,
    ));
    out.push_str(&format!(
        "\n  {}\n\n",
        st.dim("Run `tscore help run` for run's flags.")
    ));
    out
}

pub fn run(st: &Style) -> String {
    let mut out = format!(
        "\n  {}  {}\n",
        st.bold(&st.rgb(CORAL, "tscore run")),
        st.dim("Execute a TypeScript file")
    );
    out.push_str(&section(
        st,
        "USAGE",
        &[("tscore run <file.ts> [flags]", "")],
        CYAN,
    ));
    out.push_str(&section(
        st,
        "FLAGS",
        &[
            (
                "--workers N",
                "Executors in the work-stealing pool, incl. the caller (default: CPU count)",
            ),
            (
                "--max-heap MB",
                "Live-byte limit for the main realm; clean runtime error when exceeded",
            ),
            (
                "--stats",
                "Print an exit summary to stderr (workers, tasks, steals, GC, heap)",
            ),
            (
                "--dump-bytecode",
                "Disassemble the compiled program to stdout and exit without running",
            ),
            (
                "--no-stats-export",
                "Skip the 500 ms snapshot that `tscore top` reads (benchmarks, tests)",
            ),
            ("-h, --help", "Show this help"),
        ],
        YELLOW,
    ));
    out.push_str(&section(
        st,
        "EXIT CODES",
        &[
            ("0", "Program ran to completion"),
            (
                "1",
                "Could not read the file, compile error, or runtime error",
            ),
            (
                "2",
                "Usage error: bad flag, missing file, non-numeric value",
            ),
        ],
        GREEN,
    ));
    out.push('\n');
    out
}

pub fn top(st: &Style) -> String {
    let mut out = format!(
        "\n  {}  {}\n",
        st.bold(&st.rgb(CORAL, "tscore top")),
        st.dim("Live table of every running tscore process")
    );
    out.push_str(&section(st, "USAGE", &[("tscore top", "")], CYAN));
    out.push_str(&section(
        st,
        "COLUMNS",
        &[
            ("UP", "Seconds since the process started"),
            ("WRK", "Worker count the pool settled on"),
            (
                "TASKS / STEALS",
                "Tasks executed / tasks stolen across workers",
            ),
            ("HEAP", "Live bytes after the last major GC"),
            ("GC m/M", "Minor / major collections"),
            ("PAUSE", "Last GC pause in microseconds"),
            ("ACTORS", "Live actors"),
        ],
        YELLOW,
    ));
    out.push_str(&format!(
        "\n  {}\n  {}\n\n",
        st.dim("Redraws in place once a second; rows go (stale) after 3 s and drop after 10 s."),
        st.dim("When stdout is not a terminal it prints one plain frame and exits.")
    ));
    out
}

pub fn usage_error(st: &Style, msg: &str) -> String {
    format!(
        "{} {msg}\n  {}\n",
        st.bold(&st.rgb(RED, "error:")),
        st.dim("run `tscore --help` for usage")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_lines_share_a_width() {
        for l in LOGO {
            assert!(l.chars().count() <= WIDTH, "{l:?}");
        }
    }

    #[test]
    fn plain_style_emits_no_escapes() {
        let st = Style { on: false };
        for page in [root(&st), run(&st), top(&st), usage_error(&st, "x")] {
            assert!(!page.contains('\x1b'));
        }
    }

    #[test]
    fn root_lists_every_command() {
        let page = root(&Style { on: false });
        for cmd in ["run <file.ts>", "top", "help [command]", "--version"] {
            assert!(page.contains(cmd), "missing {cmd}");
        }
    }

    #[test]
    fn welcome_box_is_rectangular() {
        let page = banner(&Style { on: false });
        let box_lines: Vec<&str> = page
            .lines()
            .filter(|l| l.trim_start().starts_with(['╭', '│', '╰']))
            .collect();
        assert_eq!(box_lines.len(), 6);
        for l in &box_lines {
            assert_eq!(l.chars().count(), WIDTH + 4, "{l:?}");
        }
    }
}
