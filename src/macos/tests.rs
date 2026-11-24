use std::thread;

use crate::{AppInstanceId, DisplaySelector, PhysicalWindowId, Rect};

use super::{
    DisplayOutput, DisplayOutputs,
    apply::{group_placements_by_app, position_matches},
    ax::{WindowServerWindow, should_ignore_missing_window_id, windows_reaching_point},
    classify::{TitleBar, TitleButton, is_dialog, is_ignored},
    events::running_application_instances,
    frontmost_application, is_process_incarnation_running, process_start_time,
};

/// Title bars measured on macOS 27.2 with `AXFullScreenButton`, `AXZoomButton`
/// and `AXCloseButton`.
fn bar(fullscreen: TitleButton, zoom: TitleButton, close: bool) -> TitleBar {
    TitleBar {
        fullscreen,
        zoom,
        close,
    }
}

#[test]
fn primary_windows_without_a_working_fullscreen_button_still_tile() {
    use TitleButton::{Absent, Disabled, Enabled};
    // FaceTime (16:10 landscape), Phone, News, Podcasts, Games, Shortcuts and
    // System Settings: no fullscreen button, an enabled zoom button.
    assert!(!is_dialog(false, false, bar(Absent, Enabled, true)));
    // FaceTime (2:3 portrait), or any window while it shows a sheet: both
    // buttons disabled, but the fullscreen button is there.
    assert!(!is_dialog(false, false, bar(Disabled, Disabled, true)));
    // An ordinary document window.
    assert!(!is_dialog(false, false, bar(Enabled, Enabled, true)));
    // Borderless regular-app windows (Ghostty without decorations).
    assert!(!is_dialog(false, false, bar(Absent, Absent, false)));
}

#[test]
fn settings_windows_modals_and_accessory_panels_still_float() {
    use TitleButton::{Absent, Disabled, Enabled};
    // TextEdit Settings and Calculator: no fullscreen button, zoom disabled.
    assert!(is_dialog(false, false, bar(Absent, Disabled, true)));
    // A closable window with neither button.
    assert!(is_dialog(false, false, bar(Absent, Absent, true)));
    // Modal windows always float.
    assert!(is_dialog(false, true, bar(Enabled, Enabled, true)));
    // Accessory apps' windows tile only with an enabled fullscreen button; an
    // enabled zoom button is not enough.
    assert!(is_dialog(true, false, bar(Absent, Enabled, true)));
    assert!(is_dialog(true, false, bar(Disabled, Disabled, true)));
    assert!(!is_dialog(true, false, bar(Enabled, Enabled, true)));
    // Their buttonless windows are ignored outright.
    assert!(is_ignored(true, bar(Absent, Absent, false)));
    assert!(!is_ignored(true, bar(Absent, Absent, true)));
    assert!(!is_ignored(false, bar(Absent, Absent, false)));
}

#[test]
fn processes_without_a_launch_date_are_identified_by_their_kernel_start_time() {
    let now = crate::config::now().as_millis() as u64;
    let started = process_start_time(std::process::id() as i32).unwrap();
    assert!(started <= now && now - started < 3_600_000);
    assert_eq!(process_start_time(std::process::id() as i32), Some(started));
    assert_eq!(process_start_time(0), None);
}

#[test]
fn pointer_hit_testing_uses_front_to_back_geometry_without_ax_focus() {
    let front = WindowServerWindow {
        pid: 1,
        id: 10,
        frame: Rect {
            x: -1000,
            y: -200,
            width: 400,
            height: 300,
        },
    };
    let behind = WindowServerWindow {
        pid: 2,
        id: 20,
        frame: Rect {
            width: 800,
            ..front.frame
        },
    };
    let windows = [front, behind];
    let ids = |point| {
        windows_reaching_point(&windows, point)
            .map(|window| window.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids((-900, -100)), [10, 20]);
    assert_eq!(ids((-500, -100)), [20]);
    assert!(ids((100, 100)).is_empty());
}

#[test]
fn pointer_hit_testing_includes_windows_whose_resize_border_reaches_a_gap() {
    let left = WindowServerWindow {
        pid: 1,
        id: 10,
        frame: Rect {
            x: 0,
            y: 0,
            width: 500,
            height: 800,
        },
    };
    // An 8-point gap: the pointer at x 504 is over neither window.
    let right = WindowServerWindow {
        pid: 2,
        id: 20,
        frame: Rect {
            x: 508,
            ..left.frame
        },
    };
    let windows = [right, left];
    let ids = |point| {
        windows_reaching_point(&windows, point)
            .map(|window| window.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids((504, 400)), [20, 10]);
    // Just past the left window's edge, inside the right one.
    assert_eq!(ids((509, 400)), [20, 10]);
    assert_eq!(ids((300, 400)), [10]);
    assert!(ids((540, 900)).is_empty());
}

#[test]
fn position_only_validation_ignores_window_size() {
    let target = Rect {
        x: 100,
        y: 200,
        width: 800,
        height: 600,
    };
    let differently_sized = Rect {
        width: 320,
        height: 240,
        ..target
    };

    assert!(position_matches(differently_sized, target));
}

#[test]
fn managed_layout_reads_each_app_window_list_once() {
    let app = AppInstanceId {
        pid: 1,
        launched_at: 1,
    };
    let windows: Vec<_> = (1..=256)
        .map(|window_id| PhysicalWindowId {
            app: app.clone(),
            window_id,
        })
        .collect();
    let placements: Vec<_> = windows
        .iter()
        .map(|window| (window, Rect::default()))
        .collect();

    let groups = group_placements_by_app(&placements);

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].0, app);
    assert_eq!(groups[0].1.len(), 256);
}

