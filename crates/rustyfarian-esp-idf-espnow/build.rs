fn main() {
    println!("cargo:rerun-if-changed=../../sdkconfig.defaults");
    println!("cargo:rerun-if-env-changed=WIFI_SSID");
    println!("cargo:rerun-if-env-changed=WIFI_PASS");
    println!("cargo:rerun-if-env-changed=COORDINATOR_MAC");
    embuild::espidf::sysenv::output();
}
