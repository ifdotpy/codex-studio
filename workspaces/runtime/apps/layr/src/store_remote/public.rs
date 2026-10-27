//! Publish and open the ordinary public reader slot.
use super::{backend::Backend, keys, reader, Descriptor, Store};
use anyhow::{bail, Context, Result};
use layr_store::public::{trusted_writers, Manifest};

pub(super) fn identity(backend: &Backend, descriptor: &Descriptor) -> Result<Option<String>> {
    let Some(bytes) = backend.public_manifest()? else { return Ok(None) };
    let manifest = Manifest::parse(&bytes, descriptor)?;
    let slot = backend.get("slots/public.age")?.context("public store has no public slot; its owner must finish publication")?;
    let identity = manifest.open(&slot)?;
    backend.unlock_reader(&identity)?;
    let records = reader::records(backend, &identity, &descriptor.project)?;
    manifest.verify_signer(&trusted_writers(&descriptor.project, &records.lines)?)?;
    Ok(Some(identity))
}

impl Store {
    /// Current admins can publish only through their own existing trusted writer slot.
    pub fn make_public(&mut self, ctx: &crate::ctx::Ctx, repo: &crate::repo::Repo) -> Result<bool> {
        repo.require(ctx, "remote")?;
        if repo.p.is_overlay() || self.layer.is_some() {
            bail!("overlays cannot be made public");
        }
        self.connect(ctx)?;
        self.publish_public(ctx, repo)
    }
    pub(super) fn publish_public(&mut self, _ctx: &crate::ctx::Ctx, _repo: &crate::repo::Repo) -> Result<bool> {
        let descriptor = self.descriptor()?;
        let slot = self
            .backend
            .get(&format!("slots/machine-{}.age", self.machine))?
            .context("making a project public requires this machine's existing trusted writer slot")?;
        let vault = keys::open_machine_slot(&slot, &self.key)
            .context("making a project public requires this machine's existing trusted writer slot")?;
        if keys::recipient_of(&vault)? != descriptor.recipient {
            bail!("machine slot does not match store recipient");
        }
        self.backend.unlock_reader(&vault)?;
        let records = reader::records(&self.backend, &vault, &descriptor.project)?;
        let trusted = trusted_writers(&descriptor.project, &records.lines)?;
        if trusted.get(&self.machine) != Some(&self.key.verifying_key()) {
            bail!("making a project public requires a trusted project writer machine");
        }
        let existing = self.backend.get("public.json")?;
        let created = if existing.is_none() {
            if self.backend.get("slots/public.age")?.is_some() {
                bail!("public slot exists without its signed manifest; nothing was changed");
            }
            let manifest = Manifest::new(&descriptor, keys::generate_identity(), self.machine.clone(), &self.key)?;
            // Publish the opener first. An interrupted slot publication can then be retried
            // without persisting the vault or keeping a second copy of either object.
            self.backend.put_new("public.json", serde_json::to_vec(&manifest)?)?
        } else {
            false
        };
        let bytes = self.backend.get("public.json")?.context("public manifest disappeared")?;
        let manifest = Manifest::parse(&bytes, &descriptor)?;
        manifest.verify_signer(&trusted)?;
        if let Some(slot) = self.backend.get("slots/public.age")? {
            manifest.open(&slot)?;
        } else {
            self.backend.put_new("slots/public.age", keys::seal_slot(&vault, &keys::recipient_of(&manifest.identity)?)?)?;
            let slot = self.backend.get("slots/public.age")?.context("public slot disappeared")?;
            manifest.open(&slot)?;
        }
        Ok(created)
    }
}
