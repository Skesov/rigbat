use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::time::timeout;

use super::*;
use crate::app::test_support::{denied, device, err, fake, ok, panics, reading, reassigned};
use crate::domain::Transport;
use crate::test_support::ScratchDir;

const TIMEOUT: Duration = Duration::from_secs(5);

/// Stand-ins for `config::load`/`config::save`, so no test reads or
/// rewrites the developer's real config file.
fn no_migration_load() -> Config {
    Config::default()
}

fn no_migration_save(_cfg: &Config) -> anyhow::Result<()> {
    Ok(())
}

fn spawn_test<F, Fut>(
    config_tx: watch::Sender<Config>,
    discover: F,
) -> (watch::Receiver<TrayState>, RefreshSignal)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Vec<BackendSweep>> + Send + 'static,
{
    Supervisor::spawn_with(
        config_tx,
        discover,
        no_migration_load,
        no_migration_save,
        None,
        ConfigRole::Owner,
    )
}

fn ok_sweep(name: &'static str, sources: Vec<Box<dyn BatterySource>>) -> BackendSweep {
    BackendSweep {
        name,
        result: Ok(sources),
    }
}

/// A discovery error, distinct from an honest `ok_sweep(name, vec![])`.
fn err_sweep(name: &'static str, msg: &str) -> BackendSweep {
    BackendSweep {
        name,
        result: Err(anyhow::anyhow!(msg.to_owned())),
    }
}

/// One successful `sysfs` sweep that finds `sources`.
fn found(sources: Vec<Box<dyn BatterySource>>) -> impl Future<Output = Vec<BackendSweep>> + Send {
    let sweeps = vec![ok_sweep("sysfs", sources)];
    async move { sweeps }
}

fn counting(info: &DeviceInfo, polls: &Arc<AtomicUsize>) -> Box<dyn BatterySource> {
    let polls = polls.clone();
    fake(info, move || {
        polls.fetch_add(1, Ordering::Relaxed);
        Ok(reading(50))
    })
}

async fn wait_for(
    rx: &mut watch::Receiver<TrayState>,
    what: &str,
    done: impl Fn(&TrayState) -> bool,
) -> TrayState {
    timeout(TIMEOUT, async {
        loop {
            {
                let state = rx.borrow().clone();
                if done(&state) {
                    return state;
                }
            }
            rx.changed().await.expect("watch channel closed");
        }
    })
    .await
    .map_err(|_| format!("timed out waiting for {what}"))
    .unwrap()
}

async fn wait_for_connected(rx: &mut watch::Receiver<TrayState>) -> TrayState {
    wait_for(rx, "a device online", |state| {
        state.devices.iter().any(|d| d.presence == Presence::Online)
    })
    .await
}

async fn wait_for_devices(rx: &mut watch::Receiver<TrayState>) -> TrayState {
    wait_for(rx, "devices to appear", |state| !state.devices.is_empty()).await
}

/// Sources are instant; this gives the spawned tasks a chance to run.
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn primary_is_first_connected() {
    let (tx, _config_rx) = watch::channel(Config::default());
    let (mut rx, _refresh) = spawn_test(tx, || {
        found(vec![err(&device("mouse")), ok(&device("keyboard"), 80)])
    });

    let state = wait_for_connected(&mut rx).await;
    assert_eq!(state.devices.len(), 2);
    let keyboard = state
        .devices
        .iter()
        .find(|d| d.info.name == "keyboard")
        .expect("keyboard present");
    assert_eq!(keyboard.last_reading, Some(reading(80)));
}

#[tokio::test]
async fn all_err_stays_offline() {
    let (tx, _config_rx) = watch::channel(Config::default());
    let (rx, _refresh) = spawn_test(tx, || {
        found(vec![err(&device("mouse")), err(&device("keyboard"))])
    });
    settle().await;

    let state = rx.borrow().clone();
    assert_eq!(state.devices.len(), 2);
    assert!(state.devices.iter().all(|d| d.last_reading.is_none()));
}

#[tokio::test]
async fn empty_sources() {
    let (tx, _config_rx) = watch::channel(Config::default());
    let (rx, _refresh) = spawn_test(tx, || async { Vec::<BackendSweep>::new() });
    settle().await;

    assert!(rx.borrow().devices.is_empty());
}

#[tokio::test]
async fn hotplug_adds_device() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (tx, _config_rx) = watch::channel(Config::default());
    let (mut rx, refresh) = spawn_test(tx, move || {
        let sources = if calls.fetch_add(1, Ordering::Relaxed) == 0 {
            vec![]
        } else {
            vec![ok(&device("kbd"), 70)]
        };
        found(sources)
    });
    settle().await;
    assert!(rx.borrow().devices.is_empty());

    refresh.trigger();

    let state = wait_for_devices(&mut rx).await;
    assert_eq!(state.devices.len(), 1);
    assert_eq!(state.devices[0].info.name, "kbd");
}

