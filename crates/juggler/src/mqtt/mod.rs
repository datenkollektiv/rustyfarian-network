//! Pure MQTT primitives — no I/O, no ESP-IDF.

/// Returns the number of 100 ms poll iterations needed to cover `timeout_ms`.
///
/// Uses ceiling division so that a timeout that is not an exact multiple of
/// 100 ms is always fully respected — e.g. 5050 ms yields 51 iterations
/// (5100 ms of polling) rather than 50 (5000 ms).
pub fn connection_wait_iterations(timeout_ms: u64) -> u64 {
    timeout_ms.div_ceil(100)
}

// ── Broker URL ───────────────────────────────────────────────────────────────

/// Formats the `mqtt://` URL used to connect to the broker.
///
/// This is the single place where the URL scheme is chosen; a future TLS
/// variant would change the prefix here.
///
/// Requires the `std` feature (returns an owned [`String`]).
#[cfg(feature = "std")]
pub fn format_broker_url(host: &str, port: u16) -> String {
    format!("mqtt://{}:{}", host, port)
}

// ── Validation ───────────────────────────────────────────────────────────────

/// Maximum client ID length for maximum MQTT 3.1.1 broker compatibility.
///
/// Section 3.1.3.1 of the MQTT 3.1.1 specification caps client IDs at
/// 23 bytes for brokers that must support all conformant clients.
pub const CLIENT_ID_MAX_LEN: usize = 23;

/// Returns `Ok(())` if `client_id` is a valid MQTT client identifier.
///
/// Rejects empty strings and strings longer than [`CLIENT_ID_MAX_LEN`] bytes.
pub fn validate_client_id(client_id: &str) -> Result<(), &'static str> {
    if client_id.is_empty() {
        return Err("MQTT client ID must not be empty");
    }
    if client_id.len() > CLIENT_ID_MAX_LEN {
        return Err("MQTT client ID exceeds the 23-byte MQTT 3.1.1 maximum");
    }
    Ok(())
}

/// Maximum UTF-8 byte length of an MQTT topic string (MQTT 3.1.1 §4.7).
pub const TOPIC_MAX_LEN: usize = 65535;

/// Returns `Ok(())` if `topic` is a valid MQTT topic string.
///
/// Rejects empty strings, strings longer than [`TOPIC_MAX_LEN`] UTF-8 bytes
/// (the MQTT maximum), and strings containing the NUL character (`\0`).
pub fn validate_topic(topic: &str) -> Result<(), &'static str> {
    if topic.is_empty() {
        return Err("MQTT topic must not be empty");
    }
    if topic.len() > TOPIC_MAX_LEN {
        return Err("MQTT topic exceeds the 65535-byte maximum");
    }
    if topic.contains('\0') {
        return Err("MQTT topic must not contain the NUL character");
    }
    Ok(())
}

/// Returns `Ok(())` if `topic` is valid as a publish topic.
///
/// Calls [`validate_topic`] for base checks, then additionally rejects topics
/// containing `+` or `#` (wildcards are forbidden in publish topics per
/// MQTT 3.1.1 §3.3.2).
pub fn validate_publish_topic(topic: &str) -> Result<(), &'static str> {
    validate_topic(topic)?;
    if topic.contains('+') {
        return Err("MQTT publish topic must not contain the '+' wildcard");
    }
    if topic.contains('#') {
        return Err("MQTT publish topic must not contain the '#' wildcard");
    }
    Ok(())
}

/// Returns `Ok(())` if `filter` is a valid MQTT subscription filter.
///
/// Calls [`validate_topic`] for base checks, then validates MQTT 3.1.1 §4.7
/// positional rules for wildcards:
/// - `+` must occupy an entire level (preceded by `/` or at start, followed by
///   `/` or at end).
/// - `#` must be the last character and either be the sole character or
///   immediately preceded by `/`.
pub fn validate_subscribe_filter(filter: &str) -> Result<(), &'static str> {
    validate_topic(filter)?;

    let bytes = filter.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'+' {
            let before_ok = i == 0 || bytes[i - 1] == b'/';
            let after_ok = i + 1 == bytes.len() || bytes[i + 1] == b'/';
            if !before_ok || !after_ok {
                return Err("MQTT subscribe filter '+' must occupy an entire level");
            }
        } else if b == b'#' {
            let is_last = i + 1 == bytes.len();
            let before_ok = i == 0 || bytes[i - 1] == b'/';
            if !is_last || !before_ok {
                return Err(
                    "MQTT subscribe filter '#' must be the last character and follow '/' or be sole",
                );
            }
        }
    }
    Ok(())
}

/// Returns `true` if `topic` matches the subscription `filter`.
///
/// Implements MQTT 3.1.1 §4.7 topic matching rules:
/// - `+` in the filter matches exactly one level in the topic.
/// - `#` in the filter matches zero or more remaining levels.
/// - Topics beginning with `$` do not match filters beginning with `+` or `#`
///   (MQTT §4.7.2).
pub fn topic_matches_filter(topic: &str, filter: &str) -> bool {
    // MQTT §4.7.2: $-prefixed topics don't match leading + or #
    if topic.starts_with('$')
        && (filter == "#" || filter.starts_with('+') || filter.starts_with("/#"))
    {
        return false;
    }

    let mut topic_levels = topic.split('/');
    let mut filter_levels = filter.split('/');

    loop {
        match (topic_levels.next(), filter_levels.next()) {
            // Filter exhausted: topic must also be exhausted for a match
            (None, None) => return true,
            (Some(_), None) => return false,
            (None, Some(f)) => {
                // # matches zero remaining levels
                return f == "#";
            }
            (Some(_), Some("#")) => return true,
            (Some(_), Some("+")) => {
                // + matches exactly one level — continue
            }
            (Some(t), Some(f)) => {
                if t != f {
                    return false;
                }
            }
        }
    }
}

/// Resolves the MQTT client ID to use for a device, applying the standard
/// 23-byte cap and empty-fallback policy.
///
/// # Resolution order
///
/// 1. If `operator_id` is `Some(id)` and non-empty, it is validated against
///    [`validate_client_id`] and returned as-is. An over-length or otherwise
///    invalid operator ID is rejected with an error — the operator supplied it
///    and truncating it silently would change its semantics.
/// 2. If `operator_id` is `None` or empty, a derived ID is taken from
///    `device_name` truncated to [`CLIENT_ID_MAX_LEN`] bytes on a UTF-8 char
///    boundary (so a naive byte-slice never splits a multi-byte codepoint).
/// 3. If the derived slice is empty (i.e. `device_name` itself is empty),
///    `fallback` is used instead.
/// 4. The chosen derived/fallback slice is then validated via
///    [`validate_client_id`] and returned.
///
/// Returns a `&str` borrowed from whichever of the three inputs was selected —
/// no allocation, no `String`.
///
/// # Errors
///
/// Returns `Err(&'static str)` when the operator-supplied ID is invalid, or
/// when the final derived/fallback value fails [`validate_client_id`]
/// (e.g. the fallback itself is empty or over-length).
pub fn resolve_client_id<'a>(
    operator_id: Option<&'a str>,
    device_name: &'a str,
    fallback: &'a str,
) -> Result<&'a str, &'static str> {
    // ── 1. Operator-supplied override ────────────────────────────────────────
    if let Some(id) = operator_id {
        if !id.is_empty() {
            validate_client_id(id)?;
            return Ok(id);
        }
    }

    // ── 2. Derive from device_name, truncated on a UTF-8 char boundary ───────
    let mut byte_len = 0usize;
    for ch in device_name.chars() {
        let next = byte_len + ch.len_utf8();
        if next > CLIENT_ID_MAX_LEN {
            break;
        }
        byte_len = next;
    }
    let derived = &device_name[..byte_len];

    // ── 3. Fall back if derived is empty ─────────────────────────────────────
    let chosen = if derived.is_empty() {
        fallback
    } else {
        derived
    };

    // ── 4. Validate and return ───────────────────────────────────────────────
    validate_client_id(chosen)?;
    Ok(chosen)
}

/// Returns `Ok(())` if `host` is a non-empty broker hostname or IP address.
pub fn validate_broker_host(host: &str) -> Result<(), &'static str> {
    if host.is_empty() {
        return Err("MQTT broker host must not be empty");
    }
    Ok(())
}

/// Returns `Ok(())` if `port` is a valid TCP port number (1–65535).
pub fn validate_broker_port(port: u16) -> Result<(), &'static str> {
    if port == 0 {
        return Err("MQTT broker port must not be 0");
    }
    Ok(())
}