#[test]
fn arbitrary_display_topology_resolves_preferences_and_fallbacks() {
    let outputs = DisplayOutputs::new(vec![
        DisplayOutput {
            id: "main-id".into(),
            name: "Built-in".into(),
            frame: Rect::default(),
            is_main: true,
        },
        DisplayOutput {
            id: "second-id".into(),
            name: "Studio Display".into(),
            frame: Rect {
                x: 100,
                ..Rect::default()
            },
            is_main: false,
        },
        DisplayOutput {
            id: "third-id".into(),
            name: "Projector".into(),
            frame: Rect {
                x: 200,
                ..Rect::default()
            },
            is_main: false,
        },
    ])
    .unwrap();

    assert_eq!(outputs.displays().len(), 3);
    assert_eq!(
        outputs
            .resolve(&DisplaySelector::Preferred("third-id".into()))
            .id,
        "third-id"
    );
    assert_eq!(
        outputs
            .resolve(&DisplaySelector::Preferred("missing".into()))
            .id,
        "main-id"
    );
}

fn current_phys_footprint() -> u64 {
    // SAFETY: `rusage_info_v4` is a plain C struct of integer counters valid when zero-initialized.
    let mut info = unsafe { std::mem::zeroed::<libc::rusage_info_v4>() };
    // SAFETY: `info` is writable stack storage matching `RUSAGE_INFO_V4`.
    let rc = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
            (&raw mut info).cast::<libc::rusage_info_t>(),
        )
    };
    assert_eq!(rc, 0, "proc_pid_rusage failed");
    info.ri_phys_footprint
}

#[test]
fn worker_thread_queries_drain_objective_c_autorelease_pool() {
    let (baseline, after) = thread::spawn(|| {
        for _ in 0..10 {
            let apps = running_application_instances();
            drop(frontmost_application());
            if let Some(first) = apps.first() {
                std::hint::black_box(is_process_incarnation_running(first));
            }
        }
        let baseline = current_phys_footprint();
        for _ in 0..400 {
            let apps = running_application_instances();
            drop(frontmost_application());
            for app in &apps {
                std::hint::black_box(is_process_incarnation_running(app));
            }
        }
        let after = current_phys_footprint();
        (baseline, after)
    })
    .join()
    .expect("worker thread panicked");

    let growth = after.saturating_sub(baseline);
    assert!(
        growth < 2 * 1024 * 1024,
        "worker thread leaked {growth} bytes of phys_footprint across 400 cycles (baseline={baseline}, after={after})"
    );
}

#[test]
fn window_event_monitor_wakes_parked_run_loop_on_refresh_and_drop() {
    let monitor = super::WindowEventMonitor::new();
    assert!(monitor.take_dirty());
    // Let the AX event thread park inside `CFRunLoop::run_in_mode` (24h timeout).
    thread::sleep(std::time::Duration::from_millis(25));
    monitor.refresh_applications();
    thread::sleep(std::time::Duration::from_millis(10));
    let stop_started = std::time::Instant::now();
    drop(monitor);
    assert!(
        stop_started.elapsed() < std::time::Duration::from_millis(500),
        "parked event monitor took {:?} to wake and stop",
        stop_started.elapsed()
    );
}

