use super::*;

#[test]
fn dual_display_cycle_call_budget_is_explicit() {
    let mut runtime = runtime(benchmark_backend(2));

    runtime.cycle(outputs(), &AtomicBool::new(false)).unwrap();
    runtime.cycle(outputs(), &AtomicBool::new(false)).unwrap();

    assert_eq!(runtime.backend.snapshot_count, 2);
    assert_eq!(runtime.backend.observed_apply_count, 2);
}

#[test]
#[ignore = "performance benchmark"]
fn benchmark_dual_display_steady_state_256_windows() {
    let mut runtime = runtime(benchmark_backend(256));
    let cancelled = AtomicBool::new(false);
    for _ in 0..10 {
        runtime.cycle(outputs(), &cancelled).unwrap();
    }
    let mut samples = Vec::new();
    for _ in 0..9 {
        let start = std::time::Instant::now();
        for _ in 0..100 {
            std::hint::black_box(runtime.cycle(outputs(), &cancelled).unwrap());
        }
        samples.push(start.elapsed().as_secs_f64() * 10_000.0);
    }
    samples.sort_by(f64::total_cmp);
    let median = samples[samples.len() / 2];
    println!("METRIC steady_state_cycle_us={median:.3}");
    println!("METRIC snapshot_calls={}", runtime.backend.snapshot_count);
    println!(
        "METRIC observed_apply_calls={}",
        runtime.backend.observed_apply_count
    );
}

#[test]
#[ignore = "performance benchmark"]
fn benchmark_new_window_event_to_correct_frame() {
    let existing = window(10);
    let created = window(11);
    let main_frame = frame(50);
    let secondary_frame = frame(1050);
    let mut samples = Vec::new();

    // One warmup followed by nine measured event-loop phases. The AX dirty bit
    // remains set until the production dispatch predicate accepts the refresh.
    for first_tick_ms in [1_u64, 1, 4, 7, 10, 13, 16, 19, 22, 25] {
        let mut backend = FakeBackend {
            focused: Some(existing.clone()),
            frontmost: Some(existing.app.clone()),
            observation: Some(observation_for_windows(
                "com.mitchellh.ghostty",
                &existing,
                &[(existing.clone(), main_frame)],
            )),
            ..FakeBackend::default()
        };
        backend.frames.insert(existing.clone(), main_frame);
        let mut runtime = runtime(backend);
        runtime.cycle(outputs(), &AtomicBool::new(false)).unwrap();

        runtime.backend.focused = Some(created.clone());
        runtime.backend.frontmost = Some(created.app.clone());
        runtime
            .backend
            .frames
            .insert(created.clone(), secondary_frame);
        runtime.backend.observation = Some(observation_for_windows(
            "com.mitchellh.ghostty",
            &created,
            &[
                (existing.clone(), main_frame),
                (created.clone(), secondary_frame),
            ],
        ));

        let mut dispatch_ms = first_tick_ms;
        let activation_ready_ms = first_tick_ms + APPLICATION_ACTIVATION_SETTLE.as_millis() as u64;
        while !refresh_due(true, true, false, dispatch_ms >= activation_ready_ms) {
            dispatch_ms += TICK_INTERVAL.as_millis() as u64;
        }
        let apply_started = Instant::now();
        runtime.cycle(outputs(), &AtomicBool::new(false)).unwrap();
        let latency_ms = dispatch_ms as f64 + apply_started.elapsed().as_secs_f64() * 1_000.0;
        assert_eq!(
            runtime.workspaces.workspace_of(&created),
            Some(WorkspaceId::ONE)
        );
        assert!(runtime.backend.frames[&created].x < 1000);
        samples.push(latency_ms);
    }

    samples.remove(0);
    samples.sort_by(f64::total_cmp);
    let median = samples[samples.len() / 2];
    let spread = samples.last().unwrap() - samples.first().unwrap();
    println!("METRIC new_window_correct_frame_ms={median:.3}");
    println!("METRIC new_window_correct_frame_spread_ms={spread:.3}");
    println!("METRIC new_window_correct_frame_best_ms={:.3}", samples[0]);
    println!(
        "METRIC new_window_correct_frame_worst_ms={:.3}",
        samples[samples.len() - 1]
    );
}