// ── Connection state machine ─────────────────────────────────────────────────

/// Observable connection states for an MQTT client session.
///
/// The state machine governs when lifecycle callbacks (`on_connect`,
/// `on_disconnect`) are invoked.
/// Invalid transitions return `None` from [`next_state`], meaning the event
/// is silently ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqttConnectionState {
    /// `build()` was called; no connection has completed yet.
    Connecting,
    /// The broker acknowledged the CONNECT packet.
    Connected,
    /// The connection was lost; the ESP-IDF layer will attempt to reconnect.
    Disconnected,
    /// Shutdown was requested; no further reconnections will be attempted.
    ShuttingDown,
}

/// Events that drive the MQTT connection state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqttEvent {
    /// The broker sent a CONNACK (maps to `EventPayload::Connected`).
    Connected,
    /// The connection was lost (maps to `EventPayload::Disconnected`).
    Disconnected,
    /// The application requested a clean shutdown.
    ShutdownRequested,
}

/// Returns the next [`MqttConnectionState`] given the current state and an
/// incoming event, or `None` if the transition is invalid.
///
/// `None` encodes important safety invariants:
/// - `Connected → Connected` returns `None`: `on_connect` is never fired
///   while already connected.
/// - `Connecting → Disconnected` returns `None`: `on_disconnect` is never
///   fired before the first successful connection.
/// - Any event from `ShuttingDown` returns `None`: no callbacks fire after
///   shutdown is initiated.
pub fn next_state(current: MqttConnectionState, event: MqttEvent) -> Option<MqttConnectionState> {
    use MqttConnectionState as S;
    use MqttEvent as E;
    match (current, event) {
        (S::Connecting, E::Connected) => Some(S::Connected),
        (S::Connecting, E::Disconnected) => None,
        (S::Connecting, E::ShutdownRequested) => Some(S::ShuttingDown),
        (S::Connected, E::Disconnected) => Some(S::Disconnected),
        (S::Connected, E::Connected) => None,
        (S::Connected, E::ShutdownRequested) => Some(S::ShuttingDown),
        (S::Disconnected, E::Connected) => Some(S::Connected),
        (S::Disconnected, E::Disconnected) => None,
        (S::Disconnected, E::ShutdownRequested) => Some(S::ShuttingDown),
        (S::ShuttingDown, _) => None,
    }
}

// ── Subscriber thread ────────────────────────────────────────────────────────

/// MQTT Quality of Service level.
///
/// Platform-neutral mirror of `esp_idf_svc::mqtt::client::QoS`.
/// Used by [`SubscribeClient`] and [`spawn_subscriber_thread`] so that
/// the subscriber thread machinery can be compiled and tested on any host.
///
/// Requires the `std` feature (part of the subscriber-thread machinery).
#[cfg(feature = "std")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QoS {
    AtMostOnce,
    AtLeastOnce,
    ExactlyOnce,
}

/// Minimal MQTT subscribe surface used by [`spawn_subscriber_thread`].
///
/// Implemented for `EspMqttClient<'static>` in `rustyfarian-esp-idf-network` (mqtt feature)
/// and for test doubles on the host.
///
/// Requires the `std` feature (part of the subscriber-thread machinery, and
/// its method returns [`anyhow::Result`], which needs `std`).
#[cfg(feature = "std")]
pub trait SubscribeClient {
    fn subscribe_topic(&mut self, topic: &str, qos: QoS) -> anyhow::Result<()>;
}

/// Spawns a single connect-time helper thread that runs an optional `prelude`
/// followed by every subscription in `topics`, all under one lock of `client`.
///
/// Runs connect-time client calls off the event-loop thread. esp-mqtt (and
/// mirrored event-loop implementations) dispatch the `Connected` event — and
/// every event after it — from the mqtt task while holding a recursive
/// `api_lock`, and the transport keeps that task parked until our event-loop
/// thread calls `next()` again. Any client call made from the event-loop
/// thread *while it is still inside the callback handling that event* — a
/// startup publish, a subscribe, anything that reaches `api_lock` — therefore
/// waits forever on a lock the parked task can never release. Moving both the
/// connect-time publish (`prelude`) and every subscription onto this thread
/// lets the event-loop thread return to `next()` immediately instead.
///
/// # Ordering
///
/// `prelude`, when present, runs to completion before any topic in `topics`
/// is subscribed — a startup/liveness publish reaches the broker's outgoing
/// queue before the SUBSCRIBE packets. `prelude` must be best-effort: it is
/// responsible for logging its own failures and must never panic, since
/// there is no caller left on this thread to observe one.
///
/// # No-op
///
/// If `prelude` is `None` and `topics` is empty, no thread is spawned.
///
/// # Stale-thread safety
///
/// On rapid reconnects, an earlier connect thread may still be running when a
/// new `Connected` event fires and spawns a fresh one.  Both threads share the
/// same `Arc<Mutex<C>>`, so they serialize behind the mutex.  A stale thread
/// may run its `prelude` — a startup publish, an `on_connect` callback, or
/// both — against the newer connection; this is a harmless duplicate (brokers
/// also accept a duplicate SUBSCRIBE per MQTT §3.8). Note that an `enqueue`
/// call on a disconnected esp-mqtt client does not error — it queues into the
/// client's outbox and is replayed on the next connection; only `subscribe`
/// returns an error in that case. When the prelude also confirms a connection
/// epoch (see [`ConnectionEpoch`]), the stale thread's confirmation fails
/// harmlessly instead of marking a dead connection as connected, and the
/// ESP-IDF adapter's prelude also checks [`ConnectionEpoch::is_current`]
/// before doing any work, so a stale helper skips the prelude entirely once a
/// newer `Connected`/`Disconnected` has landed. Either outcome is safe and the
/// fresh thread on the new connection will redo the prelude and re-subscribe
/// correctly.
///
/// Pass `stack_size = 0` to use the OS thread-stack default (suitable for host tests).
/// Pass the platform-specific constant (e.g. 8192) for embedded targets.
///
/// # Returns
///
/// `true` if either a thread was spawned, or there was nothing to do (`prelude`
/// is `None` and `topics` is empty — see [No-op](#no-op)). `false` only if
/// `std::thread::Builder::spawn` itself failed (e.g. the OS refused to create a
/// new thread); the failure is also logged via `log::warn!`. Callers that have
/// work which absolutely must run somewhere (rather than being silently
/// skipped) should react to `false` by confirming/cleaning up whatever
/// bookkeeping was expecting the thread to run.
///
/// Requires the `std` feature (uses `std::thread` and `std::sync`).
#[cfg(feature = "std")]
pub fn spawn_connect_thread<C, F>(
    client: std::sync::Arc<std::sync::Mutex<C>>,
    prelude: Option<F>,
    topics: Vec<(String, QoS)>,
    stack_size: usize,
) -> bool
where
    C: SubscribeClient + Send + 'static,
    F: FnOnce(&mut C) + Send + 'static,
{
    if prelude.is_none() && topics.is_empty() {
        return true;
    }
    let mut builder = std::thread::Builder::new();
    if stack_size > 0 {
        builder = builder.stack_size(stack_size);
    }
    match builder.spawn(move || {
        let mut guard = match client.lock() {
            Ok(g) => g,
            Err(e) => {
                log::warn!("[mqtt] connect thread: client mutex poisoned: {}", e);
                return;
            }
        };
        if let Some(prelude) = prelude {
            prelude(&mut guard);
        }
        for (topic, qos) in &topics {
            match guard.subscribe_topic(topic.as_str(), *qos) {
                Ok(()) => log::info!("[mqtt] subscribed to '{}'", topic),
                Err(e) => log::warn!("[mqtt] subscribe to '{}' failed: {:#}", topic, e),
            }
        }
    }) {
        Ok(_) => true,
        Err(e) => {
            log::warn!("[mqtt] failed to spawn connect thread: {:#}", e);
            false
        }
    }
}

