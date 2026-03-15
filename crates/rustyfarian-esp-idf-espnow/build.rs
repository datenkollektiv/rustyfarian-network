fn main() {
    println!("cargo:rerun-if-changed=../../sdkconfig.defaults");
    embuild::espidf::sysenv::output();
}