#[derive(Clone, Copy, Debug, Default)]
struct BackendCounters {
    snapshot: usize,
    window_server_frame: usize,
    window_server_frames: usize,
    onscreen_window_frames: usize,
    onscreen_window_order: usize,
    frontmost_application: usize,
    focused_window: usize,
    frame: usize,
    set_position: usize,
    apply_managed: usize,
    apply_observed: usize,
    persist_parked: usize,
    persist_workspace_state: usize,
    persist_window_memory: usize,
    is_process_running: usize,
}

impl BackendCounters {
    fn print(&self, prefix: &str) {
        println!(
            "COUNTS {prefix} snapshot={} ws_frame={} ws_frames={} onscreen_frames={} onscreen_order={} frontmost={} focused={} ax_frame={} set_pos={} apply_managed={} apply_observed={} persist_parked={} persist_ws={} persist_mem={} proc_run={}",
            self.snapshot,
            self.window_server_frame,
            self.window_server_frames,
            self.onscreen_window_frames,
            self.onscreen_window_order,
            self.frontmost_application,
            self.focused_window,
            self.frame,
            self.set_position,
            self.apply_managed,
            self.apply_observed,
            self.persist_parked,
            self.persist_workspace_state,
            self.persist_window_memory,
            self.is_process_running,
        );
    }
}

struct BenchBackend {
    inner: FakeBackend,
    counts: BackendCounters,
    real_io: bool,
    parked_path: Option<PathBuf>,
    real_probe_window: Option<crate::PhysicalWindowId>,
}

impl BenchBackend {
    fn new(inner: FakeBackend, real_io: bool, parked_path: Option<PathBuf>) -> Self {
        let real_probe_window = real_io.then(|| {
            let wid = macos::onscreen_window_frames()
                .and_then(|frames| frames.keys().next().copied())
                .or_else(|| {
                    macos::window_server_frames().and_then(|frames| frames.keys().next().copied())
                })
                .unwrap_or(1);
            crate::PhysicalWindowId {
                app: AppInstanceId {
                    pid: std::process::id() as i32,
                    launched_at: 1,
                },
                window_id: wid,
            }
        });
        Self {
            inner,
            counts: BackendCounters::default(),
            real_io,
            parked_path,
            real_probe_window,
        }
    }

    fn reset_counts(&mut self) {
        self.counts = BackendCounters::default();
    }
}

impl WorkspaceBackend for BenchBackend {
    fn confirms_no_windows(&mut self, app: &AppInstanceId) -> bool {
        self.inner.confirms_no_windows(app)
    }

    fn confirms_set_aside(&mut self, window: &crate::PhysicalWindowId) -> bool {
        self.inner.confirms_set_aside(window)
    }

    fn snapshot(
        &mut self,
        cycle: u64,
        outputs: &macos::DisplayOutputs,
    ) -> Result<crate::NativeObservation, String> {
        self.counts.snapshot += 1;
        self.inner.snapshot(cycle, outputs)
    }

    fn is_process_running(&mut self, app: &AppInstanceId) -> bool {
        self.counts.is_process_running += 1;
        self.inner.is_process_running(app)
    }

    fn apply_observed(
        &mut self,
        placements: &[crate::Placement],
        settings: &Settings,
        cancelled: &AtomicBool,
    ) -> Vec<macos::ApplyResult> {
        self.counts.apply_observed += 1;
        self.inner.apply_observed(placements, settings, cancelled)
    }

    fn frame(
        &mut self,
        window: &crate::PhysicalWindowId,
    ) -> Result<crate::Rect, macos::WindowAccessError> {
        self.counts.frame += 1;
        self.inner.frame(window)
    }

    fn window_server_frame(&mut self, window: &crate::PhysicalWindowId) -> Option<crate::Rect> {
        self.counts.window_server_frame += 1;
        if let Some(probe) = &self.real_probe_window {
            std::hint::black_box(macos::window_server_frame(probe));
        }
        self.inner.window_server_frame(window)
    }

    fn window_server_frames(&mut self) -> Option<HashMap<u32, crate::Rect>> {
        self.counts.window_server_frames += 1;
        if self.real_io {
            std::hint::black_box(macos::window_server_frames());
        }
        self.inner.window_server_frames()
    }

    fn onscreen_window_frames(&mut self) -> Option<HashMap<u32, crate::Rect>> {
        self.counts.onscreen_window_frames += 1;
        if self.real_io {
            std::hint::black_box(macos::onscreen_window_frames());
        }
        self.inner.onscreen_window_frames()
    }

