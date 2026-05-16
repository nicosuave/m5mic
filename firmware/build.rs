fn main() {
    println!("cargo:rerun-if-env-changed=M5MIC_SERVER_URL");
    println!("cargo:rerun-if-env-changed=WIFI_SSID");
    println!("cargo:rerun-if-env-changed=WIFI_PASS");
    embuild::espidf::sysenv::output();
}