#[test]
fn missing_window_id_skips_non_standard_elements_like_finders_desktop() {
    // Finder reports the desktop as an AXScrollArea / AXDesktop entry without a CGWindowID.
    assert!(should_ignore_missing_window_id(
        Some("AXScrollArea"),
        Some("AXDesktop"),
    ));
    assert!(should_ignore_missing_window_id(Some("AXScrollArea"), None));
    assert!(should_ignore_missing_window_id(
        Some("AXWindow"),
        Some("AXDesktop"),
    ));
    // Standard windows or unreadable elements stay conservative (Unavailable).
    assert!(!should_ignore_missing_window_id(
        Some("AXWindow"),
        Some("AXStandardWindow"),
    ));
    assert!(!should_ignore_missing_window_id(Some("AXWindow"), None));
    assert!(!should_ignore_missing_window_id(None, None));
}

#[test]
fn window_server_frame_matches_full_list_and_returns_none_for_unknown_id() {
    let Some(windows) =
        super::ax::window_server_windows(objc2_core_graphics::CGWindowListOption::OptionAll)
    else {
        return;
    };
    for window in windows.iter().take(16) {
        let id = PhysicalWindowId {
            app: AppInstanceId {
                pid: window.pid,
                launched_at: 1,
            },
            window_id: window.id,
        };
        assert_eq!(super::ax::window_server_frame(&id), Some(window.frame));
    }
    let bogus = PhysicalWindowId {
        app: AppInstanceId {
            pid: 1,
            launched_at: 1,
        },
        window_id: u32::MAX,
    };
    assert_eq!(super::ax::window_server_frame(&bogus), None);
}

#[test]
fn run_loop_waker_wakes_immediately_coalesces_bursts_and_never_loses_wakeups() {
    use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        },
        time::{Duration, Instant},
    };

    let (waker_tx, waker_rx) = mpsc::channel();
    let (events_tx, events_rx) = mpsc::channel::<usize>();
    let (drained_tx, drained_rx) = mpsc::channel::<usize>();
    let (mid_cb_enter_tx, mid_cb_enter_rx) = mpsc::channel::<()>();
    let (mid_cb_release_tx, mid_cb_release_rx) = mpsc::channel::<()>();

    let callback_count = Arc::new(AtomicUsize::new(0));
    let pause_next_callback = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    let cb_count = Arc::clone(&callback_count);
    let cb_pause = Arc::clone(&pause_next_callback);
    let loop_stop = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        let run_loop = CFRunLoop::current().unwrap();
        let guard = super::RunLoopWakeGuard::install_on_current_run_loop(move || {
            cb_count.fetch_add(1, Ordering::AcqRel);
            if cb_pause.swap(false, Ordering::AcqRel) {
                mid_cb_enter_tx.send(()).unwrap();
                mid_cb_release_rx.recv().unwrap();
            }
            while let Ok(item) = events_rx.try_recv() {
                drained_tx.send(item).unwrap();
            }
        })
        .unwrap();
        waker_tx.send(guard.waker()).unwrap();
        while !loop_stop.load(Ordering::Acquire) {
            // SAFETY: `kCFRunLoopDefaultMode` is a static Core Foundation mode constant.
            CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.5, true);
        }
        drop(guard);
        drop(run_loop);
    });

    let waker = waker_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    // Zero idle wakeups when nothing signals.
    thread::sleep(Duration::from_millis(30));
    assert_eq!(callback_count.load(Ordering::Acquire), 0);

    // Burst of 50 events coalesces and wakes well below the 25 ms timer interval.
    let started = Instant::now();
    for i in 0..50 {
        events_tx.send(i).unwrap();
        waker.wake();
    }
    let mut seen = Vec::with_capacity(50);
    for _ in 0..50 {
        seen.push(drained_rx.recv_timeout(Duration::from_secs(1)).unwrap());
    }
    let burst_elapsed = started.elapsed();
    assert_eq!(seen, (0..50).collect::<Vec<_>>());
    assert!(
        burst_elapsed < Duration::from_millis(25),
        "event-driven wakeup took {burst_elapsed:?}"
    );
    let burst_callbacks = callback_count.load(Ordering::Acquire);
    assert!(
        (1..=10).contains(&burst_callbacks),
        "expected coalesced callbacks, got {burst_callbacks}"
    );

    // A wake arriving while the callback is executing must trigger a follow-up
    // pass so events published after the callback started draining are never lost.
    pause_next_callback.store(true, Ordering::Release);
    events_tx.send(100).unwrap();
    waker.wake();
    mid_cb_enter_rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    // Release the callback and race a new event + wake immediately.
    mid_cb_release_tx.send(()).unwrap();
    thread::sleep(Duration::from_millis(2));
    events_tx.send(101).unwrap();
    waker.wake();

    let first = drained_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let second = drained_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!([first, second], [100, 101]);

    stop.store(true, Ordering::Release);
    waker.wake();
    handle.join().unwrap();
}