/// Spawns a short-lived thread that subscribes `client` to each topic in `topics`.
///
/// The thread runs independently so that a blocking `subscribe_topic` implementation
/// (e.g. `EspMqttClient::subscribe` on esp-idf-svc, which blocks on esp-mqtt's
/// `api_lock` while an event is being delivered) does not prevent the caller
/// (event loop thread) from processing further events.
///
/// Thin wrapper over [`spawn_connect_thread`] with no prelude — see its docs for
/// the full rationale (including stale-thread safety on rapid reconnects).
///
/// Pass `stack_size = 0` to use the OS thread-stack default (suitable for host tests).
/// Pass the platform-specific constant (e.g. 8192) for embedded targets.
///
/// Unlike [`spawn_connect_thread`], this always returns `()`: a subscribe-only
/// caller has no prelude bookkeeping to confirm or clean up, so a spawn
/// failure (logged internally as a warning) is not actionable here — the next
/// reconnect will simply retry.
///
/// Requires the `std` feature (uses `std::thread` and `std::sync`).
#[cfg(feature = "std")]
pub fn spawn_subscriber_thread<C>(
    client: std::sync::Arc<std::sync::Mutex<C>>,
    topics: Vec<(String, QoS)>,
    stack_size: usize,
) where
    C: SubscribeClient + Send + 'static,
{
    let _spawned = spawn_connect_thread(client, None::<fn(&mut C)>, topics, stack_size);
}

// ── Acknowledged-publish correlation ───────────────────────────────────────

/// Message identifier assigned by the MQTT client to an outgoing publish.
///
/// This is the same `u32` id space that `esp-idf-svc`'s `EspMqttClient::enqueue`
/// returns and that `EventPayload::Published` echoes when the broker's PUBACK
/// arrives — the two carry the identical `msg_id`, which is what makes the
/// correlation in [`PendingAcks`] sound. Kept as a plain alias so the pure tier
/// stays free of any ESP-IDF type.
///
/// Requires the `std` feature.
#[cfg(feature = "std")]
pub type MessageId = u32;

/// Terminal outcome of an in-flight acknowledged (QoS 1) publish.
///
/// A timeout is *not* an outcome here — it is the absence of one, reported as
/// `None` from [`AckWaiter::wait`].
///
/// Requires the `std` feature.
#[cfg(feature = "std")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckOutcome {
    /// The broker acknowledged the publish (PUBACK received).
    Acked,
    /// The MQTT session dropped before the acknowledgment arrived.
    Disconnected,
}

/// Per-message rendezvous: the outcome slot plus the condvar the publisher parks on.
#[cfg(feature = "std")]
type AckSlot = std::sync::Arc<(std::sync::Mutex<Option<AckOutcome>>, std::sync::Condvar)>;

#[cfg(feature = "std")]
#[derive(Default)]
struct PendingInner {
    /// Publishers currently blocked waiting for their PUBACK.
    waiters: std::collections::HashMap<MessageId, AckSlot>,
    /// Outcomes that arrived before their waiter registered (see
    /// [`PendingAcks::resolve`]). Drained by [`PendingAcks::register`]; bounded
    /// by the number of un-awaited in-flight ids.
    early: std::collections::HashMap<MessageId, AckOutcome>,
}

/// Registry correlating in-flight QoS 1 publishes to their PUBACK.
///
/// A publisher [`register`](PendingAcks::register)s the [`MessageId`] its enqueue
/// call returned and receives an [`AckWaiter`] to block on; the event-loop thread
/// calls [`resolve`](PendingAcks::resolve) when a `Published` event arrives, or
/// [`fail_all`](PendingAcks::fail_all) when the session drops. The two threads
/// meet on a per-message condvar, mirroring the `AckStatus` pattern `espnow` uses
/// for ESP-NOW send confirmation.
///
/// Cloning shares the same underlying map (an `Arc` handle), so the publish
/// handle and the event loop observe the same registry. This type is pure: it
/// performs no I/O and knows nothing about ESP-IDF, so it is fully host-testable.
///
/// Requires the `std` feature.
#[cfg(feature = "std")]
#[derive(Clone, Default)]
pub struct PendingAcks {
    inner: std::sync::Arc<std::sync::Mutex<PendingInner>>,
}

#[cfg(feature = "std")]
impl PendingAcks {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers interest in the PUBACK for `id` and returns a waiter to block on.
    ///
    /// If the outcome already arrived — a `Published` or disconnect that raced
    /// ahead of this call (see [`resolve`](Self::resolve)) — the returned waiter
    /// is pre-resolved and [`wait`](AckWaiter::wait) returns immediately.
    pub fn register(&self, id: MessageId) -> AckWaiter {
        let mut inner = self.lock();
        let slot: AckSlot = if let Some(outcome) = inner.early.remove(&id) {
            std::sync::Arc::new((
                std::sync::Mutex::new(Some(outcome)),
                std::sync::Condvar::new(),
            ))
        } else {
            let slot: AckSlot =
                std::sync::Arc::new((std::sync::Mutex::new(None), std::sync::Condvar::new()));
            inner.waiters.insert(id, std::sync::Arc::clone(&slot));
            slot
        };
        drop(inner);
        AckWaiter {
            id,
            slot,
            registry: self.clone(),
        }
    }

    /// Records the terminal `outcome` for `id`, waking any waiter.
    ///
    /// If no waiter has registered yet, the outcome is buffered so the next
    /// [`register`](Self::register) for `id` resolves immediately. This closes
    /// the window between a publisher's enqueue returning an id and its
    /// `register` — a window that cannot occur in practice, since a PUBACK is a
    /// network round-trip away while the `register` is the next instruction, but
    /// the buffer makes the ordering correct by construction and testable.
    pub fn resolve(&self, id: MessageId, outcome: AckOutcome) {
        let mut inner = self.lock();
        if let Some(slot) = inner.waiters.remove(&id) {
            drop(inner);
            Self::fill(&slot, outcome);
        } else {
            inner.early.insert(id, outcome);
        }
    }

    /// Fails every outstanding waiter with `outcome` (used on `Disconnected`).
    ///
    /// Also clears the early-outcome buffer, whose entries are stale once the
    /// session has dropped.
    pub fn fail_all(&self, outcome: AckOutcome) {
        let mut inner = self.lock();
        inner.early.clear();
        let slots: Vec<AckSlot> = inner.waiters.drain().map(|(_, slot)| slot).collect();
        drop(inner);
        for slot in slots {
            Self::fill(&slot, outcome);
        }
    }

    /// Removes a waiter without resolving it (cleanup on timeout / drop).
    fn cancel(&self, id: MessageId) {
        let mut inner = self.lock();
        inner.waiters.remove(&id);
        inner.early.remove(&id);
    }

