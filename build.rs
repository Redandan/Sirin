fn main() {
    // Only rebuild if source code changes, not runtime data
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=tauri.conf.json");
    
    tauri_build::build()
}
