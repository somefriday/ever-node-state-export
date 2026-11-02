//! Progress lines for the long steps of an export, printed to stderr.

use std::{
    sync::atomic::{AtomicU64, Ordering::Relaxed},
    time::Instant,
};

/// How often a step reports how far it has got, in milliseconds.
const EVERY_MS: u64 = 10_000;

/// What a step counts.
#[derive(Clone, Copy, PartialEq)]
pub enum Unit {
    Cells,
    Bytes,
    /// Units that mean nothing to the user: only the share done and the time left are shown.
    Steps,
}

/// Counts what a step has done and prints a line every 10 seconds: the count, the rate and, when
/// the total is known, the share done and the time left.
pub struct Progress {
    what: String,
    unit: Unit,
    total: Option<u64>,
    estimated: bool,
    done: AtomicU64,
    start: Instant,
    /// Milliseconds after the start when the next line is due.
    next: AtomicU64,
}

impl Progress {
    pub fn new(what: impl Into<String>, unit: Unit, total: Option<u64>) -> Self {
        Self {
            what: what.into(),
            unit,
            total,
            estimated: false,
            done: AtomicU64::new(0),
            start: Instant::now(),
            next: AtomicU64::new(EVERY_MS),
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
        if n == 1 && done % 4096 != 0 {
            return;
        }
        let elapsed = self.start.elapsed().as_millis() as u64;
        let next = self.next.load(Relaxed);
        if elapsed >= next && self.next.compare_exchange(next, elapsed + EVERY_MS, Relaxed, Relaxed).is_ok() {
            eprintln!("  {}", self.line(done, elapsed));
        }
    }

    pub fn done(&self) -> u64 {
        self.done.load(Relaxed)
    }

    /// Prints what the step did and how long it took.
    pub fn finish(&self) {
        let seconds = self.start.elapsed().as_secs_f64();
        match self.unit {
            Unit::Steps => eprintln!("  {}: done in {}", self.what, duration(seconds)),
            _ => eprintln!("  {}: {} in {}", self.what, self.amount(self.done()), duration(seconds)),
        }
    }

    fn line(&self, done: u64, elapsed_ms: u64) -> String {
        let rate = done as f64 * 1000.0 / elapsed_ms.max(1) as f64;
        let mut line = format!("{}:", self.what);
        match self.total {
            Some(total) if done < total => {
                let percent = done * 100 / total;
                if self.unit == Unit::Steps {
                    line += &format!(" {percent}%");
                } else {
                    let about = if self.estimated { "about " } else { "" };
                    let (done, total, rate) = (self.amount(done), self.amount(total), self.amount(rate as u64));
                    line += &format!(" {done} of {about}{total} ({percent}%), {rate}/s");
                }
                line += &format!(", about {} left", duration((total - done) as f64 / rate.max(1e-9)));
            }
            _ if self.unit == Unit::Steps => line += " almost done",
            _ => line += &format!(" {}, {}/s", self.amount(done), self.amount(rate as u64)),
        }
        line
    }

    fn amount(&self, n: u64) -> String {
        match self.unit {
            Unit::Bytes => bytes(n),
            _ => grouped(n),
        }
    }
}

/// 1234567 as "1,234,567".
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() * 4 / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
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
        60..=3599 if seconds % 60 == 0 => format!("{} min", seconds / 60),
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
    fn lines_show_the_share_done_and_the_time_left() {
        let progress = Progress::new("reading", Unit::Cells, Some(1000)).estimated();
        assert_eq!(progress.line(250, 10_000), "reading: 250 of about 1,000 (25%), 25/s, about 30 s left");
        let progress = Progress::new("reading it", Unit::Bytes, Some(6_620_000_000));
        assert_eq!(
            progress.line(1_655_000_000, 4_000),
            "reading it: 1.66 GB of 6.62 GB (25%), 413.8 MB/s, about 12 s left"
        );
        let progress = Progress::new("writing x", Unit::Steps, Some(300));
        assert_eq!(progress.line(150, 60_000), "writing x: 50%, about 1 min left");
        let progress = Progress::new("reading", Unit::Cells, None);
        assert_eq!(progress.line(5000, 2_000), "reading: 5,000, 2,500/s");
    }
}
