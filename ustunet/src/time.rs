use smoltcp::time::Instant as SmolInstant;
use std::ops::Add;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Clock {
    start_time: Instant,
}

impl Clock {
    pub fn new() -> Clock {
        Clock {
            start_time: Instant::now(),
        }
    }
    /// Returns a wrapper around a `i64` value that
    /// represents the number of milliseconds since start_time.
    pub fn timestamp(&self) -> SmolInstant {
        let elapsed = self.start_time.elapsed();
        SmolInstant::from_millis(
            (elapsed.as_secs() * 1_000) as i64 + (elapsed.subsec_millis()) as i64,
        )
    }
    /// Convert the number of milliseconds that smoltcp uses
    /// back to std::time::Instant.
    pub fn resolve(&self, timestamp: SmolInstant) -> Instant {
        self.start_time
            .add(Duration::from_millis(timestamp.millis as u64))
    }
}