    fn onscreen_window_order(&mut self) -> Option<Vec<u32>> {
        self.counts.onscreen_window_order += 1;
        if self.real_io {
            std::hint::black_box(macos::onscreen_window_order());
        }
        self.inner.onscreen_window_order()
    }

    fn set_position(
        &mut self,
        window: &crate::PhysicalWindowId,
        target: crate::Rect,
    ) -> macos::ApplyOutcome {
        self.counts.set_position += 1;
        self.inner.set_position(window, target)
    }

    fn focused_window(&mut self) -> Option<crate::PhysicalWindowId> {
        self.counts.focused_window += 1;
        self.inner.focused_window()
    }

    fn frontmost_application(&mut self) -> Option<AppInstanceId> {
        self.counts.frontmost_application += 1;
        if self.real_io {
            std::hint::black_box(macos::frontmost_application());
        }
        self.inner.frontmost_application()
    }

    fn raise_window(&mut self, window: &crate::PhysicalWindowId) -> bool {
        self.inner.raise_window(window)
    }

    fn focus_window(&mut self, window: &crate::PhysicalWindowId) -> bool {
        self.inner.focus_window(window)
    }

    fn pointer_location(&mut self) -> Option<(i32, i32)> {
        self.inner.pointer_location()
    }

    fn mouse_down(&mut self) -> bool {
        self.inner.mouse_down()
    }

    fn warp_pointer(&mut self, point: (i32, i32)) -> bool {
        self.inner.warp_pointer(point)
    }

    fn apply_managed(
        &mut self,
        placements: &[crate::Placement],
        settings: &Settings,
        cancelled: &AtomicBool,
    ) -> Vec<macos::ApplyResult> {
        self.counts.apply_managed += 1;
        self.inner.apply_managed(placements, settings, cancelled)
    }

    fn apply_managed_during_drag(
        &mut self,
        placements: &[crate::Placement],
        settings: &Settings,
        dragged: &crate::PhysicalWindowId,
        cancelled: &AtomicBool,
    ) -> Vec<macos::ApplyResult> {
        self.counts.apply_managed += 1;
        self.inner
            .apply_managed_during_drag(placements, settings, dragged, cancelled)
    }

    fn persist_parked(
        &mut self,
        parked: &HashMap<crate::PhysicalWindowId, crate::Rect>,
    ) -> Result<(), String> {
        self.counts.persist_parked += 1;
        if let Some(path) = &self.parked_path {
            let entries: Vec<(&crate::PhysicalWindowId, &crate::Rect)> = parked.iter().collect();
            let bytes = serde_json::to_vec(&entries)
                .map_err(|error| format!("failed to serialize parked bench state: {error}"))?;
            crate::state_file::write_atomic(path, &bytes)
                .map_err(|error| format!("failed to write parked bench state: {error}"))?;
        }
        self.inner.persist_parked(parked)
    }

    fn persist_workspace_state(
        &mut self,
        state: &crate::workspace::WorkspaceState,
    ) -> Result<(), String> {
        self.counts.persist_workspace_state += 1;
        self.inner.persist_workspace_state(state)
    }

    fn persist_window_memory(
        &mut self,
        memory: &crate::window_memory::WindowMemory,
    ) -> Result<(), String> {
        self.counts.persist_window_memory += 1;
        self.inner.persist_window_memory(memory)
    }

    fn persist_workspace_monitor(
        &mut self,
        workspace: WorkspaceId,
        selector: &DisplaySelector,
    ) -> Result<(), String> {
        self.inner.persist_workspace_monitor(workspace, selector)
    }

    fn journal(&mut self, line: &crate::journal::JournalLine) {
        self.inner.journal(line);
    }
}