/// The interval is an hour, so a rising poll count within the timeout
/// can only come from the `config_rx.changed()` arm, not the sleep.
#[tokio::test]
async fn config_update_shortens_interval_immediately() {
    let initial = Config {
        poll_interval_secs: 3600,
        ..Config::default()
    };
    let (tx, _config_rx) = watch::channel(initial);
    let polls = Arc::new(AtomicUsize::new(0));
    let (mut rx, _refresh) = spawn_test(tx.clone(), {
        let polls = polls.clone();
        move || found(vec![counting(&device("mouse"), &polls)])
    });

    wait_for_connected(&mut rx).await;
    let before = polls.load(Ordering::Relaxed);

    let shortened = Config {
        poll_interval_secs: 1,
        ..Config::default()
    };
    tx.send(shortened).expect("config receiver still alive");

    timeout(TIMEOUT, async {
        loop {
            tokio::task::yield_now().await;
            if polls.load(Ordering::Relaxed) > before {
                return;
            }
        }
    })
    .await
    .expect("poll count did not rise after config update");
}

/// A change that does not touch the polling interval (palette, theme,
/// language) re-renders, it does not send a query to every device.
#[tokio::test(start_paused = true)]
async fn a_cosmetic_config_change_polls_nothing() {
    let initial = Config {
        poll_interval_secs: 3600,
        ..Config::default()
    };
    let (tx, _config_rx) = watch::channel(initial.clone());
    let polls = Arc::new(AtomicUsize::new(0));
    let (mut rx, _refresh) = spawn_test(tx.clone(), {
        let polls = polls.clone();
        move || found(vec![counting(&device("mouse"), &polls)])
    });
    wait_for_connected(&mut rx).await;
    let before = polls.load(Ordering::Relaxed);

    tx.send(Config {
        palette: crate::domain::Palette::Nord,
        language: Some("ru".to_owned()),
        ..initial
    })
    .expect("config receiver still alive");
    tokio::time::sleep(Duration::from_secs(10)).await;

    assert_eq!(polls.load(Ordering::Relaxed), before);
}

/// Inserts a bare entry directly, bypassing `reconcile`, so `record`/
/// `prune_stale` can be tested in isolation from task spawning.
fn insert_entry(
    registry: &mut DeviceRegistry,
    info: &DeviceInfo,
    presence: Presence,
    last_reading: Option<BatteryReading>,
    last_seen: Option<BootTime>,
    consecutive_failures: u32,
) {
    registry.order.push(info.id());
    registry.entries.insert(
        info.id(),
        DeviceEntry {
            info: info.clone(),
            last_reading,
            last_seen,
            presence,
            consecutive_failures,
            battery_history: Vec::new(),
        },
    );
}

fn insert_online(registry: &mut DeviceRegistry, info: &DeviceInfo, percent: u8) {
    insert_entry(
        registry,
        info,
        Presence::Online,
        Some(reading(percent)),
        Some(crate::clock::now()),
        0,
    );
}

/// The context, the receiving end of its reports, and the config sender,
/// which must outlive the test so `config_rx.changed()` waits instead of
/// erroring.
fn source_ctx() -> (SourceCtx, mpsc::Receiver<SourceMsg>, watch::Sender<Config>) {
    let (config_tx, config_rx) = watch::channel(Config::default());
    let (tx, rx) = mpsc::channel(64);
    let ctx = SourceCtx {
        tx,
        config_rx,
        refresh: RefreshSignal::new(),
    };
    (ctx, rx, config_tx)
}

async fn sweep(
    registry: &mut DeviceRegistry,
    ctx: &SourceCtx,
    sources: Vec<Box<dyn BatterySource>>,
) -> Vec<Rename> {
    registry
        .reconcile(vec![ok_sweep("sysfs", sources)], ctx)
        .await
}

async fn next_msg(rx: &mut mpsc::Receiver<SourceMsg>) -> SourceMsg {
    timeout(TIMEOUT, rx.recv())
        .await
        .expect("timed out waiting for the task's report")
        .expect("mpsc channel closed")
}

/// Gives an `insert_entry` device a live task that never ends.
fn attach_task(registry: &mut DeviceRegistry, id: &DeviceId) {
    registry.last_generation += 1;
    let task = SourceTask {
        handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
        generation: registry.last_generation,
        // Arbitrary: no caller exercises backend-ownership retirement.
        backend: "test",
    };
    registry.tasks.insert(id.clone(), task);
}

fn live_generation(registry: &DeviceRegistry, id: &DeviceId) -> u64 {
    registry.tasks.get(id).map_or(0, |task| task.generation)
}

fn record_live(registry: &mut DeviceRegistry, id: &DeviceId, reading: Option<BatteryReading>) {
    let generation = live_generation(registry, id);
    registry.record(id, generation, reading);
}

