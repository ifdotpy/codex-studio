fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "client_request",
            "close_ready",
            "close_response",
        ]),
    ))
    .expect("Could not generate the native client permissions");
}
