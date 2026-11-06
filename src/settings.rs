//! Product preferences are public; provider grants remain trusted startup data.
use serde::{Deserialize, Deserializer, Serialize, de::Error};
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProductSettings {
    pub remote_monitor_mute_audio: bool,
    pub remote_monitor_disconnect_on_stream_end: bool,
    pub remote_monitor_disconnect_on_client_disconnect: bool,
    pub remote_monitor_terminate_on_first_request: bool,
    pub remote_monitor_confirm_app_replacement: bool,
    pub show_remote_input: bool,
    pub show_remote_monitor: bool,
    pub max_clients: usize,
    #[serde(default = "empty_layout")]
    pub remote_monitor_layout: Value,
}

impl Default for ProductSettings {
    fn default() -> Self {
        Self {
            remote_monitor_mute_audio: false,
            remote_monitor_disconnect_on_stream_end: false,
            remote_monitor_disconnect_on_client_disconnect: false,
            remote_monitor_terminate_on_first_request: false,
            remote_monitor_confirm_app_replacement: true,
            show_remote_input: true,
            show_remote_monitor: true,
            max_clients: 16,
            remote_monitor_layout: empty_layout(),
        }
    }
}

impl ProductSettings {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(1..=32).contains(&self.max_clients) {
            return Err("max_clients must be between 1 and 32");
        }
        if !self.remote_monitor_layout.is_object()
            || self.remote_monitor_layout["version"] != 1
            || !self.remote_monitor_layout["placements"]
                .as_object()
                .is_some_and(|placements| placements.len() <= 32)
            || !serde_json::to_vec(&self.remote_monitor_layout)
                .is_ok_and(|bytes| bytes.len() <= 12_000)
        {
            return Err("remote_monitor_layout must be a bounded version 1 layout");
        }
        Ok(())
    }
}

fn empty_layout() -> Value {
    serde_json::json!({"version":1,"placements":{}})
}

/// This type can be read only from trusted composition. Public settings writes
/// deserialize `ProductSettings`, which has no fields capable of changing grants.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub allowed_principals: Vec<String>,
    pub source_verifiers: Vec<String>,
    pub product: ProductSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_principals: Vec::new(),
            source_verifiers: Vec::new(),
            product: ProductSettings::default(),
        }
    }
}

impl<'de> Deserialize<'de> for Settings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let mut fields = value
            .as_object()
            .cloned()
            .ok_or_else(|| D::Error::custom("startup settings must be an object"))?;
        let enabled = fields
            .remove("enabled")
            .map(serde_json::from_value)
            .transpose()
            .map_err(D::Error::custom)?
            .unwrap_or(true);
        let allowed_principals = fields
            .remove("allowed_principals")
            .map(serde_json::from_value)
            .transpose()
            .map_err(D::Error::custom)?
            .unwrap_or_default();
        let source_verifiers = fields
            .remove("source_verifiers")
            .map(serde_json::from_value)
            .transpose()
            .map_err(D::Error::custom)?
            .unwrap_or_default();
        let mut product = serde_json::to_value(ProductSettings::default())
            .map_err(D::Error::custom)?
            .as_object()
            .cloned()
            .ok_or_else(|| D::Error::custom("product settings must be an object"))?;
        product.extend(fields);
        let product = serde_json::from_value(Value::Object(product)).map_err(D::Error::custom)?;
        let settings = Self {
            enabled,
            allowed_principals,
            source_verifiers,
            product,
        };
        settings.validate().map_err(D::Error::custom)?;
        Ok(settings)
    }
}

impl Settings {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_principals(&self.allowed_principals)?;
        validate_principals(&self.source_verifiers)?;
        self.product.validate()
    }

    pub fn allows(&self, caller: &str) -> bool {
        self.enabled
            && self
                .allowed_principals
                .iter()
                .any(|principal| principal == caller)
    }
}

