fn main() {
    // Declare sdkconfig.defaults as a build dependency so Cargo re-runs this
    // script (and triggers an ESP-IDF CMake reconfigure) whenever the file changes.
    // Note: embuild reads sdkconfig.defaults from the workspace root, not the crate root.
    println!("cargo:rerun-if-changed=../../sdkconfig.defaults");

    // Declare LoRaWAN credential env vars so Cargo re-compiles the example
    // whenever they change (e.g., from .env updates or inline overrides).
    println!("cargo:rerun-if-env-changed=LORAWAN_DEV_EUI");
    println!("cargo:rerun-if-env-changed=LORAWAN_APP_EUI");
    println!("cargo:rerun-if-env-changed=LORAWAN_APP_KEY");

    // Loud build-time check: if the DevEUI/AppKey are absent or all-zero, the
    // examples will compile placeholder credentials and TTN will silently reject
    // the join (no accept). This catches the common LORA_* vs LORAWAN_* naming
    // mismatch in .env. Only the variable *presence* is inspected — never logged.
    let is_blank = |v: &str| v.is_empty() || v.chars().all(|c| c == '0');
    let dev_eui = std::env::var("LORAWAN_DEV_EUI").unwrap_or_default();
    let app_key = std::env::var("LORAWAN_APP_KEY").unwrap_or_default();
    if is_blank(&dev_eui) || is_blank(&app_key) {
        println!(
            "cargo:warning=LoRaWAN credentials missing or all-zero — examples will use \
             placeholder values and TTN will reject the join. Set LORAWAN_DEV_EUI / \
             LORAWAN_APP_EUI / LORAWAN_APP_KEY (note the 'WAN') in .env."
        );
    }

    embuild::espidf::sysenv::output();
}
