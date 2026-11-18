use std::future::Future;
use std::pin::Pin;

/// Spawning, timers and the clock, supplied by the host (the core is runtime-free). The same trait serves the offline
/// library, the playback scheduler and the server connection.
pub trait Runtime {
    /// Runs `future` to completion in the background.
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()>>>);
    fn sleep(&self, millis: u64) -> Pin<Box<dyn Future<Output = ()>>>;
    fn now_ms(&self) -> u64;
}
