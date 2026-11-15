use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub trait Clock {
    fn now_ms(&self) -> u64;
    fn sleep_ms(&self, ms: u64);
}

pub struct SystemClock;

/// Wall epoch ms at the first read, then monotonic: a wall-clock step never shortens a wait.
fn clock_start() -> &'static (Instant, u64) {
    static START: OnceLock<(Instant, u64)> = OnceLock::new();
    START.get_or_init(|| {
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        (Instant::now(), epoch)
    })
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        let (start, epoch) = clock_start();
        epoch + start.elapsed().as_millis() as u64
    }
    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}
