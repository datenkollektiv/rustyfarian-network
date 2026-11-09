//! Shared workload and measurements of the two tier-parity spike examples
//! (`idf_c3_async_mqtt`, `idf_c3_async_mqtt_native`), so both variants do the same work
//! and report the same numbers.
//!
//! * Inbound messages go through one bounded queue with a drop counter, drained by
//!   [`rx`]; the producer is `on_message` (variant A) or the event pump (variant B).
//! * [`log_resources`] reports free and minimum free heap plus the stack high-water mark
//!   of the main task and of every named worker task the variant runs.
//! * [`name_next_threads`] labels the pthreads the library spawns from the calling
//!   thread, because Rust's `std::thread` names do not reach FreeRTOS on ESP-IDF.
//!
//! Each example includes this file with `#[path = "common/spike.rs"] mod spike;`.

use std::ffi::CStr;
use std::sync::atomic::{AtomicU32, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;

/// Inbound messages buffered between the producer thread or pump and [`rx`].
const INBOUND_DEPTH: usize = 4;

/// One received message, copied out of the MQTT event.
pub struct Inbound {
    topic: String,
    payload: Vec<u8>,
}

static INBOUND: Channel<CriticalSectionRawMutex, Inbound, INBOUND_DEPTH> = Channel::new();
static DROPPED_INBOUND: AtomicU32 = AtomicU32::new(0);
static RECEIVED_INBOUND: AtomicU32 = AtomicU32::new(0);

/// Queues a received message without blocking; a full queue drops it and counts it.
pub fn offer_inbound(topic: &str, payload: &[u8]) {
    let message = Inbound {
        topic: topic.to_string(),
        payload: payload.to_vec(),
    };
    if INBOUND.try_send(message).is_err() {
        DROPPED_INBOUND.fetch_add(1, Ordering::Relaxed);
    }
}

/// Drains the inbound queue and logs each message.
pub async fn rx() {
    loop {
        let message = INBOUND.receive().await;
        let n = RECEIVED_INBOUND.fetch_add(1, Ordering::Relaxed) + 1;
        match std::str::from_utf8(&message.payload) {
            Ok(text) => log::info!("[app] rx #{} topic={} payload={}", n, message.topic, text),
            Err(_) => log::info!(
                "[app] rx #{} topic={} payload={} bytes (binary)",
                n,
                message.topic,
                message.payload.len()
            ),
        }
    }
}

/// Labels the pthreads that the calling thread spawns next; `None` restores the default.
///
/// Rust passes its own stack size to `pthread_create`, which overrides the configured
/// one, so this only sets the FreeRTOS task name.
#[allow(dead_code)] // variant B spawns no library threads
pub fn name_next_threads(name: Option<&'static CStr>) {
    // SAFETY: plain FFI calls; `name` is a 'static C string, as ESP-IDF keeps the pointer.
    unsafe {
        let mut cfg = esp_idf_svc::sys::esp_pthread_get_default_config();
        if let Some(name) = name {
            cfg.thread_name = name.as_ptr();
        }
        cfg.inherit_cfg = false;
        esp_idf_svc::sys::esp_pthread_set_cfg(&cfg);
    }
}

fn free_heap() -> u32 {
    // SAFETY: plain FFI query with no arguments or side effects.
    unsafe { esp_idf_svc::sys::esp_get_free_heap_size() }
}

fn min_free_heap() -> u32 {
    // SAFETY: plain FFI query with no arguments or side effects.
    unsafe { esp_idf_svc::sys::esp_get_minimum_free_heap_size() }
}

/// Longest name FreeRTOS stores (`configMAX_TASK_NAME_LEN` 16 minus the NUL);
/// `xTaskGetHandle` asserts on a longer query.
const MAX_TASK_NAME_LEN: usize = 15;

/// Unused stack of the named task in bytes, or `None` if no task has that name.
fn stack_high_water(name: &CStr) -> Option<u32> {
    if name.to_bytes().len() > MAX_TASK_NAME_LEN {
        return None;
    }
    // SAFETY: `name` is a valid C string; a non-null handle refers to a live task.
    unsafe {
        let task = esp_idf_svc::sys::xTaskGetHandle(name.as_ptr());
        if task.is_null() {
            None
        } else {
            Some(esp_idf_svc::sys::uxTaskGetStackHighWaterMark(task))
        }
    }
}

/// Free heap, as a number for the tick payload.
pub fn heap_free() -> u32 {
    free_heap()
}

/// Logs heap, per-task stack high-water marks and the inbound counters.
pub fn log_resources(variant: &str, connects: u32, worker_tasks: &[&CStr]) {
    log::info!(
        "[app] {} heap free: {} B, min free: {} B, connects: {}, rx: {}, dropped rx: {}",
        variant,
        free_heap(),
        min_free_heap(),
        connects,
        RECEIVED_INBOUND.load(Ordering::Relaxed),
        DROPPED_INBOUND.load(Ordering::Relaxed)
    );
    for name in [c"main"].iter().chain(worker_tasks) {
        match stack_high_water(name) {
            Some(free) => log::info!(
                "[app] {} stack high-water {}: {} B unused",
                variant,
                name.to_string_lossy(),
                free
            ),
            None => log::info!(
                "[app] {} stack high-water {}: no such task",
                variant,
                name.to_string_lossy()
            ),
        }
    }
}
