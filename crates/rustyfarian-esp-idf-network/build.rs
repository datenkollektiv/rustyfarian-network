fn main() {
    // Rerun triggers for Wi-Fi credentials used by wifi examples.
    println!("cargo:rerun-if-env-changed=WIFI_SSID");
    println!("cargo:rerun-if-env-changed=WIFI_PASS");
    // Rerun triggers for LoRaWAN credentials used by lora examples.
    println!("cargo:rerun-if-env-changed=LORAWAN_DEV_EUI");
    println!("cargo:rerun-if-env-changed=LORAWAN_APP_EUI");
    println!("cargo:rerun-if-env-changed=LORAWAN_APP_KEY");
    // Rerun trigger for sdkconfig (ESP-IDF config changes require a rebuild).
    // `sdkconfig.defaults` lives at the workspace root, not this crate's root;
    // `rerun-if-changed` paths are relative to the crate dir, so reach up two
    // levels (crates/rustyfarian-esp-idf-network/ -> workspace root).
    println!("cargo:rerun-if-changed=../../sdkconfig.defaults");
    // Required for ESP-IDF ldproxy linker argument injection.
    embuild::espidf::sysenv::output();
}