    /// Stores `outcome` in a slot and wakes the parked publisher.
    fn fill(slot: &AckSlot, outcome: AckOutcome) {
        let (lock, cvar) = &**slot;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
        cvar.notify_all();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PendingInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Number of waiters currently registered (introspection for tests).
    #[cfg(test)]
    fn waiter_count(&self) -> usize {
        self.lock().waiters.len()
    }
}

/// A handle a publisher blocks on until its PUBACK arrives or a timeout elapses.
///
/// Returned by [`PendingAcks::register`]. Completing [`wait`](Self::wait) removes
/// the corresponding registry entry, so a timed-out publish never leaks a slot.
///
/// Requires the `std` feature.
#[cfg(feature = "std")]
pub struct AckWaiter {
    id: MessageId,
    slot: AckSlot,
    registry: PendingAcks,
}

#[cfg(feature = "std")]
impl AckWaiter {
    /// Blocks until the outcome is known or `timeout` elapses.
    ///
    /// Returns `Some(AckOutcome)` when the broker acknowledged
    /// ([`Acked`](AckOutcome::Acked)) or the session dropped
    /// ([`Disconnected`](AckOutcome::Disconnected)), or `None` on timeout. In
    /// every case the registry entry for this message is removed before
    /// returning, so a late resolution after a timeout is harmlessly discarded.
    pub fn wait(self, timeout: std::time::Duration) -> Option<AckOutcome> {
        let (lock, cvar) = &*self.slot;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = std::time::Instant::now().checked_add(timeout);
        while guard.is_none() {
            match deadline {
                Some(deadline) => {
                    let now = std::time::Instant::now();
                    if now >= deadline {
                        break;
                    }
                    let (g, _timed_out) = cvar
                        .wait_timeout(guard, deadline - now)
                        .unwrap_or_else(|e| e.into_inner());
                    guard = g;
                }
                // `now + timeout` overflowed (an implausibly large timeout): park
                // without a deadline until resolved.
                None => {
                    guard = cvar.wait(guard).unwrap_or_else(|e| e.into_inner());
                }
            }
        }
        let outcome = *guard;
        drop(guard);
        // On timeout the slot is still in `waiters`; drop it so it can neither
        // leak nor be resolved into the void. On a resolved outcome
        // `resolve`/`fail_all` already removed it, and this is a cheap no-op.
        self.registry.cancel(self.id);
        outcome
    }
}

// ── Connect-time callback safety ─────────────────────────────────────────────

/// Opaque state snapshot returned by [`ConnectionEpoch::advance`].
///
/// Passed back to [`ConnectionEpoch::confirm`] to atomically claim "connected"
/// for the connection that was current when the token was issued. Carries no
/// public accessors — callers only round-trip it.
///
/// Requires the `std` feature.
#[cfg(feature = "std")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EpochToken(u32);

/// Tracks the current MQTT connection generation and whether it is confirmed
/// connected, in a single `AtomicU32`.
///
/// A per-connect helper thread runs `on_connect` off the event-loop thread (see
/// [`spawn_connect_thread`]) and, once it returns, wants to flip "connected" to
/// `true`. But the helper may still be running when a fast disconnect/reconnect
/// happens: a stale helper from a dead connection must never mark a *newer*
/// connection's connected flag, and must never resurrect a *dropped*
/// connection's flag either. `ConnectionEpoch` makes that race-free without a
/// lock: [`advance`](Self::advance) — called by the event loop on every
/// `Connected` *and* every `Disconnected` — bumps the generation and clears the
/// connected bit, returning an [`EpochToken`] that snapshots the new
/// generation. The helper holds onto that token and calls
/// [`confirm`](Self::confirm) after `on_connect` finishes; the compare-exchange
/// only succeeds if no `advance` happened in between, so a stale helper's
/// `confirm` harmlessly fails instead of flipping the flag for a connection
/// state that no longer applies.
///
/// The `u32` packs `generation << 1 | connected_bit` so both fields move
/// together in one atomic operation.
///
/// Requires the `std` feature.
#[cfg(feature = "std")]
#[derive(Default, Debug)]
pub struct ConnectionEpoch(std::sync::atomic::AtomicU32);

#[cfg(feature = "std")]
impl ConnectionEpoch {
    /// Creates a fresh epoch: generation 0, not connected.
    pub fn new() -> Self {
        Self(std::sync::atomic::AtomicU32::new(0))
    }

    /// Bumps the generation and clears the connected bit, returning a token
    /// that identifies this new generation.
    ///
    /// Call on every `Connected` (a fresh connection to confirm) and every
    /// `Disconnected` (the current connection is gone; any in-flight helper's
    /// token is now stale).
    pub fn advance(&self) -> EpochToken {
        use std::sync::atomic::Ordering;
        let mut current = self.0.load(Ordering::Acquire);
        loop {
            let generation = (current >> 1).wrapping_add(1);
            let next = generation << 1;
            match self
                .0
                .compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return EpochToken(next),
                Err(actual) => current = actual,
            }
        }
    }

