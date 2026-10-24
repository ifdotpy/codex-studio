//! Profile request rules used by the panel routes.

use studio_runtime_store::{ProfileRecord, ProfileRole, ReasoningEffort};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileField {
    Name,
    Model,
}

pub fn make_profile(
    id: String,
    name: &str,
    role: ProfileRole,
    model: &str,
    effort: Option<ReasoningEffort>,
    instructions: &str,
) -> Result<ProfileRecord, ProfileField> {
    let name = name.trim();
    let model = model.trim();
    if name.is_empty() {
        return Err(ProfileField::Name);
    }
    if model.is_empty() {
        return Err(ProfileField::Model);
    }
    Ok(ProfileRecord {
        id,
        name: name.to_owned(),
        role,
        model: model.to_owned(),
        effort,
        instructions: instructions.trim().to_owned(),
    })
}
