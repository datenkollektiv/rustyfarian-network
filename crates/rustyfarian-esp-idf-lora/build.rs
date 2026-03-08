fn main() {
    // Declare sdkconfig.defaults as a build dependency so Cargo re-runs this
    // script (and triggers an ESP-IDF CMake reconfigure) whenever the file changes.
    // Note: embuild reads sdkconfig.defaults from the workspace root, not the crate root.
    println!("cargo:rerun-if-changed=../../sdkconfig.defaults");
    embuild::espidf::sysenv::output();
}
