//! Test helper. Secret identities are read from private files, never command arguments or output.
use anyhow::{Context, Result};
use layr_store::keys;
use std::{fs, io::Write, os::unix::fs::OpenOptionsExt};
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("new") => {
            let identity = keys::generate_identity();
            let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&args[1])?;
            file.write_all(identity.as_bytes())?;
            println!("{}", keys::recipient_of(&identity)?);
        }
        Some("person") => {
            let paper = keys::identity_from_paper(fs::read_to_string(&args[1])?.trim())?;
            let person = keys::open_slot(&fs::read(&args[2])?, &paper)?;
            let vault = keys::open_slot(&fs::read(&args[3])?, &person)?;
            if args.len() > 4 {
                keys::decrypt(&vault, &fs::read(&args[4])?)?;
            }
            println!("opened");
        }
        Some("opens") => {
            let identity = fs::read_to_string(&args[1])?;
            let slot = fs::read(&args[2])?;
            let opened = keys::open_slot(&slot, identity.trim()).context("test opener cannot open slot")?;
            if args.len() > 3 {
                let data = fs::read(&args[3])?;
                keys::decrypt(&opened, &data).context("test epoch identity cannot decrypt object")?;
            }
            println!("opened");
        }
        _ => anyhow::bail!("usage: epoch_probe new <private-file> | opens <private-file> <slot> [object]"),
    }
    Ok(())
}
