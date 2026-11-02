//! How far an export has got, on stderr. Each step prints its name, numbered ("[3/9]"), then a
//! status line with a progress bar. In a terminal the status line is redrawn in place several times
//! a second; elsewhere, as in a log file, a new one is added every 10 seconds.

use std::{
    io::IsTerminal,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed},
        OnceLock,
    },
    time::Instant,
};

/// The width of the progress bar, in characters.
const BAR: u64 = 20;

/// Whether stderr is a terminal, where a status line can be redrawn in place.
fn terminal() -> bool {
    static TERMINAL: OnceLock<bool> = OnceLock::new();
    *TERMINAL.get_or_init(|| std::io::stderr().is_terminal())
}

/// The width of the terminal. A redrawn line must not be wider: the terminal would wrap it, and only
/// its last part would be redrawn.
fn width() -> usize {
    // SAFETY: TIOCGWINSZ only writes the window size into `size`.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let found = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    if found && size.ws_col > 0 {
        size.ws_col as usize
    } else {
        80
    }
}

/// Numbers the steps of an export, so that each says how far through the export it is.
pub struct Steps {
    next: AtomicUsize,
    total: usize,
}

impl Steps {
    pub fn new(total: usize) -> Self {
        Self { next: AtomicUsize::new(1), total }
    }

    /// The name of the next step, with its number: "[3/9] writing masterstate.boc".
    pub fn next(&self, what: &str) -> String {
        format!("[{}/{}] {what}", self.next.fetch_add(1, Relaxed), self.total)
    }
}

/// What a step counts.
#[derive(Clone, Copy, PartialEq)]
pub enum Unit {
    Bytes,
    Cells,
    Accounts,
    Blocks,
}

/// Counts what a step has done and shows it: the share done, the rate and the time left when the
/// total is known, the count, the rate and the time so far when it isn't.
pub struct Progress {
    unit: Unit,
    total: Option<u64>,
    estimated: bool,
    done: AtomicU64,
    start: Instant,
    /// How often the status line is shown, in milliseconds.
    every: u64,
    /// Milliseconds after the start when the next status line is due.
    next: AtomicU64,
    /// A status line is on the terminal without its line end.
    open: AtomicBool,
}

impl Progress {
    /// Starts a step: prints its name.
    pub fn new(what: impl AsRef<str>, unit: Unit, total: Option<u64>) -> Self {
        eprintln!("  {}", what.as_ref());
        let every = if terminal() { 250 } else { 10_000 };
        Self {
            unit,
            total,
            estimated: false,
            done: AtomicU64::new(0),
            start: Instant::now(),
            every,
            next: AtomicU64::new(every),
            open: AtomicBool::new(false),
        }
    }

    /// Marks the total as a guess, so it is shown as "about".
    pub fn estimated(mut self) -> Self {
        self.estimated = true;
        self
    }

    pub fn add(&self, n: u64) {
        let done = self.done.fetch_add(n, Relaxed) + n;
        // Reading the clock for every cell would cost more than the counting.
        if n == 1 && !done.is_multiple_of(1024) {
            return;
        }
        let elapsed = self.start.elapsed().as_millis() as u64;
        let next = self.next.load(Relaxed);
        if elapsed >= next && self.next.compare_exchange(next, elapsed + self.every, Relaxed, Relaxed).is_ok() {
            self.show(&self.status(done, elapsed), false);
        }
    }

    pub fn done(&self) -> u64 {
        self.done.load(Relaxed)
    }

    /// Shows what the step did and how long it took.
    pub fn finish(&self) {
        self.show(&self.summary(self.done(), self.start.elapsed().as_secs_f64()), true);
    }

    /// Prints a status line: in a terminal over the previous one, cut to the terminal's width
    /// unless it is the last.
    fn show(&self, status: &str, last: bool) {
        let line = format!("    {status}");
        if !terminal() {
            eprintln!("{line}");
        } else if last {
            eprintln!("\r\x1b[2K{line}");
            self.open.store(false, Relaxed);
        } else {
            let line: String = line.chars().take(width() - 1).collect();
            eprint!("\r\x1b[2K{line}");
            self.open.store(true, Relaxed);
        }
    }

    fn status(&self, done: u64, elapsed_ms: u64) -> String {
        let rate = done as f64 * 1000.0 / elapsed_ms.max(1) as f64;
        let (amount, per_second) = (self.amount(done, false), self.amount(rate as u64, false));
        match self.total {
            // The time left comes first after the share done: a narrow terminal cuts off the end.
            Some(total) if done < total => {
                let left = match done {
                    0 => String::new(),
                    _ => format!("about {} left, ", duration((total - done) as f64 / rate)),
                };
                let about = if self.estimated { "about " } else { "" };
                let (bar, percent) = (bar(done * BAR / total), done * 100 / total);
                let total = self.amount(total, true);
                format!("{bar} {percent:>3}%  {left}{amount} of {about}{total}, {per_second}/s")
            }
            _ => {
                let elapsed = duration(elapsed_ms as f64 / 1000.0);
                format!("{}, {per_second}/s, {elapsed} so far", self.amount(done, true))
            }
        }
    }