#[test]
fn wake_slot_remembers_early_wake_until_bound() {
    use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::Duration,
    };

    let slot = Arc::new(super::WakeSlot::new());
    slot.wake();

    let (ran_tx, ran_rx) = mpsc::channel::<()>();
    let stop = Arc::new(AtomicBool::new(false));
    let loop_stop = Arc::clone(&stop);
    let loop_slot = Arc::clone(&slot);

    let handle =
        thread::spawn(move || {
            let guard = super::RunLoopWakeGuard::install_on_current_run_loop(move || {
                if ran_tx.send(()).is_err() {}
            })
            .unwrap();
            loop_slot.bind(guard.waker());
            while !loop_stop.load(Ordering::Acquire) {
                // SAFETY: `kCFRunLoopDefaultMode` is a static Core Foundation mode constant.
                CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.5, true);
            }
        });

    ran_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    stop.store(true, Ordering::Release);
    slot.wake();
    handle.join().unwrap();
}

#[test]
fn dropping_wake_guard_from_inside_callback_and_concurrent_wakes_are_safe() {
    use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
    use std::{
        cell::RefCell,
        rc::Rc,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        },
        time::Duration,
    };

    struct DropCounter(Arc<AtomicUsize>);
    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    let drops = Arc::new(AtomicUsize::new(0));
    let drops_for_cb = Arc::clone(&drops);
    let (waker_tx, waker_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let stop_spammers = Arc::new(AtomicBool::new(false));

    let handle = thread::spawn(move || {
        let guard_slot: Rc<RefCell<Option<super::RunLoopWakeGuard>>> = Rc::new(RefCell::new(None));
        let guard_for_cb = Rc::clone(&guard_slot);
        let sentinel = DropCounter(drops_for_cb);
        let guard = super::RunLoopWakeGuard::install_on_current_run_loop(move || {
            let _keep = &sentinel;
            // Dropping the guard from inside its own callback calls
            // `CFRunLoopSourceInvalidate` while `wake_perform` is still on the stack.
            guard_for_cb.borrow_mut().take();
            if done_tx.send(()).is_err() {}
        })
        .unwrap();
        let waker = guard.waker();
        *guard_slot.borrow_mut() = Some(guard);
        waker_tx.send(waker).unwrap();
        while guard_slot.borrow().is_some() {
            // SAFETY: `kCFRunLoopDefaultMode` is a static Core Foundation mode constant.
            CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.5, true);
        }
    });

    let waker = waker_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let spam_stop = Arc::clone(&stop_spammers);
    let spam_waker = Arc::clone(&waker);
    let spammer = thread::spawn(move || {
        while !spam_stop.load(Ordering::Acquire) {
            spam_waker.wake();
            thread::yield_now();
        }
    });

    done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    handle.join().unwrap();
    stop_spammers.store(true, Ordering::Release);
    spammer.join().unwrap();
    // Calling wake after the guard is gone is a safe no-op.
    waker.wake();
    assert_eq!(drops.load(Ordering::Acquire), 1);
}

