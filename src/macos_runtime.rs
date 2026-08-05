//! macOS process bootstrap that keeps native main-thread services responsive.

use std::{
    sync::mpsc::{self, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use dispatch2::DispatchQueue;
use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};

/// Run the application on a worker while the process main thread services the
/// run loop required by native macOS speech synthesis.
pub fn run<T, F>(application: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    // SAFETY: This call only reads pthread identity state.
    assert_eq!(unsafe { libc::pthread_main_np() }, 1);

    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name("agent-speak-application".into())
        .spawn(move || {
            let result = application();
            let _ = result_tx.send(result);
            // Wake an idle main run loop so it observes the result promptly.
            DispatchQueue::main().exec_async(|| {});
        })
        .expect("failed to start the Agent Speak application thread");

    // SAFETY: Core Foundation owns this immutable process-lifetime mode.
    let mode =
        unsafe { kCFRunLoopDefaultMode }.expect("default macOS run-loop mode is unavailable");
    let result = loop {
        match result_rx.try_recv() {
            Ok(result) => break result,
            Err(TryRecvError::Disconnected) => match worker.join() {
                Ok(()) => panic!("Agent Speak application ended without a result"),
                Err(panic) => std::panic::resume_unwind(panic),
            },
            Err(TryRecvError::Empty) => {}
        }

        let slice = Duration::from_millis(20);
        let started = Instant::now();
        CFRunLoop::run_in_mode(Some(mode), slice.as_secs_f64(), true);
        if let Some(remaining) = slice.checked_sub(started.elapsed()) {
            thread::sleep(remaining);
        }
    };

    worker
        .join()
        .expect("Agent Speak application thread panicked after returning a result");
    result
}
