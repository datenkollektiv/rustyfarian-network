// ota_lifecycle.rs — host simulation of the OTA recovery contract.
//
// Models two OTA slots with bootloader semantics, an NVS store, a reporting
// channel (at-least-once, can be offline), and a consumer that follows the
// documented persistence order of `juggler::ota::{decide_offer, reconcile}`.
// Every matrix scenario injects exactly one fault: a crash (power loss) after
// one individual step, or a failed NVS write.
//
//   just test-ota → -p juggler --features ota
#![cfg(feature = "ota")]

use juggler::backoff::ExponentialBackoff;
use juggler::ota::{
    decide_offer, reconcile, Admission, AttemptRecord, BootFacts, OfferDecision, ReconcileAction,
    SlotId, SlotState, Version,
};

const V_RUNNING: Version = Version::new(1, 0, 0);
const V_OLD_FAILED: Version = Version::new(1, 5, 0);
const V_OFFERED: Version = Version::new(2, 0, 0);
const V_SECOND: Version = Version::new(2, 1, 0);
const V_MISLABEL: Version = Version::new(3, 0, 0);
const V_NEWER_FAILED: Version = Version::new(3, 5, 0);

const MAX_OFFERS: usize = 6;
const MAX_BOOTS: usize = 12;
const MAX_DOWNLOADS: usize = 3;
const RUNTIME_RETRIES: usize = 3;

/// The bootloader's otadata record of a slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Otadata {
    None,
    New,
    PendingVerify,
    Valid,
    Invalid,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Behaviour {
    Good,
    CrashEarly,
    Unhealthy,
    Mislabelled,
}

#[derive(Clone, Copy, Debug)]
struct Image {
    version: Version,
    behaviour: Behaviour,
    /// Written by a scenario offer (not a pre-existing image).
    offered: bool,
}

/// The single persisted report record.
#[derive(Clone, Copy)]
struct Report {
    attempt_id: u32,
    /// The delivery was acknowledged (persisted after the send).
    acked: bool,
}

#[derive(Default)]
struct Nvs {
    attempt: Option<AttemptRecord>,
    attempt_counter: u32,
    refused: Option<Version>,
    report: Option<Report>,
    /// Simulation bookkeeping: attempt id of every report record created.
    created_log: Vec<u32>,
}

struct Crash;