    fn summary(&self, done: u64, seconds: f64) -> String {
        let summary = format!("{} in {}", self.amount(done, true), duration(seconds));
        match self.total {
            Some(total) if !self.estimated && done == total => format!("{} 100%  {summary}", bar(BAR)),
            _ => summary,
        }
    }

    /// An amount of what the step counts: "6.62 GB", "1,234,567", or "1,234,567 cells" with the noun.
    fn amount(&self, n: u64, noun: bool) -> String {
        let noun = match (self.unit, noun) {
            (Unit::Bytes, _) => return bytes(n),
            (_, false) => return grouped(n),
            (Unit::Cells, true) => "cell",
            (Unit::Accounts, true) => "account",
            (Unit::Blocks, true) => "block",
        };
        format!("{} {noun}{}", grouped(n), if n == 1 { "" } else { "s" })
    }
}

impl Drop for Progress {
    /// Ends a status line left on the terminal when a step fails, so the next message starts on a
    /// line of its own.
    fn drop(&mut self) {
        if self.open.load(Relaxed) {
            eprintln!();
        }
    }
}

/// "[########------------]" with `filled` of the 20 places filled.
fn bar(filled: u64) -> String {
    let filled = filled.min(BAR) as usize;
    format!("[{}{}]", "#".repeat(filled), "-".repeat(BAR as usize - filled))
}

/// 1234567 as "1,234,567".
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() * 4 / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A size in decimal units: "6.62 GB", "412.3 MB", "24.7 kB".
pub fn bytes(n: u64) -> String {
    match n {
        1_000_000_000.. => format!("{:.2} GB", n as f64 / 1e9),
        1_000_000.. => format!("{:.1} MB", n as f64 / 1e6),
        _ => format!("{:.1} kB", n as f64 / 1e3),
    }
}

/// "42 s", "3 min 5 s", "2 h 14 min".
pub fn duration(seconds: f64) -> String {
    let seconds = seconds.round() as u64;
    match seconds {
        0..=59 => format!("{seconds} s"),
        60..=3599 if seconds.is_multiple_of(60) => format!("{} min", seconds / 60),
        60..=3599 => format!("{} min {} s", seconds / 60, seconds % 60),
        _ => format!("{} h {} min", seconds / 3600, seconds % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_and_times_read_easily() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(126_912_345), "126,912,345");
        assert_eq!(bytes(6_620_000_000), "6.62 GB");
        assert_eq!(bytes(412_345_678), "412.3 MB");
        assert_eq!(bytes(24_659), "24.7 kB");
        assert_eq!(duration(42.4), "42 s");
        assert_eq!(duration(120.0), "2 min");
        assert_eq!(duration(185.0), "3 min 5 s");
        assert_eq!(duration(8040.0), "2 h 14 min");
    }

    #[test]
    fn status_lines_show_the_share_done_and_the_time_left() {
        let progress = Progress::new("reading", Unit::Cells, Some(1000)).estimated();
        assert_eq!(
            progress.status(250, 10_000),
            "[#####---------------]  25%  about 30 s left, 250 of about 1,000 cells, 25/s"
        );
        assert_eq!(progress.status(0, 0), "[--------------------]   0%  0 of about 1,000 cells, 0/s");
        // an estimate can fall short
        assert_eq!(progress.status(1200, 2_000), "1,200 cells, 600/s, 2 s so far");
        assert_eq!(progress.summary(1200, 3.0), "1,200 cells in 3 s");
        let progress = Progress::new("reading it", Unit::Bytes, Some(6_620_000_000));
        assert_eq!(
            progress.status(1_655_000_000, 4_000),
            "[#####---------------]  25%  about 12 s left, 1.66 GB of 6.62 GB, 413.8 MB/s"
        );
        assert_eq!(progress.summary(6_620_000_000, 16.0), "[####################] 100%  6.62 GB in 16 s");
        let progress = Progress::new("counting", Unit::Accounts, None);
        assert_eq!(progress.status(5000, 2_000), "5,000 accounts, 2,500/s, 2 s so far");
        assert_eq!(progress.summary(5000, 2.0), "5,000 accounts in 2 s");
        let progress = Progress::new("building", Unit::Blocks, Some(1));
        assert_eq!(progress.summary(1, 0.2), "[####################] 100%  1 block in 0 s");
    }

    #[test]
    fn steps_are_numbered() {
        let steps = Steps::new(9);
        assert_eq!(steps.next("reading it"), "[1/9] reading it");
        assert_eq!(steps.next("hashing its cells"), "[2/9] hashing its cells");
    }
}
