//! The runner's view of one connect step (ADR 023 decision 10): each
//! [`ConnectPort`] method is one esp-mqtt call, performed on the `mqtt-net`
//! thread under the outbox's client lock. The step order lives in
//! `juggler::mqtt::ConnectWorkMachine`; the runner feeds it one action per turn.

use std::time::Instant;

use juggler::mqtt::{CallbackScope, ConnectPort, SubscribeClient as _};

use super::subscriptions::Subscriptions;
use super::{to_esp_qos, OnConnectCallback, StartupMessage, SubscribableClient};

/// The runner's view of one connect step, built inside one `with_client_mut`
/// hold (a struct, not loose arguments: the Xtensa LLVM register-scavenge
/// crash, `docs/project-lore.md` "Toolchain & Build").
pub(super) struct IdfConnectPort<'a> {
    pub(super) client: &'a mut SubscribableClient,
    pub(super) subs: &'a Subscriptions,
    pub(super) startup: Option<&'a StartupMessage>,
    pub(super) on_connect: Option<&'a OnConnectCallback>,
}

/// Past this an `on_connect` is reported as slow: the runner performed no
/// Close, Shutdown or retry meanwhile (ADR 017).
pub(super) const ON_CONNECT_SLOW_WARN_MS: u128 = 1_000;

impl ConnectPort for IdfConnectPort<'_> {
    /// SUBSCRIBE for entry `index`, then `mark_sent`; a missing entry is done.
    fn subscribe(&mut self, index: usize) -> bool {
        let Some((topic, qos)) = self.subs.get(index) else {
            return true;
        };
        match self.client.subscribe_topic(&topic, qos) {
            Ok(()) => {
                self.subs.mark_sent(index, qos);
                log::info!("[mqtt] subscribed to '{}'", topic);
                true
            }
            Err(e) => {
                log::warn!("[mqtt] subscribe to '{}' failed: {:#}", topic, e);
                false
            }
        }
    }

    /// Hands the startup message to esp-mqtt's outbox; none configured is done.
    fn publish_startup(&mut self) -> bool {
        let Some(message) = self.startup else {
            return true;
        };
        match self.client.enqueue(
            &message.topic,
            to_esp_qos(message.qos),
            message.retain,
            &message.payload,
        ) {
            Ok(_) => true,
            Err(e) => {
                log::warn!(
                    "[mqtt] startup-message publish to '{}' failed: {:?}",
                    message.topic,
                    e
                );
                false
            }
        }
    }

    /// Runs the user's `on_connect` under [`CallbackScope`] (ADR 017 §3): a
    /// failure is logged, never fatal; a slow one is reported, since session
    /// control waits for it.
    fn run_on_connect(&mut self, session_present: bool) {
        let Some(callback) = self.on_connect else {
            return;
        };
        let _scope = CallbackScope::enter();
        let started = Instant::now();
        if let Err(e) = callback(self.client, session_present) {
            log::warn!("[mqtt] on_connect callback failed: {:#}", e);
        }
        let took_ms = started.elapsed().as_millis();
        if took_ms > ON_CONNECT_SLOW_WARN_MS {
            log::warn!(
                "[mqtt] on_connect took {} ms; session control waited that long (ADR 017)",
                took_ms
            );
        }
    }

    /// Runtime requests the plan did not cover (stored while disconnected on a
    /// kept session, or admitted meanwhile) go out, then the tracker goes live.
    fn finish(&mut self) -> bool {
        let client = &mut *self.client;
        self.subs
            .catch_up_and_go_live(|topic, qos| match client.subscribe_topic(topic, qos) {
                Ok(()) => {
                    log::info!("[mqtt] subscribed to '{}'", topic);
                    true
                }
                Err(e) => {
                    log::warn!("[mqtt] subscribe to '{}' failed: {:#}", topic, e);
                    false
                }
            })
    }
}