fn validate_principals(principals: &[String]) -> Result<(), &'static str> {
    if principals.len() > 16 {
        return Err("too many trusted principals");
    }
    let mut unique = BTreeSet::new();
    for principal in principals {
        if principal.is_empty()
            || principal.len() > 128
            || !principal.is_ascii()
            || principal
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
            || !unique.insert(principal)
        {
            return Err("invalid trusted principal");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn trusted_startup_defaults_and_sparse_product_overrides_are_explicit() {
        let settings: Settings = serde_json::from_value(json!({
            "allowed_principals":["amaterasu.nvhttp"],
            "source_verifiers":["amaterasu.session-lifecycle"],
            "remote_monitor_mute_audio":true,
            "max_clients":4
        }))
        .unwrap();
        assert!(settings.enabled);
        assert!(settings.product.remote_monitor_mute_audio);
        assert!(settings.product.remote_monitor_confirm_app_replacement);
        assert_eq!(settings.product.max_clients, 4);
        assert!(settings.allows("amaterasu.nvhttp"));
        assert!(!settings.allows("amaterasu.management"));
        assert!(!Settings::default().allows("amaterasu.nvhttp"));
        assert_eq!(settings.source_verifiers, ["amaterasu.session-lifecycle"]);
        assert!(Settings::default().source_verifiers.is_empty());
    }

    #[test]
    fn public_replacements_cannot_change_grants_or_omit_preferences() {
        let base = serde_json::to_value(ProductSettings::default()).unwrap();
        for key in [
            "enabled",
            "allowed_principals",
            "source_verifiers",
            "unknown",
        ] {
            let mut value = base.clone();
            value[key] = json!(false);
            assert!(serde_json::from_value::<ProductSettings>(value).is_err());
        }
        let mut incomplete = base;
        incomplete
            .as_object_mut()
            .unwrap()
            .remove("show_remote_input");
        assert!(serde_json::from_value::<ProductSettings>(incomplete).is_err());
    }

    #[test]
    fn startup_rejects_unknown_fields_invalid_principals_and_capacity() {
        for value in [
            json!({"enabled":null}),
            json!({"unknown":true}),
            json!({"allowed_principals":[" "]}),
            json!({"allowed_principals":["peer","peer"]}),
            json!({"source_verifiers":["peer","peer"]}),
            json!({"source_verifiers":[" "]}),
            json!({"max_clients":0}),
            json!({"max_clients":33}),
            json!({"max_clients":1.5}),
        ] {
            assert!(serde_json::from_value::<Settings>(value).is_err());
        }
    }

    #[test]
    fn legacy_preferences_gain_an_empty_layout_without_relaxing_required_fields() {
        let mut legacy = serde_json::to_value(ProductSettings::default()).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("remote_monitor_layout");
        let decoded: ProductSettings = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(decoded.remote_monitor_layout, empty_layout());
        legacy.as_object_mut().unwrap().remove("max_clients");
        assert!(serde_json::from_value::<ProductSettings>(legacy).is_err());
    }

    #[test]
    fn layout_bounds_reject_invalid_documents_before_persistence() {
        let mut settings = ProductSettings::default();
        for layout in [
            Value::Null,
            json!({"version":2,"placements":{}}),
            json!({"version":1,"placements":[]}),
            json!({"version":1,"placements":{"client":{"value":"x".repeat(12_000)}}}),
            json!({"version":1,"placements":(0..33).map(|id| (id.to_string(),json!({}))).collect::<serde_json::Map<String,Value>>()}),
        ] {
            settings.remote_monitor_layout = layout;
            assert!(settings.validate().is_err());
        }
        settings.remote_monitor_layout = json!({"version":1,"placements":{"client":{"anchor_kind":"automatic","anchor_id":"","edge":"right","alignment":"center","gap_px":0}}});
        settings.validate().unwrap();
        settings.remote_monitor_layout = json!({"version":1,"placements":(0..32).map(|id| (format!("client-{id:058}"), json!({"anchor_kind":"physical","anchor_id":"a".repeat(64),"edge":"right","alignment":"center","gap_px":0}))).collect::<serde_json::Map<String,Value>>()});
        assert!(
            serde_json::to_vec(&settings.remote_monitor_layout)
                .unwrap()
                .len()
                > 4096
        );
        settings.validate().unwrap();
    }
}
