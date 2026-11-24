//! Cross-thread, coalescing `CFRunLoop` wakeup via a version-0 `CFRunLoopSource`.
//!
//! Producer threads (AX observer callbacks, worker completions, control socket
//! requests, global hotkeys, trackpad swipes, UI actions) call [`WakeSlot::wake`]
//! or [`RunLoopWaker::wake`] after publishing work. Coalescing ensures bursts of
//! events enqueue at most one `CFRunLoopSourceSignal` + `CFRunLoopWakeUp` until
//! the target run loop drains the source, while preserving zero idle wakeups.

use std::{
    cell::RefCell,
    ffi::c_void,
    marker::PhantomData,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use objc2::rc::autoreleasepool;
use objc2_core_foundation::{
    CFRetained, CFRunLoop, CFRunLoopSource, CFRunLoopSourceContext, kCFRunLoopCommonModes,
};

struct WakeTarget {
    run_loop: CFRetained<CFRunLoop>,
    source: CFRetained<CFRunLoopSource>,
}

// SAFETY: Apple documents `CFRunLoopSourceSignal` and `CFRunLoopWakeUp` as
// thread-safe across threads. `WakeTarget` is only accessed under the `Mutex`,
// never cloned out of it, and taken and dropped on the owning run-loop thread
// by `RunLoopWakeGuard::drop`.
unsafe impl Send for WakeTarget {}

struct SourceContext {
    pending: Arc<AtomicBool>,
    callback: RefCell<Box<dyn FnMut()>>,
}

/// Thread-safe, coalescing handle that wakes a specific `CFRunLoop`.
pub struct RunLoopWaker {
    pending: Arc<AtomicBool>,
    target: Mutex<Option<WakeTarget>>,
}

impl RunLoopWaker {
    /// Signals the target run loop to run its wake callback immediately.
    /// Repeated calls before the callback begins coalesce into a single wakeup.
    pub fn wake(&self) {
        if self.pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let target = self
            .target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(target) = target.as_ref() {
            target.source.signal();
            target.run_loop.wake_up();
        }
    }
}

/// Late-bound wake slot shared across threads before the main run loop starts.
/// Wakes that arrive before [`WakeSlot::bind`] are remembered and trigger one
/// immediate wake as soon as the waker is bound.
#[derive(Default)]
pub struct WakeSlot {
    early_pending: AtomicBool,
    waker: Mutex<Option<Arc<RunLoopWaker>>>,
}

impl WakeSlot {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind(&self, waker: Arc<RunLoopWaker>) {
        let mut slot = self
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(Arc::clone(&waker));
        let had_early = self.early_pending.swap(false, Ordering::AcqRel);
        drop(slot);
        if had_early {
            waker.wake();
        }
    }

    pub fn wake(&self) {
        let waker = {
            let slot = self
                .waker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(waker) = slot.as_ref() {
                Arc::clone(waker)
            } else {
                self.early_pending.store(true, Ordering::Release);
                return;
            }
        };
        waker.wake();
    }
}

/// Owns a version-0 `CFRunLoopSource` installed on the current thread's
/// `CFRunLoop` in `kCFRunLoopCommonModes`. Dropping it detaches and invalidates
/// the source after taking the wake target under lock so concurrent `wake()`
/// calls can never touch an invalidated source.
pub struct RunLoopWakeGuard {
    waker: Arc<RunLoopWaker>,
    _not_send: PhantomData<*const ()>,
}

impl RunLoopWakeGuard {
    /// Installs a wake source on the current thread's `CFRunLoop` that invokes
    /// `callback` whenever [`RunLoopWaker::wake`] is signalled.
    #[must_use]
    pub fn install_on_current_run_loop(callback: impl FnMut() + 'static) -> Option<Self> {
        let run_loop = CFRunLoop::current()?;
        let pending = Arc::new(AtomicBool::new(false));
        // `Arc` provides atomic refcounting for Core Foundation's `retain`/`release`
        // callbacks; `callback` itself is only invoked on the owning run-loop thread.
        #[allow(clippy::arc_with_non_send_sync)]
        let context = Arc::new(SourceContext {
            pending: Arc::clone(&pending),
            callback: RefCell::new(Box::new(callback)),
        });
        let mut cf_context = CFRunLoopSourceContext {
            version: 0,
            info: Arc::as_ptr(&context).cast_mut().cast::<c_void>(),
            retain: Some(wake_retain),
            release: Some(wake_release),
            copyDescription: None,
            equal: None,
            hash: None,
            schedule: None,
            cancel: None,
            perform: Some(wake_perform),
        };
        // SAFETY: `cf_context` is a valid version-0 `CFRunLoopSourceContext`;
        // `CFRunLoopSource::new` synchronously calls `wake_retain` to take its
        // own strong `Arc<SourceContext>` reference while `context` is alive.
        let source = unsafe { CFRunLoopSource::new(None, 0, &raw mut cf_context) }?;
        drop(context);
        // SAFETY: `kCFRunLoopCommonModes` is a static Core Foundation mode constant.
        run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
        let waker = Arc::new(RunLoopWaker {
            pending,
            target: Mutex::new(Some(WakeTarget { run_loop, source })),
        });
        Some(Self {
            waker,
            _not_send: PhantomData,
        })
    }

    #[must_use]
    pub fn waker(&self) -> Arc<RunLoopWaker> {
        Arc::clone(&self.waker)
    }
}

impl Drop for RunLoopWakeGuard {
    fn drop(&mut self) {
        let target = self
            .waker
            .target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(target) = target {
            // SAFETY: `kCFRunLoopCommonModes` is a static Core Foundation mode constant.
            target
                .run_loop
                .remove_source(Some(&target.source), unsafe { kCFRunLoopCommonModes });
            target.source.invalidate();
        }
    }
}

unsafe extern "C-unwind" fn wake_retain(info: *const c_void) -> *const c_void {
    if !info.is_null() {
        // SAFETY: `info` is a live `Arc<SourceContext>` pointer owned by the caller or CF source.
        unsafe { Arc::increment_strong_count(info.cast::<SourceContext>()) };
    }
    info
}

unsafe extern "C-unwind" fn wake_release(info: *const c_void) {
    if !info.is_null() {
        // SAFETY: Balances a matching `wake_retain` call for this `Arc<SourceContext>`.
        unsafe { drop(Arc::from_raw(info.cast::<SourceContext>())) };
    }
}

unsafe extern "C-unwind" fn wake_perform(info: *mut c_void) {
    if info.is_null() {
        return;
    }
    // Retain our own strong reference across the callback: if the callback drops
    // `RunLoopWakeGuard`, `CFRunLoopSourceInvalidate` immediately calls
    // `wake_release` before `wake_perform` returns.
    // SAFETY: `info` is a live `Arc<SourceContext>` retained by the source until
    // after we increment its strong count here.
    let context = unsafe {
        Arc::increment_strong_count(info.cast::<SourceContext>());
        Arc::from_raw(info.cast::<SourceContext>())
    };
    // Clear `pending` with `AcqRel` before running the callback so:
    // 1. Any writes published before a coalesced `wake()` synchronize with us.
    // 2. Any `wake()` arriving during the callback sees `false` and schedules
    //    another pass so no event can be lost.
    context.pending.swap(false, Ordering::AcqRel);
    autoreleasepool(|_| {
        if let Ok(mut callback) = context.callback.try_borrow_mut() {
            callback();
        }
    });
}