fn print_stats(name: &str, unit: &str, samples: &mut [f64]) {
    samples.sort_by(f64::total_cmp);
    let n = samples.len();
    let median = samples[n / 2];
    let mean = samples.iter().sum::<f64>() / n as f64;
    let p95 = samples[((n as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(n - 1)];
    let min = samples[0];
    let max = samples[n - 1];
    println!(
        "BENCH {name}: median={median:.3}{unit} mean={mean:.3}{unit} p95={p95:.3}{unit} min={min:.3}{unit} max={max:.3}{unit} (n={n})"
    );
}

fn bench_temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spaceman-bench-{name}-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_desktop_runtime(
    windows_per_workspace: u32,
    real_io: bool,
    parked_path: Option<PathBuf>,
) -> WorkspaceRuntime<BenchBackend> {
    let out = outputs();
    let displays: Vec<_> = out.frames().collect();
    let main_output = out.main().frame;
    let sec_output = out.displays()[1].frame;
    let mut frames = HashMap::new();
    let mut parked = HashMap::new();
    let mut apps = Vec::new();
    let mut next_wid = 100u32;

    // Workspaces 1..=5 on main (1 visible, 2..=5 hidden), 6..=8 on secondary (6 visible, 7..=8 hidden)
    let ws_plan: &[(u8, bool, crate::Rect)] = &[
        (1, true, main_output),
        (2, false, main_output),
        (3, false, main_output),
        (4, false, main_output),
        (5, false, main_output),
        (6, true, sec_output),
        (7, false, sec_output),
        (8, false, sec_output),
    ];
    let mut ws_windows: Vec<(WorkspaceId, Vec<crate::PhysicalWindowId>)> = Vec::new();
    let mut focused = None;
    let mut frontmost = None;

    for (app_idx, &(ws_num, visible, rect)) in ws_plan.iter().enumerate() {
        let ws = WorkspaceId::new(ws_num).unwrap();
        let app = AppInstanceId {
            pid: (app_idx as i32) + 10,
            launched_at: 1000 + app_idx as u64,
        };
        let mut obs_windows = Vec::new();
        let mut owned = Vec::new();
        for _ in 0..windows_per_workspace {
            let wid = crate::PhysicalWindowId {
                app: app.clone(),
                window_id: next_wid,
            };
            next_wid += 1;
            let normal = crate::Rect {
                x: rect.x + 20,
                y: rect.y + 20,
                width: 400,
                height: 300,
            };
            if visible {
                frames.insert(wid.clone(), normal);
                obs_windows.push(crate::ObservedWindow {
                    id: wid.clone(),
                    frame: normal,
                    resizable: true,
                    title: None,
                });
                if focused.is_none() {
                    focused = Some(wid.clone());
                    frontmost = Some(app.clone());
                }
            } else {
                let p = crate::geometry::parked_frame(rect, &displays, normal);
                frames.insert(wid.clone(), p);
                parked.insert(wid.clone(), normal);
            }
            owned.push(wid);
        }
        apps.push(crate::AppObservation::Complete {
            app: crate::ObservedApplication {
                id: app,
                bundle_id: format!("com.example.App{ws_num}"),
            },
            focused: if visible {
                owned.first().cloned()
            } else {
                None
            },
            windows: obs_windows,
        });
        ws_windows.push((ws, owned));
    }

    let fake = FakeBackend {
        frames,
        focused,
        frontmost,
        observation: Some(crate::NativeObservation {
            cycle: 0,
            mouse_down: false,
            applications: apps,
        }),
        ..FakeBackend::default()
    };
    let backend = BenchBackend::new(fake, real_io, parked_path);
    let mut rt = WorkspaceRuntime::with_backend(ManagementScope::Both, backend, parked);
    for (ws, windows) in ws_windows {
        for wid in windows {
            rt.workspaces.engine_mut(ws).insert_slot(Slot {
                id: SlotId(u64::from(wid.window_id)),
                weight: 1,
                binding: BindingState::Bound(wid),
            });
        }
    }
    let cancelled = AtomicBool::new(false);
    rt.cycle(out, &cancelled).unwrap();
    rt.backend.reset_counts();
    rt
}

#[test]
#[ignore = "performance benchmark"]
fn perf_bench_realistic_steady_state_worker_retile() {
    let out = outputs();
    let cancelled = AtomicBool::new(false);

    // 1) Pure in-memory realistic desktop (40 windows: 10 visible + 30 parked across 8 workspaces)
    let mut rt = make_desktop_runtime(5, false, None);
    for _ in 0..20 {
        rt.cycle(out.clone(), &cancelled).unwrap();
    }
    rt.backend.reset_counts();
    let mut cycle_samples = Vec::new();
    for _ in 0..15 {
        let start = Instant::now();
        for _ in 0..100 {
            std::hint::black_box(rt.cycle(out.clone(), &cancelled).unwrap());
        }
        cycle_samples.push(start.elapsed().as_secs_f64() * 10_000.0);
    }
    print_stats("realistic_40w_cycle", "us", &mut cycle_samples);
    rt.backend.counts.print("realistic_40w_cycle_1500x");

    // 2) Full worker execute(Retile) in-memory (includes debug_state, sync_scratchpad, etc.)
    rt.backend.reset_counts();
    let mut exec_samples = Vec::new();
    for _ in 0..15 {
        let start = Instant::now();
        for _ in 0..100 {
            std::hint::black_box(rt.execute(
                WorkerCommand::Action(WorkerAction::Retile, out.clone()),
                &cancelled,
            ));
        }
        exec_samples.push(start.elapsed().as_secs_f64() * 10_000.0);
    }
    print_stats(
        "realistic_40w_worker_execute_retile",
        "us",
        &mut exec_samples,
    );
    rt.backend.counts.print("realistic_40w_exec_1500x");

    // 3) Full worker execute(Retile) with REAL macOS CGWindowList + frontmost_application IPC
    let mut rt_io = make_desktop_runtime(5, true, None);
    for _ in 0..3 {
        rt_io.execute(
            WorkerCommand::Action(WorkerAction::Retile, out.clone()),
            &cancelled,
        );
    }
    rt_io.backend.reset_counts();
    let mut io_samples = Vec::new();
    for _ in 0..25 {
        let start = Instant::now();
        std::hint::black_box(rt_io.execute(
            WorkerCommand::Action(WorkerAction::Retile, out.clone()),
            &cancelled,
        ));
        io_samples.push(start.elapsed().as_secs_f64() * 1_000.0);
    }
    print_stats(
        "realistic_40w_worker_retile_real_macos_ipc",
        "ms",
        &mut io_samples,
    );
    rt_io
        .backend
        .counts
        .print("realistic_40w_retile_real_ipc_25x");
}

#[test]
#[ignore = "performance benchmark"]
fn perf_bench_workspace_switch() {
    let out = outputs();
    let cancelled = AtomicBool::new(false);
    let ws1 = WorkspaceId::ONE;
    let ws2 = WorkspaceId::new(2).unwrap();

    // 1) Pure in-memory workspace switch (6 outgoing + 6 incoming + 30 other parked)
    let mut rt = make_desktop_runtime(6, false, None);
    for i in 0..6 {
        let target = if i % 2 == 0 { ws2 } else { ws1 };
        rt.execute(
            WorkerCommand::Action(WorkerAction::SwitchWorkspace(target), out.clone()),
            &cancelled,
        );
    }
    rt.backend.reset_counts();
    let mut mem_samples = Vec::new();
    for i in 0..40 {
        let target = if i % 2 == 0 { ws2 } else { ws1 };
        let start = Instant::now();
        std::hint::black_box(rt.execute(
            WorkerCommand::Action(WorkerAction::SwitchWorkspace(target), out.clone()),
            &cancelled,
        ));
        mem_samples.push(start.elapsed().as_secs_f64() * 1_000_000.0);
    }
    print_stats("workspace_switch_6w_in_memory", "us", &mut mem_samples);
    rt.backend.counts.print("workspace_switch_6w_mem_40x");

    // 2) REAL macOS APFS write_atomic + REAL macOS CGWindowList + REAL frontmost_application
    let dir = bench_temp_dir("switch");
    let parked_path = dir.join("parked-windows.json");
    let mut rt_io = make_desktop_runtime(6, true, Some(parked_path));
    for i in 0..4 {
        let target = if i % 2 == 0 { ws2 } else { ws1 };
        rt_io.execute(
            WorkerCommand::Action(WorkerAction::SwitchWorkspace(target), out.clone()),
            &cancelled,
        );
    }
    rt_io.backend.reset_counts();
    let mut io_samples = Vec::new();
    for i in 0..20 {
        let target = if i % 2 == 0 { ws2 } else { ws1 };
        let start = Instant::now();
        std::hint::black_box(rt_io.execute(
            WorkerCommand::Action(WorkerAction::SwitchWorkspace(target), out.clone()),
            &cancelled,
        ));
        io_samples.push(start.elapsed().as_secs_f64() * 1_000.0);
    }
    print_stats("workspace_switch_6w_real_macos_io", "ms", &mut io_samples);
    rt_io
        .backend
        .counts
        .print("workspace_switch_6w_real_io_20x");
    drop(std::fs::remove_dir_all(&dir));
}

#[test]
#[ignore = "performance benchmark"]
fn perf_bench_move_workspace_and_restore_all() {
    let out = outputs();
    let cancelled = AtomicBool::new(false);
    let dir = bench_temp_dir("move_restore");
    let parked_path = dir.join("parked-windows.json");

    // 1) move_workspace_to_display (6 moved + 6 source replacement + 6 displaced) with real macOS I/O
    let mut rt = make_desktop_runtime(6, true, Some(parked_path.clone()));
    rt.execute(
        WorkerCommand::Action(
            WorkerAction::MoveWorkspaceToDisplay(DisplayTarget::Next),
            out.clone(),
        ),
        &cancelled,
    );
    rt.execute(
        WorkerCommand::Action(
            WorkerAction::MoveWorkspaceToDisplay(DisplayTarget::Prev),
            out.clone(),
        ),
        &cancelled,
    );
    rt.backend.reset_counts();
    let mut move_samples = Vec::new();
    for i in 0..14 {
        let dir_target = if i % 2 == 0 {
            DisplayTarget::Next
        } else {
            DisplayTarget::Prev
        };
        let start = Instant::now();
        std::hint::black_box(rt.execute(
            WorkerCommand::Action(
                WorkerAction::MoveWorkspaceToDisplay(dir_target),
                out.clone(),
            ),
            &cancelled,
        ));
        move_samples.push(start.elapsed().as_secs_f64() * 1_000.0);
    }
    print_stats("move_workspace_6w_real_macos_io", "ms", &mut move_samples);
    rt.backend.counts.print("move_workspace_6w_real_io_14x");

    // 2) restore_all (36 parked windows across 6 hidden workspaces) with real macOS I/O
    let mut restore_samples = Vec::new();
    let mut restore_counts = BackendCounters::default();
    for _ in 0..10 {
        let mut rt_r = make_desktop_runtime(6, true, Some(parked_path.clone()));
        rt_r.backend.reset_counts();
        let start = Instant::now();
        std::hint::black_box(
            rt_r.execute(WorkerCommand::RestoreAll(Some(out.clone())), &cancelled),
        );
        restore_samples.push(start.elapsed().as_secs_f64() * 1_000.0);
        restore_counts.window_server_frame += rt_r.backend.counts.window_server_frame;
        restore_counts.window_server_frames += rt_r.backend.counts.window_server_frames;
        restore_counts.persist_parked += rt_r.backend.counts.persist_parked;
        restore_counts.set_position += rt_r.backend.counts.set_position;
    }
    print_stats("restore_all_36w_real_macos_io", "ms", &mut restore_samples);
    restore_counts.print("restore_all_36w_real_io_10x");
    drop(std::fs::remove_dir_all(&dir));
}

#[test]
#[ignore = "performance benchmark"]
fn perf_bench_macos_primitives_and_engine_micro() {
    let dir = bench_temp_dir("primitives");
    let parked_path = dir.join("parked-windows.json");
    let sample_parked: HashMap<crate::PhysicalWindowId, crate::Rect> = (1..=30)
        .map(|id| (window(id), frame((id as i32) * 10)))
        .collect();
    let sample_entries: Vec<(&crate::PhysicalWindowId, &crate::Rect)> =
        sample_parked.iter().collect();
    let sample_bytes = serde_json::to_vec(&sample_entries).unwrap();

    // Real APFS write_atomic
    for _ in 0..5 {
        crate::state_file::write_atomic(&parked_path, &sample_bytes).unwrap();
    }
    let mut fsync_samples = Vec::new();
    for _ in 0..30 {
        let start = Instant::now();
        crate::state_file::write_atomic(&parked_path, &sample_bytes).unwrap();
        fsync_samples.push(start.elapsed().as_secs_f64() * 1_000.0);
    }
    print_stats("macos_write_atomic_30w", "ms", &mut fsync_samples);

    // Real CGWindowListCopyWindowInfo(OptionAll) vs OptionOnScreenOnly vs single window_server_frame
    let probe_wid = macos::onscreen_window_frames()
        .and_then(|frames| frames.keys().next().copied())
        .or_else(|| macos::window_server_frames().and_then(|frames| frames.keys().next().copied()))
        .unwrap_or(1);
    let probe = crate::PhysicalWindowId {
        app: AppInstanceId {
            pid: std::process::id() as i32,
            launched_at: 1,
        },
        window_id: probe_wid,
    };
    let all_count = macos::window_server_frames().map_or(0, |m| m.len());
    let onscreen_count = macos::onscreen_window_frames().map_or(0, |m| m.len());
    println!(
        "INFO cg_window_counts all={all_count} onscreen={onscreen_count} probe_wid={probe_wid}"
    );

    let mut cg_all = Vec::new();
    for _ in 0..30 {
        let start = Instant::now();
        std::hint::black_box(macos::window_server_frames());
        cg_all.push(start.elapsed().as_secs_f64() * 1_000.0);
    }
    print_stats("macos_cg_window_server_frames_all", "ms", &mut cg_all);

    let mut cg_onscreen = Vec::new();
    for _ in 0..30 {
        let start = Instant::now();
        std::hint::black_box(macos::onscreen_window_frames());
        cg_onscreen.push(start.elapsed().as_secs_f64() * 1_000.0);
    }
    print_stats("macos_cg_onscreen_window_frames", "ms", &mut cg_onscreen);

    let mut cg_single = Vec::new();
    for _ in 0..30 {
        let start = Instant::now();
        std::hint::black_box(macos::window_server_frame(&probe));
        cg_single.push(start.elapsed().as_secs_f64() * 1_000.0);
    }
    print_stats("macos_cg_single_window_server_frame", "ms", &mut cg_single);

    let mut frontmost_samples = Vec::new();
    for _ in 0..30 {
        let start = Instant::now();
        std::hint::black_box(macos::frontmost_application());
        frontmost_samples.push(start.elapsed().as_secs_f64() * 1_000_000.0);
    }
    print_stats("macos_frontmost_application", "us", &mut frontmost_samples);

    // Core engine micro: journal::diff unchanged on 256 windows
    let arr = crate::journal::Arrangement {
        displays: vec![frame(0), frame(1000)],
        active: vec![WorkspaceId::ONE, WorkspaceId::SIX],
        windows: (1..=256)
            .map(|id| crate::journal::ArrangedWindow {
                window: id,
                pid: 1,
                bundle_id: "com.example.App".into(),
                frame: Some(frame(id as i32)),
                owner: Some(WorkspaceId::ONE),
                role: crate::journal::WindowRole::Tiled,
                parked: false,
            })
            .collect(),
    };
    let mut diff_samples = Vec::new();
    for _ in 0..25 {
        let start = Instant::now();
        for _ in 0..200 {
            std::hint::black_box(crate::journal::diff(&arr, &arr));
        }
        diff_samples.push(start.elapsed().as_secs_f64() * 5_000.0);
    }
    print_stats(
        "engine_journal_diff_256w_unchanged",
        "us",
        &mut diff_samples,
    );

    // Core engine micro: reconcile::reconcile (256 windows steady-state) & LayoutTree::placements
    let mut rt_256 = runtime(benchmark_backend(256));
    rt_256.cycle(outputs(), &AtomicBool::new(false)).unwrap();
    let settings = rt_256.layout_settings(outputs().main().frame);
    let mut place_samples = Vec::new();
    for _ in 0..25 {
        let start = Instant::now();
        for _ in 0..500 {
            std::hint::black_box(
                rt_256
                    .workspaces
                    .engine(WorkspaceId::ONE)
                    .placements(&settings),
            );
        }
        place_samples.push(start.elapsed().as_secs_f64() * 2_000.0);
    }
    print_stats("engine_placements_128w", "us", &mut place_samples);

    let mut bound_samples = Vec::new();
    for _ in 0..25 {
        let start = Instant::now();
        for _ in 0..500 {
            std::hint::black_box(rt_256.workspaces.engine(WorkspaceId::ONE).bound_windows());
        }
        bound_samples.push(start.elapsed().as_secs_f64() * 2_000.0);
    }
    print_stats("engine_bound_windows_128w", "us", &mut bound_samples);

    let mut ws_state_samples = Vec::new();
    for _ in 0..25 {
        let start = Instant::now();
        for _ in 0..500 {
            rt_256.persist_workspace_state_if_changed().unwrap();
        }
        ws_state_samples.push(start.elapsed().as_secs_f64() * 2_000.0);
    }
    print_stats(
        "engine_persist_ws_if_changed_256w",
        "us",
        &mut ws_state_samples,
    );

    drop(std::fs::remove_dir_all(&dir));
}
