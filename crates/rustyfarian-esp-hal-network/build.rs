// Build script for `rustyfarian-esp-hal-network`.
//
// This crate is bare-metal (`no_std`); the script emits ONLY
// `cargo:rerun-if-env-changed` triggers and no link arguments, so it runs on
// the host and never affects the cross-target link. Its sole purpose is to
// force a rebuild when a `.env` value consumed by an example via `option_env!`
// changes — without this, editing `.env` leaves a stale example binary (a
// documented footgun; see `docs/project-lore.md`).
fn main() {
    // Wi-Fi credentials used by the connectivity examples.
    println!("cargo:rerun-if-env-changed=WIFI_SSID");
    println!("cargo:rerun-if-env-changed=WIFI_PASS");
    // Portal identity overrides used by the provisioning examples.
    println!("cargo:rerun-if-env-changed=PORTAL_SSID_PREFIX");
    println!("cargo:rerun-if-env-changed=DEVICE_NAME");
    // Non-secret portal pre-fill defaults read via `option_env!` in the
    // provisioning examples (see `.env.example`).
    println!("cargo:rerun-if-env-changed=MQTT_HOST");
    println!("cargo:rerun-if-env-changed=MQTT_PORT");
    println!("cargo:rerun-if-env-changed=MQTT_USER");
    println!("cargo:rerun-if-env-changed=MQTT_CLIENT_ID");
    println!("cargo:rerun-if-env-changed=OTA_URL");
    // Remaining non-secret keys in the shared `PortalDefaults` vocabulary
    // (`juggler::provisioning::PortalDefaults`). The bare-metal tier serves
    // only the `WifiMqttDevice` profile today, so no current example reads
    // these — they are declared for symmetry, so the trigger set stays
    // exhaustive over the shared surface and does not drift if a LoRaWAN
    // provisioning example lands here later.
    println!("cargo:rerun-if-env-changed=LORAWAN_DEV_EUI");
    println!("cargo:rerun-if-env-changed=LORAWAN_APP_EUI");
}