    /// Atomically sets the connected bit, but only if `token` still matches the
    /// current generation.
    ///
    /// Returns `true` if the state was flipped to connected, `false` if a
    /// later [`advance`](Self::advance) (from any subsequent `Connected` or
    /// `Disconnected`) made `token` stale, in which case the state is left
    /// untouched. A second `confirm` with the same already-succeeded token is
    /// also a no-op returning `false`, since the stored value has moved on to
    /// `token.0 | 1` and no longer equals `token.0`.
    pub fn confirm(&self, token: EpochToken) -> bool {
        use std::sync::atomic::Ordering;
        self.0
            .compare_exchange(token.0, token.0 | 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Returns `true` if the current generation has been confirmed connected.
    pub fn is_connected(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.0.load(Ordering::Acquire) & 1 == 1
    }

    /// Returns `true` if `token` still identifies the current generation and
    /// has not yet been confirmed.
    ///
    /// A [`confirm`](Self::confirm) call changes the stored value away from
    /// `token.0` (to `token.0 | 1`), so `is_current` also returns `false`
    /// once `token` has already been confirmed — this reports "still worth
    /// starting work for", not merely "no reconnect happened yet". Intended
    /// for a connect helper to check *before* doing any work (e.g. a startup
    /// publish or `on_connect`), so a helper that is already stale by the
    /// time it starts running can skip that work entirely instead of doing
    /// it and then discovering [`confirm`](Self::confirm) fails.
    pub fn is_current(&self, token: EpochToken) -> bool {
        use std::sync::atomic::Ordering;
        self.0.load(Ordering::Acquire) == token.0
    }

    /// Constructs an epoch at a specific packed raw state (whitebox test hook
    /// for exercising generation wraparound without billions of iterations).
    #[cfg(test)]
    fn from_raw(raw: u32) -> Self {
        Self(std::sync::atomic::AtomicU32::new(raw))
    }
}

#[cfg(feature = "std")]
std::thread_local! {
    // Depth counter rather than a boolean: a plain flag restored to a
    // captured "previous" value breaks under out-of-order drops (e.g. two
    // scopes entered as `a` then `b`, but dropped as `a` then `b` — dropping
    // `a` first would restore `false` even though `b` is still active). A
    // counter only cares how many scopes are currently open, so drop order
    // is irrelevant.
    static CALLBACK_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Returns `true` if the current thread is inside a [`CallbackScope`].
///
/// Used by `MqttHandle` methods to refuse to run when called from a thread
/// that is currently executing an MQTT callback (`on_connect`, `on_message`,
/// `on_disconnect`) — such a call would deadlock, either on the client mutex
/// (already held by the thread) or on esp-mqtt's `api_lock` (held for the
/// duration of the event dispatch that is running the callback).
///
/// Requires the `std` feature.
#[cfg(feature = "std")]
pub fn in_callback() -> bool {
    CALLBACK_DEPTH.with(|cell| cell.get() > 0)
}

/// RAII guard marking the current thread as executing an MQTT callback.
///
/// [`enter`](Self::enter) increments the thread-local depth counter observed
/// by [`in_callback`]; dropping the guard decrements it, so nested scopes
/// (e.g. a callback that itself triggers another callback context) unwind
/// correctly regardless of the order in which they are dropped.
///
/// Not [`Send`]: the guard manipulates a thread-local, so entering it on one
/// thread and dropping it on another would decrement the wrong thread's
/// counter. Moving the value across threads is prevented at compile time via
/// a `PhantomData<*const ()>` field (`*const ()` is `!Send`).
///
/// Requires the `std` feature.
#[cfg(feature = "std")]
#[must_use = "the scope ends when this guard is dropped"]
#[derive(Debug)]
pub struct CallbackScope {
    _not_send: std::marker::PhantomData<*const ()>,
}

#[cfg(feature = "std")]
impl CallbackScope {
    /// Marks the current thread as inside an MQTT callback.
    pub fn enter() -> Self {
        CALLBACK_DEPTH.with(|cell| cell.set(cell.get() + 1));
        Self {
            _not_send: std::marker::PhantomData,
        }
    }
}

#[cfg(feature = "std")]
impl Drop for CallbackScope {
    fn drop(&mut self) {
        // `try_with` (rather than `with`): thread-local destruction order is
        // unspecified, so this drop can in principle run after
        // `CALLBACK_DEPTH` has already been torn down on a dying thread.
        // `with` would panic in that case; silently doing nothing is
        // correct because the counter no longer matters to a thread that is
        // shutting down.
        let _ = CALLBACK_DEPTH.try_with(|cell| cell.set(cell.get().saturating_sub(1)));
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "std")]
    use super::format_broker_url;
    use super::{
        connection_wait_iterations, next_state, resolve_client_id, topic_matches_filter,
        validate_broker_host, validate_broker_port, validate_client_id, validate_publish_topic,
        validate_subscribe_filter, validate_topic, MqttConnectionState, MqttEvent,
        CLIENT_ID_MAX_LEN, TOPIC_MAX_LEN,
    };

    // ── connection_wait_iterations ───────────────────────────────────────────

    #[test]
    fn zero_timeout_yields_zero_iterations() {
        assert_eq!(connection_wait_iterations(0), 0);
    }

    #[test]
    fn exact_multiple_is_not_rounded_up() {
        assert_eq!(connection_wait_iterations(100), 1);
        assert_eq!(connection_wait_iterations(5000), 50);
    }

    #[test]
    fn non_multiple_is_rounded_up() {
        // The edge case from the review: 5050 ms must not be truncated to 50
        assert_eq!(connection_wait_iterations(5050), 51);
        assert_eq!(connection_wait_iterations(5001), 51);
        assert_eq!(connection_wait_iterations(4999), 50);
    }

    #[test]
    fn sub_100ms_timeout_yields_one_iteration() {
        assert_eq!(connection_wait_iterations(1), 1);
        assert_eq!(connection_wait_iterations(99), 1);
    }

    // ── format_broker_url ───────────────────────────────────────────────────

    #[cfg(feature = "std")]
    #[test]
    fn broker_url_ip_and_standard_port() {
        assert_eq!(
            format_broker_url("192.168.1.100", 1883),
            "mqtt://192.168.1.100:1883"
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn broker_url_hostname_and_tls_port() {
        assert_eq!(
            format_broker_url("broker.example.com", 8883),
            "mqtt://broker.example.com:8883"
        );
    }

    // ── validate_client_id ──────────────────────────────────────────────────

    #[test]
    fn empty_client_id_is_rejected() {
        assert!(validate_client_id("").is_err());
    }

    #[test]
    fn single_char_client_id_is_accepted() {
        assert!(validate_client_id("a").is_ok());
    }

    #[test]
    fn client_id_at_max_len_is_accepted() {
        let id = "x".repeat(CLIENT_ID_MAX_LEN);
        assert!(validate_client_id(&id).is_ok());
    }

    #[test]
    fn client_id_over_max_len_is_rejected() {
        let id = "x".repeat(CLIENT_ID_MAX_LEN + 1);
        assert!(validate_client_id(&id).is_err());
    }

    // ── validate_topic ───────────────────────────────────────────────────────

    #[test]
    fn empty_topic_is_rejected() {
        assert!(validate_topic("").is_err());
    }

    #[test]
    fn typical_topic_is_accepted() {
        assert!(validate_topic("sensors/temperature").is_ok());
    }

    #[test]
    fn topic_with_nul_is_rejected() {
        assert!(validate_topic("topic\0name").is_err());
    }

    #[test]
    fn topic_at_max_len_is_accepted() {
        let t = "t".repeat(65535);
        assert!(validate_topic(&t).is_ok());
    }

    #[test]
    fn topic_over_max_len_is_rejected() {
        let t = "t".repeat(65536);
        assert!(validate_topic(&t).is_err());
    }

    // ── validate_broker_host ─────────────────────────────────────────────────

    #[test]
    fn empty_host_is_rejected() {
        assert!(validate_broker_host("").is_err());
    }

    #[test]
    fn ip_address_host_is_accepted() {
        assert!(validate_broker_host("192.168.1.1").is_ok());
    }

    #[test]
    fn hostname_is_accepted() {
        assert!(validate_broker_host("broker.example.com").is_ok());
    }

    // ── validate_broker_port ─────────────────────────────────────────────────

    #[test]
    fn port_zero_is_rejected() {
        assert!(validate_broker_port(0).is_err());
    }

    #[test]
    fn mqtt_standard_port_is_accepted() {
        assert!(validate_broker_port(1883).is_ok());
    }

    #[test]
    fn mqtt_tls_port_is_accepted() {
        assert!(validate_broker_port(8883).is_ok());
    }

    #[test]
    fn max_port_is_accepted() {
        assert!(validate_broker_port(u16::MAX).is_ok());
    }

    // ── next_state ───────────────────────────────────────────────────────────

    use MqttConnectionState as S;
    use MqttEvent as E;

    fn assert_transition(
        current: MqttConnectionState,
        event: MqttEvent,
        expected: Option<MqttConnectionState>,
    ) {
        assert_eq!(
            next_state(current, event),
            expected,
            "unexpected transition: {current:?} + {event:?}"
        );
    }

    #[test]
    fn connecting_on_connected_transitions_to_connected() {
        assert_transition(S::Connecting, E::Connected, Some(S::Connected));
    }

    #[test]
    fn connecting_on_disconnected_is_ignored() {
        assert_transition(S::Connecting, E::Disconnected, None);
    }

    #[test]
    fn connecting_on_shutdown_transitions_to_shutting_down() {
        assert_transition(S::Connecting, E::ShutdownRequested, Some(S::ShuttingDown));
    }

    #[test]
    fn connected_on_disconnected_transitions_to_disconnected() {
        assert_transition(S::Connected, E::Disconnected, Some(S::Disconnected));
    }

    #[test]
    fn connected_on_connected_is_ignored() {
        assert_transition(S::Connected, E::Connected, None);
    }

    #[test]
    fn connected_on_shutdown_transitions_to_shutting_down() {
        assert_transition(S::Connected, E::ShutdownRequested, Some(S::ShuttingDown));
    }

    #[test]
    fn disconnected_on_connected_transitions_to_connected() {
        assert_transition(S::Disconnected, E::Connected, Some(S::Connected));
    }

    #[test]
    fn disconnected_on_disconnected_is_ignored() {
        assert_transition(S::Disconnected, E::Disconnected, None);
    }

    #[test]
    fn disconnected_on_shutdown_transitions_to_shutting_down() {
        assert_transition(S::Disconnected, E::ShutdownRequested, Some(S::ShuttingDown));
    }

    #[test]
    fn shutting_down_ignores_connected() {
        assert_transition(S::ShuttingDown, E::Connected, None);
    }

    #[test]
    fn shutting_down_ignores_disconnected() {
        assert_transition(S::ShuttingDown, E::Disconnected, None);
    }

    #[test]
    fn shutting_down_ignores_shutdown_requested() {
        assert_transition(S::ShuttingDown, E::ShutdownRequested, None);
    }

    // ── validate_publish_topic ───────────────────────────────────────────────

    #[test]
    fn publish_topic_rejects_empty() {
        assert!(validate_publish_topic("").is_err());
    }

    #[test]
    fn publish_topic_rejects_plus_wildcard() {
        assert!(validate_publish_topic("sensors/+/temp").is_err());
    }

    #[test]
    fn publish_topic_rejects_hash_wildcard() {
        assert!(validate_publish_topic("sensors/#").is_err());
    }

    #[test]
    fn publish_topic_rejects_nul() {
        assert!(validate_publish_topic("topic\0name").is_err());
    }

    #[test]
    fn publish_topic_rejects_overlength() {
        let t = "t".repeat(TOPIC_MAX_LEN + 1);
        assert!(validate_publish_topic(&t).is_err());
    }

    #[test]
    fn publish_topic_accepts_normal() {
        assert!(validate_publish_topic("sensors/temperature").is_ok());
    }

    #[test]
    fn publish_topic_accepts_leading_slash() {
        assert!(validate_publish_topic("/sensors/temperature").is_ok());
    }

    #[test]
    fn publish_topic_accepts_trailing_slash() {
        assert!(validate_publish_topic("sensors/temperature/").is_ok());
    }

    // ── validate_subscribe_filter ────────────────────────────────────────────

    #[test]
    fn subscribe_filter_accepts_single_level_wildcard() {
        assert!(validate_subscribe_filter("sensors/+/temp").is_ok());
    }

    #[test]
    fn subscribe_filter_accepts_multi_level_wildcard() {
        assert!(validate_subscribe_filter("sensors/#").is_ok());
    }

    #[test]
    fn subscribe_filter_accepts_plus_alone() {
        assert!(validate_subscribe_filter("+").is_ok());
    }

    #[test]
    fn subscribe_filter_accepts_hash_alone() {
        assert!(validate_subscribe_filter("#").is_ok());
    }

    #[test]
    fn subscribe_filter_accepts_multiple_plus() {
        assert!(validate_subscribe_filter("+/+/+").is_ok());
    }

    #[test]
    fn subscribe_filter_rejects_plus_not_at_boundary() {
        assert!(validate_subscribe_filter("sport+").is_err());
    }

    #[test]
    fn subscribe_filter_rejects_hash_not_last() {
        assert!(validate_subscribe_filter("sport/#/trailing").is_err());
    }

    #[test]
    fn subscribe_filter_rejects_empty() {
        assert!(validate_subscribe_filter("").is_err());
    }

    #[test]
    fn subscribe_filter_rejects_nul() {
        assert!(validate_subscribe_filter("topic\0filter").is_err());
    }

    // ── topic_matches_filter ─────────────────────────────────────────────────

    #[test]
    fn match_exact() {
        assert!(topic_matches_filter(
            "sensors/temperature",
            "sensors/temperature"
        ));
    }

    #[test]
    fn match_single_level_wildcard() {
        assert!(topic_matches_filter("sensors/temperature", "sensors/+"));
    }

    #[test]
    fn match_multi_level_wildcard() {
        assert!(topic_matches_filter(
            "sensors/room/temperature",
            "sensors/#"
        ));
    }

    #[test]
    fn match_hash_alone_matches_all() {
        assert!(topic_matches_filter("sensors/room/temperature", "#"));
    }

    #[test]
    fn no_match_wrong_depth() {
        assert!(!topic_matches_filter(
            "sensors/temperature/extra",
            "sensors/+"
        ));
    }

    #[test]
    fn no_match_different_value() {
        assert!(!topic_matches_filter(
            "sensors/humidity",
            "sensors/temperature"
        ));
    }

    #[test]
    fn match_empty_level_with_plus() {
        // Topic "sensors//temp" has an empty middle level; + must match it
        assert!(topic_matches_filter("sensors//temp", "sensors/+/temp"));
    }

    #[test]
    fn dollar_topic_not_matched_by_hash() {
        assert!(!topic_matches_filter("$SYS/monitor", "#"));
    }

    #[test]
    fn dollar_topic_not_matched_by_plus() {
        assert!(!topic_matches_filter("$SYS/monitor", "+/monitor"));
    }

    #[test]
    fn dollar_topic_matched_by_dollar_hash() {
        assert!(topic_matches_filter("$SYS/monitor", "$SYS/#"));
    }

    #[test]
    fn dollar_topic_matched_by_dollar_plus() {
        assert!(topic_matches_filter("$SYS/monitor", "$SYS/+"));
    }

    #[test]
    fn match_trailing_hash_zero_levels() {
        // "sport/tennis/#" should match "sport/tennis" (zero additional levels)
        assert!(topic_matches_filter("sport/tennis", "sport/tennis/#"));
    }

    // ── resolve_client_id ────────────────────────────────────────────────────

    #[test]
    fn operator_id_valid_is_returned_as_is() {
        let result = resolve_client_id(Some("my-device-01"), "device", "fallback");
        assert_eq!(result, Ok("my-device-01"));
    }

    #[test]
    fn operator_id_at_max_len_is_accepted() {
        let id = "x".repeat(CLIENT_ID_MAX_LEN);
        let result = resolve_client_id(Some(id.as_str()), "device", "fallback");
        assert_eq!(result, Ok(id.as_str()));
    }

    #[test]
    fn operator_id_over_max_len_is_rejected() {
        let id = "x".repeat(CLIENT_ID_MAX_LEN + 1);
        let result = resolve_client_id(Some(id.as_str()), "device", "fallback");
        assert!(result.is_err());
    }

    #[test]
    fn empty_operator_id_falls_through_to_device_name() {
        let result = resolve_client_id(Some(""), "my-device", "fallback");
        assert_eq!(result, Ok("my-device"));
    }

    #[test]
    fn none_operator_id_falls_through_to_device_name() {
        let result = resolve_client_id(None, "my-device", "fallback");
        assert_eq!(result, Ok("my-device"));
    }

    /// A device_name longer than 23 bytes must be truncated on a UTF-8 char
    /// boundary. Using an emoji (4 bytes each) to prove a naive byte-slice
    /// would panic while the boundary-aware code yields a valid prefix.
    #[test]
    fn derived_truncation_lands_on_utf8_char_boundary() {
        // Each emoji is 4 bytes. Seven emojis = 28 bytes > CLIENT_ID_MAX_LEN=23.
        // Floor: 5 emojis = 20 bytes. The 6th would push to 24 bytes, so 5 is the
        // largest multiple of 4 that still fits in 23.
        let emoji = "🔥"; // U+1F525, 4 UTF-8 bytes
        let name: alloc::string::String = emoji.repeat(7); // 28 bytes
        let result = resolve_client_id(None, &name, "fallback");
        let chosen = result.expect("truncated id should be valid");
        assert_eq!(chosen, emoji.repeat(5), "expected 5 emojis (20 bytes)");
        assert!(chosen.len() <= CLIENT_ID_MAX_LEN);
    }

    /// Accented characters (2 bytes each) verify the same boundary logic with a
    /// different byte width: 12 × 'é' = 24 bytes → truncated to 11 × 'é' = 22 bytes.
    #[test]
    fn derived_truncation_with_two_byte_chars() {
        let ch = "é"; // U+00E9, 2 UTF-8 bytes
        let name: alloc::string::String = ch.repeat(12); // 24 bytes
        let result = resolve_client_id(None, &name, "fallback");
        let chosen = result.expect("truncated id should be valid");
        assert_eq!(chosen, ch.repeat(11), "expected 11 × 'é' (22 bytes)");
        assert!(chosen.len() <= CLIENT_ID_MAX_LEN);
    }

    #[test]
    fn empty_device_name_uses_fallback() {
        let result = resolve_client_id(None, "", "rustyfarian");
        assert_eq!(result, Ok("rustyfarian"));
    }

    #[test]
    fn fallback_itself_is_validated() {
        // A fallback that is empty should be rejected.
        let result = resolve_client_id(None, "", "");
        assert!(result.is_err());
    }

    #[test]
    fn fallback_over_max_len_is_rejected() {
        let long_fallback = "x".repeat(CLIENT_ID_MAX_LEN + 1);
        let result = resolve_client_id(None, "", long_fallback.as_str());
        assert!(result.is_err());
    }

    #[test]
    fn device_name_exactly_max_len_bytes_is_returned_whole() {
        let name = "a".repeat(CLIENT_ID_MAX_LEN);
        let result = resolve_client_id(None, &name, "fallback");
        assert_eq!(result, Ok(name.as_str()));
    }

    // ── spawn_subscriber_thread ──────────────────────────────────────────────

    #[cfg(feature = "std")]
    use super::{spawn_connect_thread, spawn_subscriber_thread, QoS, SubscribeClient};
    #[cfg(feature = "std")]
    use std::sync::{Arc, Condvar, Mutex};
    #[cfg(feature = "std")]
    use std::time::Duration;

    #[cfg(feature = "std")]
    struct BlockingMockClient {
        gate: Arc<(Mutex<bool>, Condvar)>,
        subscribed: Arc<Mutex<Vec<String>>>,
    }

    #[cfg(feature = "std")]
    impl SubscribeClient for BlockingMockClient {
        fn subscribe_topic(&mut self, topic: &str, _qos: QoS) -> anyhow::Result<()> {
            let (lock, cvar) = &*self.gate;
            let mut ready = lock.lock().unwrap();
            while !*ready {
                ready = cvar.wait(ready).unwrap();
            }
            self.subscribed.lock().unwrap().push(topic.to_string());
            Ok(())
        }
    }

    #[cfg(feature = "std")]
    fn wait_for_count(subscribed: &Arc<Mutex<Vec<String>>>, n: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if subscribed.lock().unwrap().len() == n {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "subscribe never completed within 2 s"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Regression test for the connect-time `api_lock` deadlock (esp-idf-svc
    /// 0.52+).
    ///
    /// The old architecture called `subscribe()` on the event loop thread
    /// inside the `Connected` handler. esp-mqtt dispatches `Connected` — and
    /// every event after it — while holding its recursive `api_lock`, and
    /// esp-idf-svc keeps the mqtt task parked until the event loop thread
    /// calls `connection.next()` again; any client call made from inside
    /// that handler, `subscribe()` included, would therefore wait on
    /// `api_lock` forever.
    ///
    /// The fix spawns a separate subscriber thread.  This test verifies that the
    /// spawner returns in <100 ms even while the mock's `subscribe_topic` is still
    /// blocking — a regression that would reliably catch any revert to the old
    /// inline pattern.
    #[cfg(feature = "std")]
    #[test]
    fn subscriber_thread_does_not_block_caller() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let subscribed = Arc::new(Mutex::new(Vec::<String>::new()));

        let client = Arc::new(Mutex::new(BlockingMockClient {
            gate: Arc::clone(&gate),
            subscribed: Arc::clone(&subscribed),
        }));

        let before = std::time::Instant::now();
        spawn_subscriber_thread(
            client,
            vec![("commands/#".to_string(), QoS::AtLeastOnce)],
            0,
        );
        let elapsed = before.elapsed();

        assert!(
            elapsed < Duration::from_millis(100),
            "spawn_subscriber_thread blocked for {elapsed:?}; \
             event loop thread would have deadlocked"
        );
        assert!(subscribed.lock().unwrap().is_empty());

        *gate.0.lock().unwrap() = true;
        gate.1.notify_one();
        wait_for_count(&subscribed, 1);
        assert_eq!(subscribed.lock().unwrap()[0], "commands/#");
    }

    /// All registered topics are subscribed in order when the mock is non-blocking.
    #[cfg(feature = "std")]
    #[test]
    fn subscriber_thread_subscribes_all_topics() {
        let gate = Arc::new((Mutex::new(true), Condvar::new()));
        let subscribed = Arc::new(Mutex::new(Vec::<String>::new()));

        let client = Arc::new(Mutex::new(BlockingMockClient {
            gate: Arc::clone(&gate),
            subscribed: Arc::clone(&subscribed),
        }));

        spawn_subscriber_thread(
            client,
            vec![
                ("commands/#".to_string(), QoS::AtLeastOnce),
                ("ota/manifest".to_string(), QoS::AtLeastOnce),
            ],
            0,
        );

        wait_for_count(&subscribed, 2);
        assert_eq!(
            subscribed.lock().unwrap().as_slice(),
            ["commands/#", "ota/manifest"]
        );
    }

    // ── spawn_connect_thread ─────────────────────────────────────────────────

    /// The prelude (e.g. a connect-time startup publish) must complete before
    /// any topic in `topics` is subscribed, so the broker sees a liveness ping
    /// ahead of the SUBSCRIBE packets.  `BlockingMockClient::subscribed` doubles
    /// as an ordered log here: the prelude and every `subscribe_topic` call push
    /// into the same vector under the same client lock.
    #[cfg(feature = "std")]
    #[test]
    fn connect_thread_runs_prelude_before_subscribes() {
        let gate = Arc::new((Mutex::new(true), Condvar::new()));
        let log = Arc::new(Mutex::new(Vec::<String>::new()));

        let client = Arc::new(Mutex::new(BlockingMockClient {
            gate: Arc::clone(&gate),
            subscribed: Arc::clone(&log),
        }));

        let prelude_log = Arc::clone(&log);
        let prelude = move |_client: &mut BlockingMockClient| {
            prelude_log.lock().unwrap().push("prelude".to_string());
        };

        spawn_connect_thread(
            client,
            Some(prelude),
            vec![
                ("commands/#".to_string(), QoS::AtLeastOnce),
                ("ota/manifest".to_string(), QoS::AtLeastOnce),
            ],
            0,
        );

        wait_for_count(&log, 3);
        assert_eq!(
            log.lock().unwrap().as_slice(),
            ["prelude", "commands/#", "ota/manifest"]
        );
    }

    /// Regression test mirroring `subscriber_thread_does_not_block_caller`, but
    /// with the blocking work moved into the prelude: the spawner must return
    /// immediately even while the prelude is still parked, or the event loop
    /// thread that called it would deadlock on `api_lock` exactly like the
    /// old inline `subscribe()` case.
    #[cfg(feature = "std")]
    #[test]
    fn connect_thread_does_not_block_caller() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let log = Arc::new(Mutex::new(Vec::<String>::new()));

        let client = Arc::new(Mutex::new(BlockingMockClient {
            gate: Arc::clone(&gate),
            subscribed: Arc::clone(&log),
        }));

        let prelude_gate = Arc::clone(&gate);
        let prelude_log = Arc::clone(&log);
        let prelude = move |_client: &mut BlockingMockClient| {
            let (lock, cvar) = &*prelude_gate;
            let mut ready = lock.lock().unwrap();
            while !*ready {
                ready = cvar.wait(ready).unwrap();
            }
            prelude_log.lock().unwrap().push("prelude".to_string());
        };

        let before = std::time::Instant::now();
        spawn_connect_thread(
            client,
            Some(prelude),
            vec![("commands/#".to_string(), QoS::AtLeastOnce)],
            0,
        );
        let elapsed = before.elapsed();

        assert!(
            elapsed < Duration::from_millis(500),
            "spawn_connect_thread blocked for {elapsed:?}; \
             event loop thread would have deadlocked"
        );
        assert!(log.lock().unwrap().is_empty());

        *gate.0.lock().unwrap() = true;
        gate.1.notify_one();
        wait_for_count(&log, 2);
        assert_eq!(log.lock().unwrap().as_slice(), ["prelude", "commands/#"]);
    }

    /// A prelude must still run even when there are no topics to subscribe —
    /// e.g. `with_startup_message()` without any `MqttBuilder::subscribe` call.
    #[cfg(feature = "std")]
    #[test]
    fn connect_thread_with_prelude_and_no_topics_still_runs_prelude() {
        let gate = Arc::new((Mutex::new(true), Condvar::new()));
        let log = Arc::new(Mutex::new(Vec::<String>::new()));

        let client = Arc::new(Mutex::new(BlockingMockClient {
            gate: Arc::clone(&gate),
            subscribed: Arc::clone(&log),
        }));

        let prelude_log = Arc::clone(&log);
        let prelude = move |_client: &mut BlockingMockClient| {
            prelude_log.lock().unwrap().push("prelude".to_string());
        };

        spawn_connect_thread(client, Some(prelude), Vec::new(), 0);

        wait_for_count(&log, 1);
        assert_eq!(log.lock().unwrap().as_slice(), ["prelude"]);
    }

    /// No prelude and no topics: nothing to do is not a spawn failure.
    #[cfg(feature = "std")]
    #[test]
    fn spawn_connect_thread_noop_returns_true() {
        let gate = Arc::new((Mutex::new(true), Condvar::new()));
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let client = Arc::new(Mutex::new(BlockingMockClient {
            gate: Arc::clone(&gate),
            subscribed: Arc::clone(&log),
        }));

        let spawned =
            spawn_connect_thread(client, None::<fn(&mut BlockingMockClient)>, Vec::new(), 0);

        assert!(spawned, "nothing to do must not be reported as a failure");
    }

    /// A real spawn (a thread is actually created) also reports success.
    /// `std::thread::Builder::spawn` failing is not something a host test can
    /// force reliably, so this only pins down the success case.
    #[cfg(feature = "std")]
    #[test]
    fn spawn_connect_thread_normal_case_returns_true() {
        let gate = Arc::new((Mutex::new(true), Condvar::new()));
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let client = Arc::new(Mutex::new(BlockingMockClient {
            gate: Arc::clone(&gate),
            subscribed: Arc::clone(&log),
        }));

        let spawned = spawn_connect_thread(
            client,
            None::<fn(&mut BlockingMockClient)>,
            vec![("commands/#".to_string(), QoS::AtLeastOnce)],
            0,
        );

        assert!(spawned);
        wait_for_count(&log, 1);
    }

    // ── PendingAcks (acknowledged-publish correlation) ───────────────────────

    #[cfg(feature = "std")]
    #[test]
    fn ack_resolve_before_wait_returns_immediately() {
        use super::{AckOutcome, PendingAcks};
        let acks = PendingAcks::new();
        let waiter = acks.register(7);
        // Resolve on this thread before waiting: the slot is filled synchronously.
        acks.resolve(7, AckOutcome::Acked);
        assert_eq!(
            waiter.wait(Duration::from_millis(50)),
            Some(AckOutcome::Acked)
        );
        assert_eq!(acks.waiter_count(), 0, "resolved waiter must be removed");
    }

    #[cfg(feature = "std")]
    #[test]
    fn ack_early_resolution_survives_until_register() {
        use super::{AckOutcome, PendingAcks};
        let acks = PendingAcks::new();
        // Outcome arrives BEFORE any waiter registers — buffered, not dropped.
        acks.resolve(42, AckOutcome::Acked);
        let waiter = acks.register(42);
        assert_eq!(
            waiter.wait(Duration::from_millis(50)),
            Some(AckOutcome::Acked)
        );
        assert_eq!(acks.waiter_count(), 0);
    }

    #[cfg(feature = "std")]
    #[test]
    fn ack_timeout_returns_none_and_cleans_up() {
        use super::PendingAcks;
        let acks = PendingAcks::new();
        let waiter = acks.register(1);
        assert_eq!(acks.waiter_count(), 1);
        // Never resolved → times out.
        assert_eq!(waiter.wait(Duration::from_millis(30)), None);
        assert_eq!(
            acks.waiter_count(),
            0,
            "a timed-out waiter must not leak a slot"
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn ack_cross_thread_wakeup() {
        use super::{AckOutcome, PendingAcks};
        let acks = PendingAcks::new();
        let waiter = acks.register(99);
        let acks_bg = acks.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            acks_bg.resolve(99, AckOutcome::Acked);
        });
        // Generous timeout: the resolve fires well before it.
        assert_eq!(waiter.wait(Duration::from_secs(2)), Some(AckOutcome::Acked));
        handle.join().unwrap();
    }

    #[cfg(feature = "std")]
    #[test]
    fn ack_fail_all_disconnects_every_waiter() {
        use super::{AckOutcome, PendingAcks};
        let acks = PendingAcks::new();
        let w1 = acks.register(1);
        let w2 = acks.register(2);
        assert_eq!(acks.waiter_count(), 2);
        acks.fail_all(AckOutcome::Disconnected);
        assert_eq!(
            w1.wait(Duration::from_millis(50)),
            Some(AckOutcome::Disconnected)
        );
        assert_eq!(
            w2.wait(Duration::from_millis(50)),
            Some(AckOutcome::Disconnected)
        );
        assert_eq!(acks.waiter_count(), 0);
    }

    #[cfg(feature = "std")]
    #[test]
    fn ack_fail_all_clears_stale_early_buffer() {
        use super::{AckOutcome, PendingAcks};
        let acks = PendingAcks::new();
        // An early outcome buffered before a session drop is stale afterwards.
        acks.resolve(5, AckOutcome::Acked);
        acks.fail_all(AckOutcome::Disconnected);
        // A new publish reusing id 5 must not pick up the pre-drop outcome; with
        // no fresh resolution it times out.
        let waiter = acks.register(5);
        assert_eq!(waiter.wait(Duration::from_millis(30)), None);
    }

    #[cfg(feature = "std")]
    #[test]
    fn ack_clone_shares_one_registry() {
        use super::{AckOutcome, PendingAcks};
        let acks = PendingAcks::new();
        let waiter = acks.register(3);
        // Resolving through an independent clone must reach the original's waiter.
        acks.clone().resolve(3, AckOutcome::Acked);
        assert_eq!(
            waiter.wait(Duration::from_millis(50)),
            Some(AckOutcome::Acked)
        );
    }

    // ── ConnectionEpoch ───────────────────────────────────────────────────────

    #[cfg(feature = "std")]
    #[test]
    fn fresh_epoch_is_not_connected() {
        use super::ConnectionEpoch;
        let epoch = ConnectionEpoch::new();
        assert!(!epoch.is_connected());
    }

    #[cfg(feature = "std")]
    #[test]
    fn confirm_succeeds_flips_connected() {
        use super::ConnectionEpoch;
        let epoch = ConnectionEpoch::default();
        let token = epoch.advance();
        assert!(epoch.confirm(token));
        assert!(epoch.is_connected());
    }

    #[cfg(feature = "std")]
    #[test]
    fn advance_clears_connected() {
        use super::ConnectionEpoch;
        let epoch = ConnectionEpoch::new();
        let token = epoch.advance();
        assert!(epoch.confirm(token));
        assert!(epoch.is_connected());
        // A Disconnected event also calls advance(); the token is discarded.
        let _ = epoch.advance();
        assert!(!epoch.is_connected());
    }

    #[cfg(feature = "std")]
    #[test]
    fn stale_token_confirm_fails_and_stays_disconnected() {
        use super::ConnectionEpoch;
        let epoch = ConnectionEpoch::new();
        let stale_token = epoch.advance();
        // A fast disconnect/reconnect happens before the stale helper confirms.
        let _fresh_token = epoch.advance();
        assert!(!epoch.confirm(stale_token));
        assert!(!epoch.is_connected());
    }

    #[cfg(feature = "std")]
    #[test]
    fn second_confirm_with_same_token_is_a_noop() {
        use super::ConnectionEpoch;
        let epoch = ConnectionEpoch::new();
        let token = epoch.advance();
        assert!(epoch.confirm(token));
        assert!(!epoch.confirm(token), "second confirm must not re-succeed");
        assert!(epoch.is_connected(), "connected state must be unaffected");
    }

    #[cfg(feature = "std")]
    #[test]
    fn generation_wraps_without_panic() {
        use super::ConnectionEpoch;
        // Start one advance() away from the packed u32's generation ceiling
        // (0x7FFF_FFFF << 1, connected=1) so the very next advance() truncates
        // the shift back to generation 0 instead of panicking.
        let epoch = ConnectionEpoch::from_raw(0xFFFF_FFFF);
        let token = epoch.advance();
        assert!(!epoch.is_connected(), "advance() must clear connected");
        assert!(
            epoch.confirm(token),
            "wrapped generation must still confirm"
        );
        assert!(epoch.is_connected());
    }

    #[cfg(feature = "std")]
    #[test]
    fn is_current_true_for_fresh_token_false_after_advance() {
        use super::ConnectionEpoch;
        let epoch = ConnectionEpoch::new();
        let token = epoch.advance();
        assert!(epoch.is_current(token));
        // A Disconnected (or a fresh Connected) event calls advance() again;
        // the earlier token is no longer current.
        let _ = epoch.advance();
        assert!(!epoch.is_current(token));
    }

    #[cfg(feature = "std")]
    #[test]
    fn is_current_false_after_own_confirm() {
        use super::ConnectionEpoch;
        let epoch = ConnectionEpoch::new();
        let token = epoch.advance();
        assert!(epoch.confirm(token));
        assert!(
            !epoch.is_current(token),
            "confirm() moves the stored value to token.0 | 1, so it no \
             longer equals token.0"
        );
    }

    /// Two fast reconnects race two helpers: `t1`'s helper is still running
    /// when `t2`'s connection lands and finishes first. `t1`'s later confirm
    /// must not undo `t2`'s.
    #[cfg(feature = "std")]
    #[test]
    fn stale_confirm_after_fresh_confirm_leaves_connected() {
        use super::ConnectionEpoch;
        let epoch = ConnectionEpoch::new();
        let t1 = epoch.advance();
        let t2 = epoch.advance();
        assert!(epoch.confirm(t2));
        assert!(!epoch.confirm(t1));
        assert!(epoch.is_connected());
    }

    // ── CallbackScope / in_callback ──────────────────────────────────────────

    #[cfg(feature = "std")]
    #[test]
    fn in_callback_is_false_by_default() {
        use super::in_callback;
        assert!(!in_callback());
    }

    #[cfg(feature = "std")]
    #[test]
    fn in_callback_is_true_inside_scope() {
        use super::{in_callback, CallbackScope};
        let _scope = CallbackScope::enter();
        assert!(in_callback());
    }

    #[cfg(feature = "std")]
    #[test]
    fn in_callback_is_false_after_drop() {
        use super::{in_callback, CallbackScope};
        {
            let _scope = CallbackScope::enter();
            assert!(in_callback());
        }
        assert!(!in_callback());
    }

    #[cfg(feature = "std")]
    #[test]
    fn another_thread_does_not_see_this_threads_scope() {
        use super::{in_callback, CallbackScope};
        let _scope = CallbackScope::enter();
        assert!(in_callback());
        let other_saw = std::thread::spawn(in_callback).join().unwrap();
        assert!(!other_saw, "in_callback must be per-thread");
        assert!(in_callback(), "this thread's scope must be unaffected");
    }

    #[cfg(feature = "std")]
    #[test]
    fn nested_scopes_restore_correctly() {
        use super::{in_callback, CallbackScope};
        assert!(!in_callback());
        let outer = CallbackScope::enter();
        assert!(in_callback());
        {
            let inner = CallbackScope::enter();
            assert!(in_callback());
            drop(inner);
            assert!(in_callback(), "outer scope must still be active");
        }
        drop(outer);
        assert!(!in_callback());
    }

    /// Regression test for the previous "restore captured previous value"
    /// design: dropping the two scopes in the order they were entered (not
    /// nested/LIFO order) must still leave `in_callback()` correct after
    /// each drop, because the depth counter does not care which scope is
    /// dropped when — only how many are currently open. `CallbackScope`
    /// is not `Send` (see its doc comment), so `a`/`b` here are always
    /// entered and dropped on this same thread; this test exercises
    /// non-LIFO *drop order* on one thread, not cross-thread drops.
    #[cfg(feature = "std")]
    #[test]
    fn scopes_dropped_out_of_order_still_clear() {
        use super::{in_callback, CallbackScope};
        let a = CallbackScope::enter();
        let b = CallbackScope::enter();
        drop(a);
        assert!(in_callback(), "b is still open");
        drop(b);
        assert!(!in_callback());
    }
}
