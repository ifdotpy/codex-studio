fn main() {
    // iOS shell: optional server address compiled in (ios_server::BUILD_DEFAULT),
    // from the environment or the untracked ios-default-server.txt (Xcode runs
    // the Rust build with its own environment).
    println!("cargo:rerun-if-env-changed=STUDIO_DEFAULT_SERVER_URL");
    println!("cargo:rerun-if-changed=ios-default-server.txt");
    let default_server = std::env::var("STUDIO_DEFAULT_SERVER_URL")
        .ok()
        .or_else(|| std::fs::read_to_string("ios-default-server.txt").ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if let Some(value) = default_server {
        println!("cargo:rustc-env=STUDIO_DEFAULT_SERVER_URL={value}");
    }
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "client_request",
            "close_ready",
            "close_response",
        ]),
    ))
    .expect("Could not generate the native client permissions");
}
