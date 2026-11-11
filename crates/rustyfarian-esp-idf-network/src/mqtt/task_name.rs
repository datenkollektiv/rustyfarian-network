//! FreeRTOS task names for the library's threads (a tier fact, ADR 023
//! decision 9).
//!
//! `std::thread::Builder::name` stays a Rust-side label on ESP-IDF: every
//! spawned task is called `pthread`, so `xTaskGetHandle`, task lists and stack
//! reports cannot tell the library's threads apart (`docs/project-lore.md`
//! "ESP-IDF FreeRTOS Tasks & Threads"). ESP-IDF names a new task from the
//! spawning thread's `esp_pthread_cfg_t`, so [`spawn_named`] sets the name
//! around one spawn and restores the caller's configuration afterwards; a
//! caller that names its own threads the same way (an example's
//! `name_next_threads` bracket) keeps working.

use std::ffi::CStr;
use std::io;
use std::thread::{Builder, JoinHandle};

use esp_idf_svc::sys::{
    esp_pthread_cfg_t, esp_pthread_get_cfg, esp_pthread_get_default_config, esp_pthread_set_cfg,
    ESP_ERR_NOT_FOUND, ESP_OK,
};

/// Longest name FreeRTOS stores (`configMAX_TASK_NAME_LEN` 16 minus the NUL);
/// `xTaskGetHandle` asserts on a longer query.
const MAX_TASK_NAME_LEN: usize = 15;

const _: () = assert!(
    c"mqtt-events".count_bytes() <= MAX_TASK_NAME_LEN
        && c"mqtt-net".count_bytes() <= MAX_TASK_NAME_LEN
);

/// The caller's pthread configuration before the name was set.
///
/// `None` when nothing was set (the name could not be configured, so there is
/// nothing to restore); `Some(None)` when the caller had no configuration of
/// its own (ESP-IDF cannot clear one, so the default is restored instead, which
/// creates threads like no configuration does).
type SavedConfig = Option<Option<esp_pthread_cfg_t>>;

/// Spawns `f` on a thread whose FreeRTOS task is called `name`, with Rust's
/// `stack_size` (it overrides the configured one, lore).
///
/// The name must stay within [`MAX_TASK_NAME_LEN`] bytes; the library's two
/// names are checked at compile time above.
pub(super) fn spawn_named(
    name: &'static CStr,
    stack_size: usize,
    f: impl FnOnce() + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    let saved = set_task_name(name);
    let spawned = Builder::new()
        .name(name.to_string_lossy().into_owned())
        .stack_size(stack_size)
        .spawn(f);
    restore_config(saved);
    spawned
}

/// Configures `name` for the threads this thread spawns next and returns what
/// to restore afterwards.
fn set_task_name(name: &'static CStr) -> SavedConfig {
    // SAFETY: plain FFI calls with pointers to live locals; `name` is a
    // 'static C string, as ESP-IDF keeps the pointer.
    unsafe {
        let mut probe = esp_pthread_get_default_config();
        let saved = match esp_pthread_get_cfg(&mut probe) {
            ESP_OK => Some(probe),
            ESP_ERR_NOT_FOUND => None,
            code => {
                log::warn!(
                    "[mqtt] esp_pthread_get_cfg failed: {code:#x}; the {name:?} thread keeps \
                     its default FreeRTOS name"
                );
                return None;
            }
        };
        // `esp_pthread_get_cfg` zeroes `probe` before reporting `ESP_ERR_NOT_FOUND`
        // (pthread.c), so the default is fetched afresh rather than reused.
        let mut cfg: esp_pthread_cfg_t = saved.unwrap_or_else(|| esp_pthread_get_default_config());
        cfg.thread_name = name.as_ptr();
        cfg.inherit_cfg = false;
        let set = esp_pthread_set_cfg(&cfg);
        if set != ESP_OK {
            log::warn!(
                "[mqtt] esp_pthread_set_cfg failed: {set:#x}; the {name:?} thread keeps its \
                 default FreeRTOS name"
            );
            return None;
        }
        Some(saved)
    }
}

/// Puts the caller's pthread configuration back.
fn restore_config(saved: SavedConfig) {
    let Some(saved) = saved else { return };
    // SAFETY: plain FFI calls with pointers to live locals; the restored
    // configuration is the caller's own, whose `thread_name` pointer it set itself.
    let restored = unsafe {
        match &saved {
            Some(cfg) => esp_pthread_set_cfg(cfg),
            None => esp_pthread_set_cfg(&esp_pthread_get_default_config()),
        }
    };
    if restored != ESP_OK {
        log::warn!("[mqtt] failed to restore the caller's pthread configuration: {restored:#x}");
    }
}
