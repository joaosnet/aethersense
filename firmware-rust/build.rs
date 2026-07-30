fn main() {
    println!("cargo:rustc-check-cfg=cfg(aethersense_esp32_classic)");
    println!("cargo:rustc-check-cfg=cfg(aethersense_esp32c5)");
    println!("cargo:rustc-check-cfg=cfg(aethersense_esp32c6)");
    println!("cargo:rustc-check-cfg=cfg(esp_idf_version_major, values(\"4\", \"5\"))");
    println!(
        "cargo:rustc-check-cfg=cfg(esp_idf_version_minor, values(\"0\", \"1\", \"2\", \"3\", \"4\", \"5\", \"6\"))"
    );
    for key in [
        "AETHERSENSE_UDP_HOST",
        "AETHERSENSE_UDP_PORT",
        "AETHERSENSE_SEND_FREQUENCY_HZ",
        "AETHERSENSE_QUEUE_CAPACITY",
        "AETHERSENSE_HARDWARE_PROFILE",
        "AETHERSENSE_WIFI_BAND",
        "AETHERSENSE_WIFI_PREFER_HT40",
        "AETHERSENSE_WIFI_SSID",
        "AETHERSENSE_WIFI_PASSWORD",
        "AETHERSENSE_WIFI_MAX_RETRY",
        "AETHERSENSE_RGB_LED_ENABLED",
        "AETHERSENSE_RGB_LED_GPIO",
        "AETHERSENSE_DEVICE_PROFILE_JSON",
        "AETHERSENSE_BUILD_HASH",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }

    let target = std::env::var("TARGET").unwrap_or_default();
    let mcu = std::env::var("MCU")
        .or_else(|_| std::env::var("AETHERSENSE_MCU"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let profile = std::env::var("AETHERSENSE_HARDWARE_PROFILE")
        .unwrap_or_default()
        .to_ascii_lowercase();

    if target == "xtensa-esp32-espidf"
        || mcu == "esp32"
        || profile == "classic"
        || profile == "wroom"
    {
        println!("cargo:rustc-cfg=aethersense_esp32_classic");
    }
    if mcu == "esp32c5" || profile == "esp32c5" || profile == "c5" {
        println!("cargo:rustc-cfg=aethersense_esp32c5");
    }
    if mcu == "esp32c6" || profile == "esp32c6" || profile == "c6" {
        println!("cargo:rustc-cfg=aethersense_esp32c6");
    }

    if target.contains("espidf") {
        embuild::espidf::sysenv::relay();
        embuild::espidf::sysenv::output();
    }
}
