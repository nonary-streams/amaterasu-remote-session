//! Strict public profile values and their catalog identity projection.
use super::{failure, invalid_catalog, printable, valid_id, valid_uuid};
use amaterasu_module_sdk::application_library::LibraryIdentityEntry;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Public continuation bound to one exact catalog revision and ordered ID.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileCursor {
    pub revision: u64,
    pub after: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProfileTarget {
    #[default]
    Owned,
    Existing {
        selector: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: String,
    pub uuid: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
    pub hdr_enabled: bool,
    pub scale: i32,
    #[serde(default)]
    pub target: ProfileTarget,
}

impl Profile {
    pub(super) fn validate(&self) -> Result<(), Value> {
        if (!self.id.is_empty() && !valid_id(&self.id))
            || (!self.uuid.is_empty() && !valid_uuid(&self.uuid))
            || !printable(&self.name, 160)
            || self.name.trim().is_empty()
            || !(320..=7680).contains(&self.width)
            || !(200..=4320).contains(&self.height)
            || !(1000..=1_000_000).contains(&self.refresh_millihz)
            || !matches!(
                self.scale,
                -1 | 0 | 100 | 125 | 150 | 175 | 200 | 225 | 250 | 300 | 350 | 400 | 450 | 500
            )
            || matches!(&self.target,ProfileTarget::Existing {selector} if !printable(selector,256))
        {
            return Err(failure(
                "invalid_profile",
                "profile identity, name, mode, scale or target is invalid",
            ));
        }
        Ok(())
    }

    pub(super) fn entry(&self) -> Value {
        json!({
            "name":self.name,"uuid":self.uuid,"remote_monitor_profile":true,
            "properties":{"remote_monitor":{"width":self.width,"height":self.height,"refresh_millihz":self.refresh_millihz,"hdr_enabled":self.hdr_enabled},"dd_virtual_display_scale":self.scale,"monitor_target":self.target},
            "config_overrides":{"dd_virtual_display_scale":self.scale},
            "launch":{"kind":"desktop"},
        })
    }

    pub(super) fn from_identity(value: Value) -> Result<Self, Value> {
        let row: LibraryIdentityEntry =
            serde_json::from_value(value).map_err(|_| invalid_catalog())?;
        row.validate().map_err(|_| invalid_catalog())?;
        if !row.monitor_profile || !valid_uuid(&row.uuid) {
            return Err(invalid_catalog());
        }
        let mode = row.properties.get("remote_monitor");
        if mode.is_some_and(|mode| !mode.is_object()) {
            return Err(invalid_catalog());
        }
        let dimension = |key, default| -> Result<u32, Value> {
            match mode.and_then(|mode| mode.get(key)) {
                None => Ok(default),
                Some(value) => value
                    .as_u64()
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or_else(invalid_catalog),
            }
        };
        let hdr_enabled = match mode.and_then(|mode| mode.get("hdr_enabled")) {
            None => false,
            Some(value) => value.as_bool().ok_or_else(invalid_catalog)?,
        };
        let scale = row
            .properties
            .get("dd_virtual_display_scale")
            .or_else(|| {
                row.properties
                    .pointer("/config_overrides/dd_virtual_display_scale")
            })
            .map(|value| {
                value
                    .as_i64()
                    .and_then(|value| i32::try_from(value).ok())
                    .ok_or_else(invalid_catalog)
            })
            .transpose()?
            .unwrap_or(100);
        let target = row
            .properties
            .get("monitor_target")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| invalid_catalog())?
            .unwrap_or_default();
        let result = Self {
            id: row.id,
            uuid: row.uuid,
            name: row.title,
            width: dimension("width", 1920)?,
            height: dimension("height", 1080)?,
            refresh_millihz: dimension("refresh_millihz", 60_000)?,
            hdr_enabled,
            scale,
            target,
        };
        result.validate()?;
        Ok(result)
    }
}
