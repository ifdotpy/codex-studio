//! Disposable verification credentials. Never reads or emits a paper key.
use layr_store::{keys, person, session};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

fn private(path: &Path, bytes: &[u8]) {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args[1].as_str() {
        "person" => {
            let dir = Path::new(&args[2]);
            fs::create_dir(dir).unwrap();
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
            let identity = keys::generate_identity();
            let recipient = keys::recipient_of(&identity).unwrap();
            let signing = person::make_signing_keys(&recipient).unwrap();
            private(Path::new(&args[3]), identity.as_bytes());
            private(&dir.join("recipient.txt"), recipient.as_bytes());
            private(
                &dir.join("signing-public.txt"),
                signing.public_key.as_bytes(),
            );
            private(&dir.join("signing.age"), &signing.sealed);
            private(
                &dir.join("index.age"),
                &person::seal_index(&identity, &person::Index::default()).unwrap(),
            );
        }
        "encrypt" => {
            let mut input = Vec::new();
            std::io::stdin().read_to_end(&mut input).unwrap();
            std::io::stdout()
                .write_all(&keys::encrypt(&args[2], &input).unwrap())
                .unwrap();
        }
        "verify" => {
            let message = session::message(&args[2], &args[3]).unwrap();
            keys::verify_bytes(
                &message,
                &args[4],
                &person::verifying_key(&args[5]).unwrap(),
            )
            .unwrap();
        }
        _ => panic!("unknown fixture operation"),
    }
}
