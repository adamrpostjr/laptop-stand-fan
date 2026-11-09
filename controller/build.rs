//! On macOS, embed `macos/Info.plist` in the binary (`__TEXT,__info_plist`).
//! Without `NSBluetoothAlwaysUsageDescription`, macOS kills a process the
//! moment it touches CoreBluetooth. It also gives the Bluetooth permission
//! prompt a proper name.
fn main() {
    println!("cargo:rerun-if-changed=macos/Info.plist");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let plist = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("macos/Info.plist");
        println!(
            "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