#[tokio::test]
async fn reading_from_a_retired_generation_is_dropped() {
    let (ctx, _, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");

    sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
    let first = live_generation(&registry, &a.id());
    registry.record(&a.id(), first, Some(reading(80)));

    sweep(&mut registry, &ctx, vec![]).await;
    sweep(&mut registry, &ctx, vec![ok(&a, 55)]).await;
    let second = live_generation(&registry, &a.id());
    assert_ne!(first, second);

    registry.record(&a.id(), first, Some(reading(10)));
    let snapshot = registry.snapshot();
    assert_eq!(snapshot[0].presence, Presence::Disconnected);
    assert_eq!(snapshot[0].last_reading, Some(reading(80)));

    registry.record(&a.id(), second, Some(reading(55)));
    let snapshot = registry.snapshot();
    assert_eq!(snapshot[0].presence, Presence::Online);
    assert_eq!(snapshot[0].last_reading, Some(reading(55)));
}

#[tokio::test]
async fn reading_from_an_aborted_task_does_not_revive_a_vanished_device() {
    let (ctx, _, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");

    sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
    let generation = live_generation(&registry, &a.id());
    registry.record(&a.id(), generation, Some(reading(80)));
    sweep(&mut registry, &ctx, vec![]).await;

    registry.record(&a.id(), generation, Some(reading(79)));

    assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);
}

#[tokio::test]
async fn reassigned_node_ends_the_task_and_disconnects_the_device() {
    let (ctx, mut rx, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");
    insert_online(&mut registry, &a, 80);
    registry.spawn(reassigned(&a), a.id(), "test", &ctx);

    let msg = next_msg(&mut rx).await;
    assert!(matches!(msg.event, SourceEvent::NodeReassigned));
    assert_eq!(msg.generation, live_generation(&registry, &a.id()));
    registry.retire(&msg.id, msg.generation);

    assert!(!registry.tasks.contains_key(&a.id()));
    let snapshot = registry.snapshot();
    assert_eq!(snapshot[0].presence, Presence::Disconnected);
    assert_eq!(snapshot[0].last_reading, Some(reading(80)));
}

/// The task keeps polling: access can be granted without the device leaving.
#[tokio::test]
async fn access_denied_is_reported_as_its_own_event_and_the_task_keeps_running() {
    let (ctx, mut rx, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");
    insert_entry(&mut registry, &a, Presence::Unreachable, None, None, 0);
    registry.spawn(denied(&a), a.id(), "test", &ctx);

    let msg = next_msg(&mut rx).await;
    assert!(matches!(msg.event, SourceEvent::AccessDenied));
    registry.deny(&msg.id, msg.generation);

    assert_eq!(registry.snapshot()[0].presence, Presence::NoAccess);
    tokio::task::yield_now().await;
    assert!(!registry.tasks[&a.id()].handle.is_finished());
}

#[tokio::test]
async fn access_denial_shows_at_once_and_a_reading_clears_it() {
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");
    insert_online(&mut registry, &a, 80);
    attach_task(&mut registry, &a.id());

    registry.deny(&a.id(), live_generation(&registry, &a.id()));
    let snapshot = registry.snapshot();
    assert_eq!(snapshot[0].presence, Presence::NoAccess);
    assert_eq!(snapshot[0].last_reading, Some(reading(80)));

    record_live(&mut registry, &a.id(), Some(reading(79)));
    assert_eq!(registry.snapshot()[0].presence, Presence::Online);
}

#[tokio::test]
async fn a_silent_poll_after_access_is_granted_reads_unreachable_not_no_access() {
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");
    insert_entry(&mut registry, &a, Presence::Unreachable, None, None, 0);
    attach_task(&mut registry, &a.id());

    registry.deny(&a.id(), live_generation(&registry, &a.id()));
    record_live(&mut registry, &a.id(), None);

    assert_eq!(registry.snapshot()[0].presence, Presence::Unreachable);
}

#[tokio::test]
async fn access_denial_from_a_stale_generation_is_ignored() {
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");
    insert_online(&mut registry, &a, 80);
    attach_task(&mut registry, &a.id());
    let stale = live_generation(&registry, &a.id());
    attach_task(&mut registry, &a.id());

    registry.deny(&a.id(), stale);

    assert_eq!(registry.snapshot()[0].presence, Presence::Online);
}

#[tokio::test]
async fn retire_from_a_stale_generation_keeps_the_live_task() {
    let (ctx, _, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");

    sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
    let first = live_generation(&registry, &a.id());
    sweep(&mut registry, &ctx, vec![]).await;
    sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
    let second = live_generation(&registry, &a.id());
    registry.record(&a.id(), second, Some(reading(80)));

    registry.retire(&a.id(), first);
    assert_eq!(live_generation(&registry, &a.id()), second);
    assert_eq!(registry.snapshot()[0].presence, Presence::Online);

    registry.retire(&a.id(), second);
    assert!(!registry.tasks.contains_key(&a.id()));

    // The next sweep that finds the device spawns afresh, not via the crash path.
    sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
    assert!(live_generation(&registry, &a.id()) > second);
    assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);
}

#[tokio::test]
async fn registry_record_then_snapshot_preserves_discovery_order() {
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");
    let b = device("b");
    insert_entry(&mut registry, &a, Presence::Unreachable, None, None, 0);
    insert_entry(&mut registry, &b, Presence::Unreachable, None, None, 0);
    attach_task(&mut registry, &a.id());
    attach_task(&mut registry, &b.id());

    record_live(&mut registry, &a.id(), Some(reading(10)));
    record_live(&mut registry, &b.id(), Some(reading(20)));

    let snapshot = registry.snapshot();
    assert_eq!(snapshot.len(), 2);
    assert_eq!(snapshot[0].info.name, "a");
    assert_eq!(snapshot[0].last_reading, Some(reading(10)));
    assert_eq!(snapshot[0].presence, Presence::Online);
    assert_eq!(snapshot[1].info.name, "b");
    assert_eq!(snapshot[1].last_reading, Some(reading(20)));
}

#[test]
fn registry_ignores_reading_for_unregistered_id() {
    let mut registry = DeviceRegistry::new(None);
    let ghost = device("ghost");

    record_live(&mut registry, &ghost.id(), Some(reading(99)));

    assert!(registry.snapshot().is_empty());
}

#[tokio::test]
async fn one_failed_poll_keeps_online_and_reading() {
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");
    insert_online(&mut registry, &a, 80);

    attach_task(&mut registry, &a.id());
    record_live(&mut registry, &a.id(), None);

    let snapshot = registry.snapshot();
    assert_eq!(snapshot[0].presence, Presence::Online);
    assert_eq!(snapshot[0].last_reading, Some(reading(80)));
}

#[tokio::test]
async fn offline_after_failures_flips_to_unreachable() {
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");
    insert_online(&mut registry, &a, 80);

    attach_task(&mut registry, &a.id());
    for _ in 0..OFFLINE_AFTER_FAILURES {
        record_live(&mut registry, &a.id(), None);
    }

    let snapshot = registry.snapshot();
    assert_eq!(snapshot[0].presence, Presence::Unreachable);
    // The reading stays retained even once Unreachable.
    assert_eq!(snapshot[0].last_reading, Some(reading(80)));
}

#[tokio::test]
async fn success_after_failures_resets_counter_and_online() {
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");
    insert_entry(
        &mut registry,
        &a,
        Presence::Unreachable,
        Some(reading(80)),
        Some(crate::clock::now()),
        OFFLINE_AFTER_FAILURES - 1,
    );

    attach_task(&mut registry, &a.id());
    record_live(&mut registry, &a.id(), Some(reading(75)));
    assert_eq!(registry.entries[&a.id()].consecutive_failures, 0);
    assert_eq!(registry.snapshot()[0].presence, Presence::Online);

    // Counter was reset — a single further failure must not re-trip
    // Unreachable immediately.
    record_live(&mut registry, &a.id(), None);
    assert_eq!(registry.snapshot()[0].presence, Presence::Online);
}

#[tokio::test]
async fn vanished_device_becomes_disconnected_keeps_reading_and_aborts_task() {
    let (ctx, _, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");

    sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
    record_live(&mut registry, &a.id(), Some(reading(80)));
    assert!(registry.tasks.contains_key(&a.id()));

    sweep(&mut registry, &ctx, vec![]).await;

    assert!(!registry.tasks.contains_key(&a.id()));
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].presence, Presence::Disconnected);
    assert_eq!(snapshot[0].last_reading, Some(reading(80)));
    assert!(snapshot[0].last_seen.is_some());
}

/// A panicked task never calls `record`, so its stale `Online` reading
/// must not survive the sweep that notices `is_finished()`. Prints
/// tokio's panic message to stderr — expected, not a failure.
#[tokio::test]
async fn crashed_task_is_demoted_and_respawned() {
    let (ctx, mut rx, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");

    sweep(&mut registry, &ctx, vec![panics(&a)]).await;
    settle().await;
    assert!(registry.tasks[&a.id()].handle.is_finished());

    // Simulate the device having been Online with a stale reading right
    // before its task crashed.
    record_live(&mut registry, &a.id(), Some(reading(80)));
    assert_eq!(registry.snapshot()[0].presence, Presence::Online);

    // Next sweep still sees the device, with a fresh replacement source.
    sweep(&mut registry, &ctx, vec![ok(&a, 55)]).await;

    let snapshot = registry.snapshot();
    assert_eq!(snapshot[0].presence, Presence::Unreachable);
    assert_eq!(snapshot[0].last_reading, Some(reading(80)));

    // The replacement task is running — its first poll proves the respawn.
    let msg = next_msg(&mut rx).await;
    assert_eq!(msg.id, a.id());
    let SourceEvent::Polled(polled) = msg.event else {
        unreachable!("replacement task reported a reassigned node");
    };
    registry.record(&msg.id, msg.generation, polled);

    let snapshot = registry.snapshot();
    assert_eq!(snapshot[0].presence, Presence::Online);
    assert_eq!(snapshot[0].last_reading, Some(reading(55)));
}

/// `reconcile` only inspects `is_finished()` for ids still in the fresh
/// set, so a vanished device is aborted, not mistaken for a crash.
#[tokio::test]
async fn vanished_device_is_not_reported_as_crashed() {
    let (ctx, _, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");

    sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
    record_live(&mut registry, &a.id(), Some(reading(80)));

    sweep(&mut registry, &ctx, vec![]).await;

    assert!(!registry.tasks.contains_key(&a.id()));
    assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);
}

#[tokio::test]
async fn disconnected_without_a_reading_is_dropped_immediately() {
    let (ctx, _, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");

    sweep(&mut registry, &ctx, vec![err(&a)]).await;
    sweep(&mut registry, &ctx, vec![]).await;

    assert!(registry.snapshot().is_empty());
}

#[tokio::test]
async fn reappeared_device_reuses_entry_and_returns_online() {
    let (ctx, _, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");

    sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
    record_live(&mut registry, &a.id(), Some(reading(80)));

    sweep(&mut registry, &ctx, vec![]).await;
    assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);

    sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;

    // Same entry reused, not duplicated.
    assert_eq!(registry.snapshot().len(), 1);
    // Retained reading still visible until the fresh task proves online.
    assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);

    record_live(&mut registry, &a.id(), Some(reading(80)));
    assert_eq!(registry.snapshot().len(), 1);
    assert_eq!(registry.snapshot()[0].presence, Presence::Online);
}

/// A failed sweep (a bus hiccup) is not an empty one: the backend's
/// devices stay exactly as they were.
#[tokio::test]
async fn failed_backend_sweep_does_not_retire_its_devices() {
    let (ctx, _, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");

    registry
        .reconcile(vec![ok_sweep("bluez", vec![ok(&a, 80)])], &ctx)
        .await;
    record_live(&mut registry, &a.id(), Some(reading(80)));
    assert!(registry.tasks.contains_key(&a.id()));

    registry
        .reconcile(vec![err_sweep("bluez", "GetManagedObjects failed")], &ctx)
        .await;

    assert!(
        registry.tasks.contains_key(&a.id()),
        "task must keep running across a failed sweep"
    );
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_ne!(
        snapshot[0].presence,
        Presence::Disconnected,
        "presence must not change on a failed sweep"
    );
    assert_eq!(snapshot[0].last_reading, Some(reading(80)));
}

#[tokio::test]
async fn succeeded_backend_sweep_still_retires_its_devices() {
    let (ctx, _, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");

    registry
        .reconcile(vec![ok_sweep("bluez", vec![ok(&a, 80)])], &ctx)
        .await;
    record_live(&mut registry, &a.id(), Some(reading(80)));

    registry
        .reconcile(vec![ok_sweep("bluez", vec![])], &ctx)
        .await;

    assert!(!registry.tasks.contains_key(&a.id()));
    assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);
}

/// `steelseries` and `eightbitdo` both report `Transport::Hidraw`, so
/// only `SourceTask::backend` can tell whose sweep failed.
#[tokio::test]
async fn backend_ownership_does_not_collapse_across_shared_transport() {
    let (ctx, _, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(None);

    let mut mouse = device("aerox");
    mouse.transport = Transport::Hidraw;
    let mut controller = device("ultimate2");
    controller.transport = Transport::Hidraw;

    registry
        .reconcile(
            vec![
                ok_sweep("steelseries", vec![ok(&mouse, 80)]),
                ok_sweep("eightbitdo", vec![ok(&controller, 50)]),
            ],
            &ctx,
        )
        .await;
    record_live(&mut registry, &mouse.id(), Some(reading(80)));
    record_live(&mut registry, &controller.id(), Some(reading(50)));

    // steelseries fails; eightbitdo succeeds and no longer sees its controller.
    registry
        .reconcile(
            vec![
                err_sweep("steelseries", "hidraw read error"),
                ok_sweep("eightbitdo", vec![]),
            ],
            &ctx,
        )
        .await;

    assert!(
        registry.tasks.contains_key(&mouse.id()),
        "steelseries device must survive its own backend's failure"
    );
    assert!(
        !registry.tasks.contains_key(&controller.id()),
        "eightbitdo device is correctly retired despite sharing Transport::Hidraw"
    );
}

#[test]
fn disconnected_entry_older_than_cap_is_pruned() {
    let mut registry = DeviceRegistry::new(None);
    let a = device("a");
    let last_seen = crate::clock::now();
    insert_entry(
        &mut registry,
        &a,
        Presence::Disconnected,
        Some(reading(80)),
        Some(last_seen),
        0,
    );

    // Still within the cap.
    registry.prune_stale(last_seen + DISCONNECTED_RETENTION);
    assert_eq!(registry.snapshot().len(), 1);

    // Past the cap.
    registry.prune_stale(last_seen + DISCONNECTED_RETENTION + Duration::from_secs(1));
    assert!(registry.snapshot().is_empty());
}

#[test]
fn an_offline_icon_never_outlives_its_roster_entry() {
    assert!(crate::domain::OFFLINE_SHELF_LIFE_MAX <= DISCONNECTED_RETENTION);
}

#[tokio::test]
async fn spawn_with_migrates_shown_devices_and_publishes_after_first_sweep() {
    let (tx, _config_rx) = watch::channel(Config::default());
    let mut published = tx.subscribe();

    let (saved_tx, saved) = std::sync::mpsc::channel::<Config>();

    let (_rx, _refresh) = Supervisor::spawn_with(
        tx,
        || found(vec![ok(&device("a"), 50), ok(&device("b"), 50)]),
        || Config {
            shown_devices: vec!["a".to_string()],
            ..Config::default()
        },
        move |cfg: &Config| {
            saved_tx.send(cfg.clone()).expect("test receiver alive");
            Ok(())
        },
        None,
        ConfigRole::Owner,
    );

    // The migration's config_tx.send is the channel's only publisher in
    // this test, so its first change is the migrated config.
    timeout(TIMEOUT, published.changed())
        .await
        .expect("timed out waiting for the migration to publish")
        .expect("config channel closed");

    let migrated = published.borrow().clone();
    assert_eq!(migrated.hidden_devices, vec!["b".to_string()]);
    assert!(migrated.shown_devices.is_empty());
    assert_eq!(saved.try_iter().last().as_ref(), Some(&migrated));
}

#[tokio::test]
async fn a_reader_never_converts_shown_devices() {
    let (tx, config_rx) = watch::channel(Config::default());
    let (saved_tx, saved) = std::sync::mpsc::channel::<Config>();

    let (mut rx, _refresh) = Supervisor::spawn_with(
        tx,
        || found(vec![ok(&device("a"), 50), ok(&device("b"), 50)]),
        || Config {
            shown_devices: vec!["a".to_string()],
            ..Config::default()
        },
        move |cfg: &Config| {
            saved_tx.send(cfg.clone()).expect("test receiver alive");
            Ok(())
        },
        None,
        ConfigRole::Reader,
    );
    // A reading is applied only after the first sweep's conversion attempt.
    wait_for_connected(&mut rx).await;

    assert_eq!(saved.try_iter().count(), 0);
    assert!(!config_rx.has_changed().expect("config channel open"));
}

// Every test above passes no store; these open one in a scratch
// directory, never `state::open()`'s real one.

fn open_scratch_store(test_name: &str) -> (state::Store, ScratchDir) {
    let scratch = ScratchDir::new(test_name);
    let db =
        state::SqliteStore::open(&scratch.join("state.db")).expect("opening scratch state store");
    let store = state::Store::start(db).expect("starting the store thread");
    (store, scratch)
}

#[tokio::test]
async fn reconcile_upserts_devices_row_when_store_present() {
    let (store, _scratch) = open_scratch_store("reconcile-upsert");
    let (ctx, _, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(Some(store.clone()));
    let a = device("a");

    sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;

    let devices = store.list_devices().await.expect("listing devices");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].device, a.id());
}

/// Same transport and locator under a new display name (a BlueZ alias
/// edit): the row is renamed, not doubled, and the caller is told.
#[tokio::test]
async fn reconcile_reports_a_rename_instead_of_adding_a_row() {
    let (store, _scratch) = open_scratch_store("reconcile-rename");
    let (ctx, _, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(Some(store.clone()));

    let before = DeviceInfo {
        name: "MX Anywhere 3".to_owned(),
        kind: DeviceKind::Mouse,
        transport: Transport::Sysfs,
        locator: Some("00:00:5e:00:53:01".to_owned()),
    };
    let after = DeviceInfo {
        name: "Work mouse".to_owned(),
        ..before.clone()
    };

    let renames = sweep(&mut registry, &ctx, vec![ok(&before, 80)]).await;
    assert!(renames.is_empty());

    let renames = sweep(&mut registry, &ctx, vec![ok(&after, 80)]).await;

    assert_eq!(
        renames,
        vec![Rename {
            from: "MX Anywhere 3".to_string(),
            to: "Work mouse".to_string(),
        }]
    );
    let devices = store.list_devices().await.expect("listing devices");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].device, after.id());

    // The live roster must not keep the old name around as a Disconnected
    // duplicate: the tray would show one device twice for a day.
    let roster = registry.snapshot();
    assert_eq!(roster.len(), 1);
    assert_eq!(roster[0].info.name, "Work mouse");
}

/// The config half of the same story: the settings keyed by the old name
/// move to the new one and the result is both saved and republished.
#[test]
fn apply_renames_moves_settings_and_republishes() {
    let (saved_tx, saved) = std::sync::mpsc::channel::<Config>();
    let on_disk = Config {
        hidden_devices: vec!["MX Anywhere 3".to_string()],
        primary_device: Some("MX Anywhere 3".to_string()),
        ..Config::default()
    };
    let (tx, config_rx) = watch::channel(on_disk.clone());
    let config = ConfigIo {
        tx,
        load: move || on_disk.clone(),
        save: move |cfg: &Config| {
            saved_tx.send(cfg.clone()).expect("test receiver alive");
            Ok(())
        },
    };

    config.apply_renames(&[Rename {
        from: "MX Anywhere 3".to_string(),
        to: "Work mouse".to_string(),
    }]);

    let published = config_rx.borrow().clone();
    assert_eq!(published.hidden_devices, vec!["Work mouse".to_string()]);
    assert_eq!(published.primary_device, Some("Work mouse".to_string()));
    assert_eq!(saved.try_iter().last().as_ref(), Some(&published));
}

#[tokio::test]
async fn record_writes_reading_to_store_when_present() {
    let (store, _scratch) = open_scratch_store("record-reading");
    let (ctx, _, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(Some(store.clone()));
    let a = device("a");

    sweep(&mut registry, &ctx, vec![ok(&a, 80)]).await;
    record_live(&mut registry, &a.id(), Some(reading(80)));

    let history = store
        .recent_readings(&a.id(), 10)
        .await
        .expect("reading history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].1, 80);
}

/// A device found for the first time in a fresh process still gets the
/// change-point history a previous run persisted, so the estimate does
/// not start from zero after every tray restart.
#[tokio::test]
async fn newly_discovered_device_seeds_history_from_store() {
    let (store, _scratch) = open_scratch_store("seed-on-discovery");
    let a = device("a");
    let id = a.id();

    store
        .record_seen(vec![(id.clone(), a.kind)], 0)
        .await
        .expect("record_seen");
    for (at, percent) in [(0i64, 80u8), (600, 79), (1200, 78)] {
        store.record_reading(&id, reading(percent), at);
    }

    let (ctx, _, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(Some(store.clone()));

    sweep(&mut registry, &ctx, vec![ok(&a, 78)]).await;

    let entry = registry.entries.get(&id).expect("device entry present");
    let percents: Vec<u8> = entry.battery_history.iter().map(|&(_, p)| p).collect();
    assert_eq!(percents, vec![80, 79, 78]);
}

const HOUR: Duration = Duration::from_secs(60 * 60);

/// A store holding one device and its reading taken `age` ago, as a
/// previous run left it.
async fn store_with_reading(
    test_name: &str,
    info: &DeviceInfo,
    percent: u8,
    age: Duration,
) -> (state::Store, ScratchDir) {
    let (store, scratch) = open_scratch_store(test_name);
    let age_secs = i64::try_from(age.as_secs()).expect("a test age fits in i64");
    let at = state::now_unix() - age_secs;
    store
        .record_seen(vec![(info.id(), info.kind)], at)
        .await
        .expect("record_seen");
    store.record_reading(&info.id(), reading(percent), at);
    (store, scratch)
}

fn age_of(device: &DeviceState) -> Duration {
    let seen = device.last_seen.expect("a restored reading has a time");
    crate::clock::now().saturating_duration_since(seen)
}

/// A device switched off before a restart: no sweep finds it, and it
/// still shows, dimmed, with the age of its last reading.
#[tokio::test]
async fn a_restart_restores_a_recent_reading_of_an_undiscovered_device() {
    let keyboard = device("keyboard");
    let (store, _scratch) = store_with_reading("restore-recent", &keyboard, 64, 2 * HOUR).await;

    let (tx, _config_rx) = watch::channel(Config::default());
    let (mut rx, _refresh) = Supervisor::spawn_with(
        tx,
        || found(vec![]),
        no_migration_load,
        no_migration_save,
        Some(store),
        ConfigRole::Owner,
    );
    let state = wait_for_devices(&mut rx).await;

    assert_eq!(state.devices.len(), 1, "{:?}", state.devices);
    let restored = &state.devices[0];
    assert_eq!(restored.presence, Presence::Disconnected);
    assert_eq!(restored.last_reading, Some(reading(64)));
    let age = age_of(restored);
    assert!(
        age >= 2 * HOUR && age < 2 * HOUR + Duration::from_secs(60),
        "age {age:?}"
    );
    assert!(crate::domain::is_visible(
        restored,
        |_| true,
        crate::clock::now(),
        crate::domain::OFFLINE_SHELF_LIFE_MAX,
    ));
}

#[tokio::test]
async fn a_restart_does_not_restore_a_reading_past_the_retention() {
    let keyboard = device("keyboard");
    let (store, _scratch) = store_with_reading("restore-old", &keyboard, 64, 25 * HOUR).await;
    let (ctx, _, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(Some(store));

    sweep(&mut registry, &ctx, vec![]).await;
    registry.restore_retained().await;

    assert_eq!(registry.snapshot(), []);
}

#[tokio::test]
async fn a_discovered_device_shows_its_stored_reading_until_its_first_poll() {
    let mouse = device("mouse");
    let (store, _scratch) = store_with_reading("restore-discovered", &mouse, 70, HOUR).await;
    let (ctx, _, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(Some(store));

    sweep(&mut registry, &ctx, vec![ok(&mouse, 65)]).await;

    let before = &registry.snapshot()[0];
    assert_eq!(before.presence, Presence::Unreachable);
    assert_eq!(before.last_reading, Some(reading(70)));
    assert!(age_of(before) >= HOUR);

    record_live(&mut registry, &mouse.id(), Some(reading(65)));

    let after = &registry.snapshot()[0];
    assert_eq!(after.presence, Presence::Online);
    assert_eq!(after.last_reading, Some(reading(65)));
    assert!(age_of(after) < Duration::from_secs(60));
}

/// A restored entry has no task; the sweep that finds the device gives it
/// one, and losing it again retires it like any other.
#[tokio::test]
async fn a_restored_device_is_polled_once_a_sweep_finds_it() {
    let mouse = device("mouse");
    let (store, _scratch) = store_with_reading("restore-handover", &mouse, 70, HOUR).await;
    let (ctx, _, _config) = source_ctx();
    let mut registry = DeviceRegistry::new(Some(store));
    sweep(&mut registry, &ctx, vec![]).await;
    registry.restore_retained().await;
    assert!(!registry.tasks.contains_key(&mouse.id()));

    registry
        .reconcile(vec![err_sweep("sysfs", "bus hiccup")], &ctx)
        .await;
    assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);

    sweep(&mut registry, &ctx, vec![ok(&mouse, 65)]).await;
    assert!(registry.tasks.contains_key(&mouse.id()));
    assert_eq!(registry.snapshot()[0].last_reading, Some(reading(70)));
    record_live(&mut registry, &mouse.id(), Some(reading(65)));
    assert_eq!(registry.snapshot()[0].presence, Presence::Online);

    sweep(&mut registry, &ctx, vec![]).await;
    assert!(!registry.tasks.contains_key(&mouse.id()));
    assert_eq!(registry.snapshot()[0].presence, Presence::Disconnected);
}

#[test]
fn push_history_point_dedups_and_caps_like_live_polling() {
    let base = crate::clock::now();
    let mut history = Vec::new();
    push_history_point(&mut history, base, 80);
    push_history_point(&mut history, base, 80); // duplicate, dropped
    push_history_point(&mut history, base, 79);
    assert_eq!(history, vec![(base, 80), (base, 79)]);

    // An increase clears everything recorded before it.
    push_history_point(&mut history, base, 90);
    assert_eq!(history, vec![(base, 90)]);
}

/// Readings every few seconds must not postpone the periodic sweep.
#[tokio::test(start_paused = true)]
async fn readings_do_not_postpone_the_discovery_sweep() {
    let cfg = Config {
        poll_interval_secs: 5,
        ..Config::default()
    };
    let (tx, _config_rx) = watch::channel(cfg);
    let sweeps = Arc::new(AtomicUsize::new(0));
    let (_rx, _refresh) = spawn_test(tx, {
        let sweeps = sweeps.clone();
        move || {
            sweeps.fetch_add(1, Ordering::Relaxed);
            found(vec![ok(&device("mouse"), 80)])
        }
    });

    tokio::time::sleep(DISCOVERY_INTERVAL * 3 + Duration::from_secs(1)).await;

    assert_eq!(
        sweeps.load(Ordering::Relaxed),
        4,
        "initial sweep plus one per interval"
    );
}

/// A silent device's polls and every later sweep change nothing a surface
/// shows, so none of them wakes a receiver.
#[tokio::test(start_paused = true)]
async fn unchanged_state_is_not_republished() {
    let cfg = Config {
        poll_interval_secs: 5,
        ..Config::default()
    };
    let (tx, _config_rx) = watch::channel(cfg);
    let sweeps = Arc::new(AtomicUsize::new(0));
    let (mut rx, _refresh) = spawn_test(tx, {
        let sweeps = sweeps.clone();
        move || {
            sweeps.fetch_add(1, Ordering::Relaxed);
            found(vec![err(&device("mouse"))])
        }
    });
    rx.changed().await.expect("the first sweep is published");
    rx.borrow_and_update();

    let mut notifications = 0;
    let quiet = tokio::time::sleep(DISCOVERY_INTERVAL * 3 + Duration::from_secs(1));
    tokio::pin!(quiet);
    loop {
        tokio::select! {
            r = rx.changed() => {
                r.expect("supervisor alive");
                notifications += 1;
            }
            () = &mut quiet => break,
        }
    }

    assert_eq!(sweeps.load(Ordering::Relaxed), 4);
    assert_eq!(notifications, 0);
}

struct PushingSource {
    info: DeviceInfo,
    polls: Arc<AtomicUsize>,
    pushes: watch::Receiver<Option<BatteryReading>>,
}

#[async_trait::async_trait]
impl BatterySource for PushingSource {
    fn device(&self) -> &DeviceInfo {
        &self.info
    }

    async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
        self.polls.fetch_add(1, Ordering::Relaxed);
        Ok(reading(80))
    }

    async fn pushed(&mut self) -> BatteryReading {
        loop {
            if self.pushes.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
            if let Some(r) = *self.pushes.borrow_and_update() {
                return r;
            }
        }
    }
}

/// A pushed reading lands without a poll; `rediscover` sweeps without
/// re-polling the devices already running.
#[tokio::test(start_paused = true)]
async fn a_pushed_reading_and_a_rediscover_poll_nothing() {
    let (tx, _config_rx) = watch::channel(Config::default());
    let polls = Arc::new(AtomicUsize::new(0));
    let sweeps = Arc::new(AtomicUsize::new(0));
    let (push_tx, push_rx) = watch::channel(None);
    let (polls_seen, sweeps_seen) = (polls.clone(), sweeps.clone());
    let (mut rx, refresh) = spawn_test(tx, move || {
        sweeps_seen.fetch_add(1, Ordering::Relaxed);
        let source: Box<dyn BatterySource> = Box::new(PushingSource {
            info: device("mouse"),
            polls: polls_seen.clone(),
            pushes: push_rx.clone(),
        });
        found(vec![source])
    });
    let state = wait_for_connected(&mut rx).await;
    assert_eq!(state.devices[0].last_reading.map(|r| r.percent), Some(80));
    assert_eq!(polls.load(Ordering::Relaxed), 1);

    push_tx.send_replace(Some(reading(42)));
    timeout(Duration::from_secs(1), async {
        loop {
            rx.changed().await.expect("supervisor alive");
            let percent = rx.borrow_and_update().devices[0]
                .last_reading
                .map(|r| r.percent);
            if percent == Some(42) {
                return;
            }
        }
    })
    .await
    .expect("the pushed reading was published");
    assert_eq!(polls.load(Ordering::Relaxed), 1, "a push is not a poll");

    let before = sweeps.load(Ordering::Relaxed);
    refresh.rediscover();
    settle().await;
    assert_eq!(sweeps.load(Ordering::Relaxed), before + 1);
    assert_eq!(polls.load(Ordering::Relaxed), 1, "rediscover re-polled");
}
