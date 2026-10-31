//! Strict stored project fields used by project and radio admission.
use crate::raw_object::{self, RawObjectFields};
use serde::de::{Error, MapAccess};
use serde::{Deserialize, Deserializer, Serialize};
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
    pub name: Option<String>,
    pub locations: Vec<ProjectLocation>,
    pub extensions: BTreeMap<String, Box<RawValue>>,
}

/// A stored project location; unrecognized fields remain opaque.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLocation {
    pub server_id: String,
    pub path: String,
    pub project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_path: Option<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Box<RawValue>>,
}

#[derive(Default)]
struct LocationFields {
    server_id: Option<String>,
    path: Option<String>,
    project_id: Option<String>,
    git_origin: Option<String>,
    requested_path: Option<String>,
    seen: BTreeSet<String>,
}

impl<'de> RawObjectFields<'de> for LocationFields {
    type Output = ProjectLocation;
    fn field<M: MapAccess<'de>>(&mut self, name: &str, map: &mut M) -> Result<bool, M::Error> {
        if !matches!(
            name,
            "serverId" | "path" | "projectId" | "gitOrigin" | "requestedPath"
        ) {
            return Ok(false);
        }
        if !self.seen.insert(name.to_owned()) {
            return Err(M::Error::custom(format!("duplicate field {name}")));
        }
        match name {
            "serverId" => self.server_id = Some(map.next_value()?),
            "path" => self.path = Some(map.next_value()?),
            "projectId" => self.project_id = Some(map.next_value()?),
            "gitOrigin" => self.git_origin = map.next_value()?,
            "requestedPath" => self.requested_path = map.next_value()?,
            _ => unreachable!(),
        }
        Ok(true)
    }
    fn finish<E: Error>(
        self,
        extensions: BTreeMap<String, Box<RawValue>>,
    ) -> Result<ProjectLocation, E> {
        Ok(ProjectLocation {
            server_id: self.server_id.ok_or_else(|| E::missing_field("serverId"))?,
            path: self.path.ok_or_else(|| E::missing_field("path"))?,
            project_id: self
                .project_id
                .ok_or_else(|| E::missing_field("projectId"))?,
            git_origin: self.git_origin,
            requested_path: self.requested_path,
            extensions,
        })
    }
}

impl<'de> Deserialize<'de> for ProjectLocation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        raw_object::deserialize::<D, LocationFields>(deserializer)
    }
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
    name: Option<String>,
    locations: Vec<ProjectLocation>,
    preference: ProjectAccountPreference,
    seen: BTreeSet<String>,
}

impl<'de> RawObjectFields<'de> for Fields {
    type Output = ProjectRecord;
    fn field<M: MapAccess<'de>>(&mut self, name: &str, map: &mut M) -> Result<bool, M::Error> {
        if !matches!(
            name,
            "id" | "path"
                | "peerTeams"
                | "accountKey"
                | "accountGroups"
                | "accountKeys"
                | "name"
                | "locations"
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
            "name" => self.name = map.next_value()?,
            "locations" => self.locations = map.next_value()?,
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
            name: self.name,
            locations: self.locations,
            extensions,
        })
    }
}

impl<'de> Deserialize<'de> for ProjectRecord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        raw_object::deserialize::<D, Fields>(deserializer)
    }
}
