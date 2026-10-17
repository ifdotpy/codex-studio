#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--import-server-credentials") {
        if let Err(error) = studio_native_client::import_server_credentials() {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    studio_native_client::run();
}