#[test]
#[ignore = "performance benchmark: run in --release with -- --ignored --nocapture"]
fn perf_bench_main_loop_event_wakeup() {
    use objc2_core_foundation::{
        CFAbsoluteTimeGetCurrent, CFRunLoop, CFRunLoopTimer, CFRunLoopTimerContext,
        kCFRunLoopCommonModes, kCFRunLoopDefaultMode,
    };
    use std::{
        ffi::c_void,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::{Duration, Instant},
    };

    struct PendingStamp {
        published: Mutex<Option<Instant>>,
        latencies: Mutex<Vec<Duration>>,
        done_tx: mpsc::Sender<()>,
    }

    unsafe extern "C-unwind" fn timer_cb(_timer: *mut CFRunLoopTimer, info: *mut c_void) {
        if info.is_null() {
            return;
        }
        // SAFETY: `info` points to `PendingStamp` kept alive until after `timer.invalidate()`.
        let state = unsafe { &*info.cast::<PendingStamp>() };
        if let Some(published) = state.published.lock().unwrap().take() {
            state.latencies.lock().unwrap().push(published.elapsed());
            if state.done_tx.send(()).is_err() {}
        }
    }

    fn summarize(mut samples: Vec<Duration>) -> (f64, f64, f64) {
        samples.sort_unstable();
        let median = samples[samples.len() / 2].as_secs_f64() * 1000.0;
        let mean = samples
            .iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .sum::<f64>()
            / samples.len() as f64;
        let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)].as_secs_f64() * 1000.0;
        (median, mean, p95)
    }

    fn run_single_hop(use_waker: bool, events: usize) -> (f64, f64, f64) {
        let (done_tx, done_rx) = mpsc::channel();
        let state = Arc::new(PendingStamp {
            published: Mutex::new(None),
            latencies: Mutex::new(Vec::with_capacity(events)),
            done_tx,
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();

        let loop_state = Arc::clone(&state);
        let loop_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let run_loop = CFRunLoop::current().unwrap();
            let mut timer_ctx = CFRunLoopTimerContext {
                version: 0,
                info: Arc::as_ptr(&loop_state).cast_mut().cast::<c_void>(),
                retain: None,
                release: None,
                copyDescription: None,
            };
            // SAFETY: `timer_ctx` is valid and `loop_state` outlives the timer.
            let timer = unsafe {
                CFRunLoopTimer::new(
                    None,
                    CFAbsoluteTimeGetCurrent() + 0.025,
                    0.025,
                    0,
                    0,
                    Some(timer_cb),
                    &raw mut timer_ctx,
                )
            }
            .unwrap();
            // SAFETY: `kCFRunLoopCommonModes` is a static Core Foundation mode constant.
            run_loop.add_timer(Some(&timer), unsafe { kCFRunLoopCommonModes });

            let wake_state = Arc::clone(&loop_state);
            let guard = super::RunLoopWakeGuard::install_on_current_run_loop(move || {
                if let Some(published) = wake_state.published.lock().unwrap().take() {
                    wake_state
                        .latencies
                        .lock()
                        .unwrap()
                        .push(published.elapsed());
                    if wake_state.done_tx.send(()).is_err() {}
                }
            })
            .unwrap();
            ready_tx.send(guard.waker()).unwrap();

            while !loop_stop.load(Ordering::Acquire) {
                // SAFETY: `kCFRunLoopDefaultMode` is a static Core Foundation mode constant.
                CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.05, true);
            }
            drop(guard);
            // SAFETY: `kCFRunLoopCommonModes` is a static Core Foundation mode constant.
            run_loop.remove_timer(Some(&timer), unsafe { kCFRunLoopCommonModes });
            timer.invalidate();
        });

        let waker = ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // Prime loop and spread event arrival phases across the full 25 ms period.
        thread::sleep(Duration::from_millis(30));
        for i in 0..events {
            let gap_ms = 2 + ((i * 7) % 23) as u64;
            thread::sleep(Duration::from_millis(gap_ms));
            *state.published.lock().unwrap() = Some(Instant::now());
            if use_waker {
                waker.wake();
            }
            done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        stop.store(true, Ordering::Release);
        waker.wake();
        handle.join().unwrap();

        let samples = std::mem::take(&mut *state.latencies.lock().unwrap());
        summarize(samples)
    }

    fn run_two_hop_round_trip(use_waker: bool, events: usize) -> (f64, f64, f64) {
        let (to_worker_tx, to_worker_rx) = mpsc::channel::<Instant>();
        let (from_worker_tx, from_worker_rx) = mpsc::channel::<Instant>();
        let (done_tx, done_rx) = mpsc::channel::<Duration>();
        let pending_input = Arc::new(Mutex::new(None::<Instant>));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();

        struct TwoHopState {
            pending_input: Arc<Mutex<Option<Instant>>>,
            to_worker: mpsc::Sender<Instant>,
            from_worker: mpsc::Receiver<Instant>,
            done_tx: mpsc::Sender<Duration>,
        }

        impl TwoHopState {
            fn pump(&self) {
                while let Ok(started) = self.from_worker.try_recv() {
                    if self.done_tx.send(started.elapsed()).is_err() {}
                }
                if let Some(started) = self.pending_input.lock().unwrap().take()
                    && self.to_worker.send(started).is_err()
                {}
            }
        }

        unsafe extern "C-unwind" fn two_hop_timer(_timer: *mut CFRunLoopTimer, info: *mut c_void) {
            if info.is_null() {
                return;
            }
            // SAFETY: `info` points to `TwoHopState` kept alive until after `timer.invalidate()`.
            let state = unsafe { &*info.cast::<TwoHopState>() };
            state.pump();
        }

        let wake_slot = Arc::new(super::WakeSlot::new());
        let worker_wake = Arc::clone(&wake_slot);
        let worker = thread::spawn(move || {
            while let Ok(started) = to_worker_rx.recv() {
                if from_worker_tx.send(started).is_err() {
                    break;
                }
                if use_waker {
                    worker_wake.wake();
                }
            }
        });

        let loop_input = Arc::clone(&pending_input);
        let loop_stop = Arc::clone(&stop);
        let loop_wake = Arc::clone(&wake_slot);
        let handle = thread::spawn(move || {
            let run_loop = CFRunLoop::current().unwrap();
            let state = std::rc::Rc::new(TwoHopState {
                pending_input: loop_input,
                to_worker: to_worker_tx,
                from_worker: from_worker_rx,
                done_tx,
            });
            let mut timer_ctx = CFRunLoopTimerContext {
                version: 0,
                info: std::rc::Rc::as_ptr(&state).cast_mut().cast::<c_void>(),
                retain: None,
                release: None,
                copyDescription: None,
            };
            // SAFETY: `timer_ctx` is valid and `state` outlives the timer.
            let timer = unsafe {
                CFRunLoopTimer::new(
                    None,
                    CFAbsoluteTimeGetCurrent() + 0.025,
                    0.025,
                    0,
                    0,
                    Some(two_hop_timer),
                    &raw mut timer_ctx,
                )
            }
            .unwrap();
            // SAFETY: `kCFRunLoopCommonModes` is a static Core Foundation mode constant.
            run_loop.add_timer(Some(&timer), unsafe { kCFRunLoopCommonModes });

            let wake_state = std::rc::Rc::clone(&state);
            let guard = super::RunLoopWakeGuard::install_on_current_run_loop(move || {
                wake_state.pump();
            })
            .unwrap();
            loop_wake.bind(guard.waker());
            ready_tx.send(guard.waker()).unwrap();

            while !loop_stop.load(Ordering::Acquire) {
                // SAFETY: `kCFRunLoopDefaultMode` is a static Core Foundation mode constant.
                CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.05, true);
            }
            drop(guard);
            // SAFETY: `kCFRunLoopCommonModes` is a static Core Foundation mode constant.
            run_loop.remove_timer(Some(&timer), unsafe { kCFRunLoopCommonModes });
            timer.invalidate();
        });

        let waker = ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        thread::sleep(Duration::from_millis(30));
        let mut samples = Vec::with_capacity(events);
        for i in 0..events {
            let gap_ms = 2 + ((i * 7) % 23) as u64;
            thread::sleep(Duration::from_millis(gap_ms));
            *pending_input.lock().unwrap() = Some(Instant::now());
            if use_waker {
                wake_slot.wake();
            }
            samples.push(done_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        }
        stop.store(true, Ordering::Release);
        waker.wake();
        handle.join().unwrap();
        worker.join().unwrap();
        summarize(samples)
    }

    let (base_1_med, base_1_mean, base_1_p95) = run_single_hop(false, 40);
    let (opt_1_med, opt_1_mean, opt_1_p95) = run_single_hop(true, 40);
    let (base_2_med, base_2_mean, base_2_p95) = run_two_hop_round_trip(false, 25);
    let (opt_2_med, opt_2_mean, opt_2_p95) = run_two_hop_round_trip(true, 25);

    println!(
        "perf_bench_main_loop_wakeup_1hop_timer_only: median={base_1_med:.3}ms mean={base_1_mean:.3}ms p95={base_1_p95:.3}ms"
    );
    println!(
        "perf_bench_main_loop_wakeup_1hop_event_driven: median={opt_1_med:.3}ms mean={opt_1_mean:.3}ms p95={opt_1_p95:.3}ms (speedup={:.1}x)",
        base_1_med / opt_1_med.max(0.0001)
    );
    println!(
        "perf_bench_main_loop_wakeup_2hop_timer_only: median={base_2_med:.3}ms mean={base_2_mean:.3}ms p95={base_2_p95:.3}ms"
    );
    println!(
        "perf_bench_main_loop_wakeup_2hop_event_driven: median={opt_2_med:.3}ms mean={opt_2_mean:.3}ms p95={opt_2_p95:.3}ms (speedup={:.1}x)",
        base_2_med / opt_2_med.max(0.0001)
    );
}