#[derive(Default)]
struct Faults {
    crash_at: Option<usize>,
    fail_write_at: Option<usize>,
    ticks: usize,
    writes: usize,
    crash_hit: bool,
    crash_in_pending_window: bool,
    write_failed: bool,
    /// The fault hit between the boot-slot switch and `boot_selected`.
    in_window: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RunEnd {
    Stable,
    Reboot,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Offer {
    Declined,
    Aborted,
    Rebooting,
}

#[derive(Clone, Copy, Debug)]
enum Start {
    Fresh,
    AfterRollback,
    /// The refused version is HIGHER than the version later offered.
    AfterRollbackNewer,
}

/// State right after the previous image first ran again after a boot-slot
/// switch (the first rollback was reconciled).
#[derive(Clone, Copy)]
struct Snapshot {
    refused: bool,
    attempt_present: bool,
    downloads: usize,
}

struct Device {
    otadata: [Otadata; 2],
    images: [Option<Image>; 2],
    boot: usize,
    running: usize,
    nvs: Nvs,
    faults: Faults,
    /// Raw deliveries (attempt ids); at least once, so duplicates are possible.
    sent: Vec<u32>,
    /// Delivery attempts that find the channel offline.
    offline_attempts: usize,
    runtime_retries: usize,
    waited_ms: u64,
    downloads: usize,
    switched_since_offer: bool,
    switched_ever: bool,
    snapshot: Option<Snapshot>,
}

fn map_state(state: Otadata) -> SlotState {
    match state {
        Otadata::Valid => SlotState::Valid,
        Otadata::New | Otadata::PendingVerify => SlotState::PendingVerify,
        Otadata::Invalid => SlotState::Invalid,
        Otadata::None => SlotState::Unknown,
    }
}

impl Device {
    fn new(start: Start, faults: Faults) -> Self {
        let running = Image {
            version: V_RUNNING,
            behaviour: Behaviour::Good,
            offered: false,
        };
        let (inactive, state, refused) = match start {
            Start::Fresh => (None, Otadata::None, None),
            Start::AfterRollback => (
                Some(Image {
                    version: V_OLD_FAILED,
                    behaviour: Behaviour::CrashEarly,
                    offered: false,
                }),
                Otadata::Invalid,
                Some(V_OLD_FAILED),
            ),
            Start::AfterRollbackNewer => (
                Some(Image {
                    version: V_NEWER_FAILED,
                    behaviour: Behaviour::CrashEarly,
                    offered: false,
                }),
                Otadata::Invalid,
                Some(V_NEWER_FAILED),
            ),
        };
        Device {
            otadata: [Otadata::Valid, state],
            images: [Some(running), inactive],
            boot: 0,
            running: 0,
            nvs: Nvs {
                refused,
                ..Nvs::default()
            },
            faults,
            sent: Vec::new(),
            offline_attempts: 0,
            runtime_retries: 0,
            waited_ms: 0,
            downloads: 0,
            switched_since_offer: false,
            switched_ever: false,
            snapshot: None,
        }
    }

    /// True between the internal boot-slot switch and a persisted
    /// `boot_selected`: the documented v1 limitation window.
    fn window_open(&self) -> bool {
        self.switched_since_offer && self.nvs.attempt.is_some_and(|a| !a.boot_selected)
    }

    /// A crash point: the step just performed is durable, then power is lost.
    fn tick(&mut self) -> Result<(), Crash> {
        let n = self.faults.ticks;
        self.faults.ticks += 1;
        if self.faults.crash_at == Some(n) {
            self.faults.crash_hit = true;
            self.faults.in_window = self.window_open();
            self.faults.crash_in_pending_window = self.otadata[self.running]
                == Otadata::PendingVerify
                && self.images[self.running].is_some_and(|i| i.offered);
            return Err(Crash);
        }
        Ok(())
    }

    /// One NVS write. `Ok(false)` is a failed write (nothing changed).
    fn write(&mut self, apply: impl FnOnce(&mut Nvs)) -> Result<bool, Crash> {
        let n = self.faults.writes;
        self.faults.writes += 1;
        if self.faults.fail_write_at == Some(n) {
            self.faults.write_failed = true;
            self.faults.in_window = self.window_open();
            return Ok(false);
        }
        apply(&mut self.nvs);
        self.tick()?;
        Ok(true)
    }

    fn running_image(&self) -> Image {
        self.images[self.running].expect("running slot holds an image")
    }

    fn undelivered(&self) -> bool {
        self.nvs.report.is_some_and(|r| !r.acked)
    }

    /// Bootloader: pick a slot and run its image until it stops or reboots.
    fn boot_and_run(&mut self) -> Result<RunEnd, Crash> {
        loop {
            let slot = self.boot;
            match self.otadata[slot] {
                Otadata::New => {
                    self.otadata[slot] = Otadata::PendingVerify;
                    self.running = slot;
                    self.tick()?;
                    return self.app_main();
                }
                Otadata::Valid => {
                    self.running = slot;
                    return self.app_main();
                }
                // Never marked valid (PendingVerify), invalid, or unknown.
                _ => {
                    self.otadata[slot] = Otadata::Invalid;
                    self.boot = 1 - slot;
                    self.tick()?;
                }
            }
        }
    }

    fn facts(&self) -> BootFacts {
        let other = 1 - self.running;
        BootFacts {
            running_slot: SlotId(self.running as u8),
            running_state: map_state(self.otadata[self.running]),
            running_version: Some(self.running_image().version),
            update_slot: Some((SlotId(other as u8), map_state(self.otadata[other]))),
            report_persisted: self.nvs.attempt.is_some_and(|a| {
                self.nvs
                    .report
                    .is_some_and(|r| r.attempt_id == a.attempt_id)
            }),
        }
    }

    fn app_rollback(&mut self) -> Result<RunEnd, Crash> {
        self.otadata[self.running] = Otadata::Invalid;
        self.boot = 1 - self.running;
        self.tick()?;
        Ok(RunEnd::Reboot)
    }

    fn update_attempt(&mut self, change: impl FnOnce(&mut AttemptRecord)) -> Result<bool, Crash> {
        let Some(mut record) = self.nvs.attempt else {
            return Ok(false);
        };
        change(&mut record);
        self.write(move |n| n.attempt = Some(record))
    }

    /// The consumer's boot script, following the documented persistence order.
    fn app_main(&mut self) -> Result<RunEnd, Crash> {
        let image = self.running_image();
        if image.behaviour == Behaviour::CrashEarly && image.offered {
            // Crashes before any app bookkeeping.
            return Ok(RunEnd::Reboot);
        }
        let facts = self.facts();
        match reconcile(self.nvs.attempt.as_ref(), &facts) {
            ReconcileAction::NoAttempt | ReconcileAction::Defer => {}
            action @ (ReconcileAction::ClearAttempt
            | ReconcileAction::CompleteAttempt
            | ReconcileAction::ReportRollback { .. }) => {
                self.rerun(action)?;
            }
            ReconcileAction::AwaitHealthCheck { mark_activated } => {
                if mark_activated {
                    // A failed write is logged; the health check still runs.
                    self.update_attempt(|a| a.activated = true)?;
                }
                if image.behaviour == Behaviour::Unhealthy {
                    return self.app_rollback();
                }
                self.otadata[self.running] = Otadata::Valid;
                self.tick()?;
                self.complete_attempt()?;
            }
            ReconcileAction::RefuseImage { mark_activated, .. } => {
                if mark_activated {
                    self.update_attempt(|a| a.activated = true)?;
                }
                return self.app_rollback();
            }
        }
        self.finish()
    }

    /// Apply an action that is safe to re-run with refreshed facts.
    /// Returns false for actions that are not (the caller must stop).
    fn rerun(&mut self, action: ReconcileAction) -> Result<bool, Crash> {
        match action {
            ReconcileAction::NoAttempt | ReconcileAction::Defer => Ok(true),
            ReconcileAction::ClearAttempt => {
                self.write(|n| n.attempt = None)?;
                Ok(true)
            }
            ReconcileAction::CompleteAttempt => {
                self.complete_attempt()?;
                Ok(true)
            }
            ReconcileAction::ReportRollback {
                attempt_id,
                left,
                report_already_persisted,
            } => {
                self.report_rollback(attempt_id, left, report_already_persisted)?;
                Ok(true)
            }
            ReconcileAction::AwaitHealthCheck { .. } | ReconcileAction::RefuseImage { .. } => {
                Ok(false)
            }
        }
    }

    /// Completion cleanup: clear a refusal that differs from the running
    /// version, then the attempt; keep the attempt if the refusal clear fails.
    fn complete_attempt(&mut self) -> Result<(), Crash> {
        let running_version = self.running_image().version;
        if self.nvs.refused.is_some_and(|r| r != running_version)
            && !self.write(|n| n.refused = None)?
        {
            return Ok(());
        }
        self.write(|n| n.attempt = None)?;
        Ok(())
    }

    /// Report, refuse, then clear; keep the attempt if any write fails.
    fn report_rollback(
        &mut self,
        attempt_id: u32,
        left: Option<Version>,
        already_persisted: bool,
    ) -> Result<(), Crash> {
        if !already_persisted
            && !self.write(move |n| {
                n.report = Some(Report {
                    attempt_id,
                    acked: false,
                });
                n.created_log.push(attempt_id);
            })?
        {
            return Ok(());
        }
        if let Some(left) = left {
            if !self.write(move |n| n.refused = Some(left))? {
                return Ok(());
            }
        }
        self.write(|n| n.attempt = None)?;
        Ok(())
    }

    /// Send the undelivered report, then persist the acknowledgement.
    /// Delivery is at least once: a fault between the two steps resends.
    fn deliver(&mut self) -> Result<(), Crash> {
        let Some(report) = self.nvs.report.filter(|r| !r.acked) else {
            return Ok(());
        };
        if self.offline_attempts > 0 {
            self.offline_attempts -= 1;
            return Ok(());
        }
        self.sent.push(report.attempt_id);
        self.tick()?;
        self.write(|n| {
            if let Some(r) = n.report.as_mut() {
                r.acked = true;
            }
        })?;
        Ok(())
    }

    /// Deliver, then (if enabled) retry unresolved work at runtime with
    /// refreshed facts and a bounded exponential backoff.
    fn finish(&mut self) -> Result<RunEnd, Crash> {
        self.deliver()?;
        let mut backoff = ExponentialBackoff::new(100, 1_000);
        for _ in 0..self.runtime_retries {
            if self.nvs.attempt.is_none() && !self.undelivered() {
                break;
            }
            self.waited_ms += backoff.next().unwrap_or(0);
            let action = reconcile(self.nvs.attempt.as_ref(), &self.facts());
            if !self.rerun(action)? {
                break;
            }
            self.deliver()?;
        }
        if self.snapshot.is_none() && self.switched_ever && !self.running_image().offered {
            self.snapshot = Some(Snapshot {
                refused: self.nvs.refused == Some(V_OFFERED),
                attempt_present: self.nvs.attempt.is_some(),
                downloads: self.downloads,
            });
        }
        Ok(RunEnd::Stable)
    }

    /// The consumer's offer handler (a retained command may redeliver it).
    fn offer(&mut self, version: Version, behaviour: Behaviour) -> Result<Offer, Crash> {
        let running = self.running_image().version;
        let admission = Admission::from_records(self.nvs.attempt.is_some(), self.undelivered());
        if decide_offer(running, version, self.nvs.refused, admission) != OfferDecision::Apply {
            return Ok(Offer::Declined);
        }
        let target = 1 - self.running;
        let record = AttemptRecord {
            attempt_id: self.nvs.attempt_counter + 1,
            version: Some(version),
            slot: SlotId(target as u8),
            boot_selected: false,
            activated: false,
            slot_was_invalid: self.otadata[target] == Otadata::Invalid,
        };
        if !self.write(move |n| {
            n.attempt = Some(record);
            n.attempt_counter = record.attempt_id;
        })? {
            return Ok(Offer::Aborted);
        }

        // fetch_and_apply: write the image, then switch the boot slot.
        self.downloads += 1;
        self.switched_since_offer = false;
        let reported = if behaviour == Behaviour::Mislabelled {
            V_MISLABEL
        } else {
            version
        };
        self.images[target] = Some(Image {
            version: reported,
            behaviour,
            offered: true,
        });
        self.tick()?;
        self.otadata[target] = Otadata::New;
        self.boot = target;
        self.switched_since_offer = true;
        self.switched_ever = true;
        self.tick()?;

        // Only after fetch_and_apply returned Ok; a failed write is logged.
        self.update_attempt(|a| a.boot_selected = true)?;
        Ok(Offer::Rebooting)
    }

    /// Boot (again) until the device runs stably.
    fn settle(&mut self) {
        for _ in 0..MAX_BOOTS {
            if let Ok(RunEnd::Stable) = self.boot_and_run() {
                return;
            }
        }
        panic!("device never became stable");
    }

    /// Offer until declined, rebooting as needed. False if it never converged.
    fn offer_until_declined(&mut self, version: Version, behaviour: Behaviour) -> bool {
        for _ in 0..MAX_OFFERS {
            match self.offer(version, behaviour) {
                Ok(Offer::Declined) => return true,
                Ok(Offer::Aborted | Offer::Rebooting) | Err(Crash) => self.settle(),
            }
        }
        false
    }
}

struct Outcome {
    crash_hit: bool,
    write_failed: bool,
    ticks: usize,
    writes: usize,
    in_window: bool,
    window_limitation_hit: bool,
    duplicate_delivery: bool,
}

fn is_failed(behaviour: Behaviour) -> bool {
    behaviour != Behaviour::Good
}

/// Run one scenario to quiescence and assert the invariants.
fn run_scenario(
    start: Start,
    behaviour: Behaviour,
    runtime_retries: usize,
    crash_at: Option<usize>,
    fail_write_at: Option<usize>,
) -> Outcome {
    let label = format!(
        "start={start:?} behaviour={behaviour:?} retries={runtime_retries} \
         crash_at={crash_at:?} fail_write_at={fail_write_at:?}"
    );
    let mut dev = Device::new(
        start,
        Faults {
            crash_at,
            fail_write_at,
            ..Faults::default()
        },
    );
    dev.runtime_retries = runtime_retries;

    assert!(
        dev.offer_until_declined(V_OFFERED, behaviour),
        "{label}: offer loop did not converge"
    );
    assert!(
        dev.downloads <= MAX_DOWNLOADS,
        "{label}: {} downloads",
        dev.downloads
    );

    // Runtime retry: a transient write failure resolves without a reboot.
    if runtime_retries > 0 && !dev.faults.crash_hit {
        assert!(
            dev.nvs.attempt.is_none() && !dev.undelivered(),
            "{label}: admission still blocked without a reboot"
        );
    }

    // A final power cycle lets a retained attempt or report resolve.
    dev.settle();
    let downloads_before = dev.downloads;
    let reoffer = dev.offer(V_OFFERED, behaviour);
    if is_failed(behaviour) {
        assert!(
            matches!(reoffer, Ok(Offer::Declined)),
            "{label}: I4 re-offer not declined"
        );
        assert_eq!(
            dev.downloads, downloads_before,
            "{label}: I4 re-offer started a download"
        );
    }

    let window_limitation_hit = check_invariants(&dev, start, behaviour, &label);
    let mut sorted = dev.sent.clone();
    sorted.sort_unstable();
    sorted.dedup();
    Outcome {
        crash_hit: dev.faults.crash_hit,
        write_failed: dev.faults.write_failed,
        ticks: dev.faults.ticks,
        writes: dev.faults.writes,
        in_window: dev.faults.in_window,
        window_limitation_hit,
        duplicate_delivery: sorted.len() != dev.sent.len(),
    }
}

/// Returns true if the known v1 crash window lost the evidence.
fn check_invariants(dev: &Device, start: Start, behaviour: Behaviour, label: &str) -> bool {
    let write_failed = dev.faults.write_failed;
    let attempt_present = dev.nvs.attempt.is_some();
    let refused = dev.nvs.refused == Some(V_OFFERED);
    let newer_refused = dev.nvs.refused == Some(V_NEWER_FAILED);

    // I3: one report record per attempt id; at-least-once delivery, but
    // after dedup by attempt id exactly one per rolled-back attempt.
    let mut created = dev.nvs.created_log.clone();
    created.sort_unstable();
    let distinct = created.len();
    created.dedup();
    assert_eq!(
        distinct,
        created.len(),
        "{label}: I3 two reports created for one attempt"
    );
    let mut sent = dev.sent.clone();
    sent.sort_unstable();
    sent.dedup();
    assert_eq!(
        sent, created,
        "{label}: I3 deliveries after dedup differ from created reports"
    );
    if refused && !attempt_present {
        assert!(!created.is_empty(), "{label}: I3 refusal without a report");
    }

    // Strict check right after the first rollback was reconciled.
    let mut limitation_hit = false;
    if let Some(snap) = dev.snapshot {
        let evidenced = snap.refused || snap.attempt_present;
        if dev.faults.in_window {
            if !evidenced {
                // Known v1 window: exactly one extra download converges.
                limitation_hit = true;
                assert_eq!(
                    dev.downloads,
                    snap.downloads + 1,
                    "{label}: window did not converge after one extra download"
                );
                assert!(refused, "{label}: window did not end refused");
            }
        } else {
            assert!(
                evidenced,
                "{label}: rollback not evidenced right after reconcile"
            );
        }
    }

    if is_failed(behaviour) {
        // I1: a failed version is never final.
        for (slot, image) in dev.images.iter().enumerate() {
            if image.is_some_and(|i| i.offered) {
                assert_ne!(
                    dev.otadata[slot],
                    Otadata::Valid,
                    "{label}: I1 failed image is Valid"
                );
            }
        }
        assert!(
            !dev.running_image().offered,
            "{label}: I1 failed image is running"
        );
        assert!(
            refused || attempt_present,
            "{label}: I1 evidence dropped (not refused, no attempt record)"
        );
        if !write_failed {
            assert!(refused, "{label}: I1 not refused without write failures");
        }
    } else {
        // I2: a good version is never refused, barring a power loss while
        // it was still unverified (the bootloader then rolls it back).
        if !dev.faults.crash_in_pending_window {
            assert!(!refused, "{label}: I2 good version refused");
            // I5: a refused version that differs from the verified running
            // one eventually clears, even if cleanup was interrupted.
            if matches!(start, Start::AfterRollbackNewer) {
                assert!(
                    !newer_refused,
                    "{label}: I5 stale higher refusal survived a successful update"
                );
            }
            if !write_failed {
                assert!(
                    dev.running_image().offered && dev.otadata[dev.running] == Otadata::Valid,
                    "{label}: I2 good version not Valid and running"
                );
            }
        }
    }
    limitation_hit
}

const BEHAVIOURS: [Behaviour; 4] = [
    Behaviour::Good,
    Behaviour::CrashEarly,
    Behaviour::Unhealthy,
    Behaviour::Mislabelled,
];
const STARTS: [Start; 3] = [
    Start::Fresh,
    Start::AfterRollback,
    Start::AfterRollbackNewer,
];

#[derive(Default, Debug)]
struct Counts {
    crashes: usize,
    write_failures: usize,
    in_window: usize,
    window_limitation_hits: usize,
    duplicate_deliveries: usize,
}

fn run_matrix(runtime_retries: usize) -> Counts {
    let mut counts = Counts::default();
    let mut tally = |o: &Outcome| {
        counts.crashes += usize::from(o.crash_hit);
        counts.write_failures += usize::from(o.write_failed);
        counts.in_window += usize::from(o.in_window);
        counts.window_limitation_hits += usize::from(o.window_limitation_hit);
        counts.duplicate_deliveries += usize::from(o.duplicate_delivery);
    };
    for start in STARTS {
        for behaviour in BEHAVIOURS {
            let dry = run_scenario(start, behaviour, runtime_retries, None, None);
            assert!(dry.ticks > 0 && dry.writes > 0);
            for n in 0..dry.ticks + 8 {
                tally(&run_scenario(
                    start,
                    behaviour,
                    runtime_retries,
                    Some(n),
                    None,
                ));
            }
            for n in 0..dry.writes + 4 {
                tally(&run_scenario(
                    start,
                    behaviour,
                    runtime_retries,
                    None,
                    Some(n),
                ));
            }
        }
    }
    counts
}

#[test]
fn lifecycle_holds_under_crash_and_write_failure_injection() {
    let c = run_matrix(0);
    println!("ota_lifecycle (reboot-only recovery): {c:?}");
    assert!(c.crashes >= 80, "only {} crash scenarios", c.crashes);
    assert!(c.write_failures >= 30, "only {} failures", c.write_failures);
    assert!(c.in_window > 0, "no scenario hit the boot-slot window");
    assert!(
        c.window_limitation_hits > 0,
        "the documented window limitation was never exercised"
    );
    assert!(
        c.duplicate_deliveries > 0,
        "no scenario produced an at-least-once duplicate delivery"
    );
}

#[test]
fn runtime_retry_resolves_transient_failures_without_reboot() {
    let c = run_matrix(RUNTIME_RETRIES);
    println!("ota_lifecycle (runtime retry): {c:?}");
    assert!(c.write_failures >= 30, "only {} failures", c.write_failures);
}

#[test]
fn fault_free_runs_end_as_expected() {
    for start in STARTS {
        for behaviour in BEHAVIOURS {
            run_scenario(start, behaviour, 0, None, None);
        }
    }
}

#[test]
fn second_failed_update_is_refused_admission_while_report_is_offline() {
    for first in [
        Behaviour::CrashEarly,
        Behaviour::Unhealthy,
        Behaviour::Mislabelled,
    ] {
        let mut dev = Device::new(Start::Fresh, Faults::default());
        dev.offline_attempts = 2;

        assert!(dev.offer_until_declined(V_OFFERED, first));
        assert!(dev.undelivered(), "{first:?}: report should be pending");
        assert_eq!(dev.nvs.refused, Some(V_OFFERED));
        assert!(dev.nvs.attempt.is_none());
        let first_id = dev.nvs.report.expect("report").attempt_id;

        // A second, different version is not admitted while the report is
        // undelivered, and the pending report is not overwritten.
        let downloads = dev.downloads;
        assert!(matches!(
            dev.offer(V_SECOND, Behaviour::Unhealthy),
            Ok(Offer::Declined)
        ));
        assert_eq!(dev.downloads, downloads);
        assert_eq!(dev.nvs.report.expect("report").attempt_id, first_id);
        assert!(dev.sent.is_empty());

        // The channel comes back: the first report is delivered, then the
        // second update proceeds, fails, and is reported too.
        while dev.undelivered() {
            dev.settle();
        }
        assert_eq!(dev.sent, vec![first_id]);
        assert!(dev.offer_until_declined(V_SECOND, Behaviour::Unhealthy));
        dev.settle();

        assert_eq!(dev.downloads, downloads + 1);
        assert_eq!(dev.nvs.refused, Some(V_SECOND));
        assert!(!dev.undelivered());
        let mut sent = dev.sent.clone();
        sent.dedup();
        assert_eq!(sent, vec![first_id, first_id + 1], "{first:?}");
        assert_eq!(dev.nvs.created_log, vec![first_id, first_id + 1]);
    }
}

#[test]
fn attempt_ids_strictly_increase() {
    let mut dev = Device::new(Start::Fresh, Faults::default());
    assert!(dev.offer_until_declined(V_OFFERED, Behaviour::Unhealthy));
    dev.settle();
    assert!(dev.offer_until_declined(V_SECOND, Behaviour::Unhealthy));
    dev.settle();
    assert_eq!(dev.nvs.created_log, vec![1, 2]);
}

/// Every cleanup step after `mark_valid` is interrupted in turn: the
/// refusal (higher than the installed version) clears and the attempt is
/// deleted, with and without runtime retries.
#[test]
fn interrupted_completion_cleanup_clears_refusal_and_attempt() {
    for retries in [0, RUNTIME_RETRIES] {
        let mut dry = Device::new(Start::AfterRollbackNewer, Faults::default());
        dry.runtime_retries = retries;
        assert!(dry.offer_until_declined(V_OFFERED, Behaviour::Good));
        dry.settle();
        assert_eq!(dry.nvs.refused, None);
        assert!(dry.nvs.attempt.is_none());
        let ticks = dry.faults.ticks;
        let writes = dry.faults.writes;

        let mut checked = 0;
        for crash_at in (0..ticks).map(Some).chain([None]) {
            for fail_write_at in (0..writes).map(Some).chain([None]) {
                if crash_at.is_some() && fail_write_at.is_some() {
                    continue;
                }
                let mut dev = Device::new(
                    Start::AfterRollbackNewer,
                    Faults {
                        crash_at,
                        fail_write_at,
                        ..Faults::default()
                    },
                );
                dev.runtime_retries = retries;
                let label = format!(
                    "retries={retries} crash_at={crash_at:?} fail_write_at={fail_write_at:?}"
                );
                assert!(
                    dev.offer_until_declined(V_OFFERED, Behaviour::Good),
                    "{label}"
                );
                dev.settle();
                dev.settle();
                if dev.faults.crash_in_pending_window {
                    continue;
                }
                assert_ne!(dev.nvs.refused, Some(V_NEWER_FAILED), "{label}: refusal");
                assert!(dev.nvs.attempt.is_none(), "{label}: attempt kept");
                assert_eq!(
                    dev.running_image().version,
                    V_OFFERED,
                    "{label}: not running the update"
                );
                checked += 1;
            }
        }
        assert!(checked >= 10, "only {checked} cleanup scenarios");
    }
}
