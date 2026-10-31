//! Strict stored project fields used by project and radio admission.
use crate::raw_object::{self, RawObjectFields};
use serde::de::{Error, MapAccess};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectAccountPreference {
    pub account_key: Option<String>,
    #[serde(default)]
    pub account_groups: Vec<String>,
    #[serde(default)]
    pub account_keys: Vec<String>,
}

#[derive(Debug)]
pub struct ProjectRecord {
    pub id: String,
    pub path: String,
    pub peer_teams: Vec<ProjectPeerTeam>,
    pub preference: ProjectAccountPreference,
    pub extensions: BTreeMap<String, Box<RawValue>>,
}

#[derive(Debug, Deserialize)]
pub struct ProjectPeerTeam {
    pub id: String,
    pub name: String,
    pub members: Vec<String>,
}

#[derive(Default)]
struct Fields {
    id: Option<String>,
    path: Option<String>,
    peer_teams: Option<Vec<ProjectPeerTeam>>,
    preference: ProjectAccountPreference,
    seen: BTreeSet<String>,
}

impl<'de> RawObjectFields<'de> for Fields {
    type Output = ProjectRecord;
    fn field<M: MapAccess<'de>>(&mut self, name: &str, map: &mut M) -> Result<bool, M::Error> {
        if !matches!(
            name,
            "id" | "path" | "peerTeams" | "accountKey" | "accountGroups" | "accountKeys"
        ) {
            return Ok(false);
        }
        if !self.seen.insert(name.to_owned()) {
            return Err(M::Error::custom(format!("duplicate field {name}")));
        }
        match name {
            "id" => self.id = Some(map.next_value()?),
            "path" => self.path = Some(map.next_value()?),
            "peerTeams" => self.peer_teams = Some(map.next_value()?),
            "accountKey" => self.preference.account_key = map.next_value()?,
            "accountGroups" => {
                self.preference.account_groups =
                    map.next_value::<Option<Vec<String>>>()?.unwrap_or_default()
            }
            "accountKeys" => {
                self.preference.account_keys =
                    map.next_value::<Option<Vec<String>>>()?.unwrap_or_default()
            }
            _ => unreachable!(),
        }
        Ok(true)
    }
    fn finish<E: Error>(
        self,
        extensions: BTreeMap<String, Box<RawValue>>,
    ) -> Result<ProjectRecord, E> {
        Ok(ProjectRecord {
            id: self.id.ok_or_else(|| E::missing_field("id"))?,
            path: self.path.ok_or_else(|| E::missing_field("path"))?,
            peer_teams: self
                .peer_teams
                .ok_or_else(|| E::missing_field("peerTeams"))?,
            preference: self.preference,
            extensions,
        })
    }
}

impl<'de> Deserialize<'de> for ProjectRecord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        raw_object::deserialize::<D, Fields>(deserializer)
    }
}
