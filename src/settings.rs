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
        }
    }
}

impl ProductSettings {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(1..=32).contains(&self.max_clients) {
            return Err("max_clients must be between 1 and 32");
        }
        Ok(())
    }
}

/// This type can be read only from trusted composition. Public settings writes
/// deserialize `ProductSettings`, which has no fields capable of changing grants.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub allowed_principals: Vec<String>,
    pub product: ProductSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_principals: Vec::new(),
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
            product,
        };
        settings.validate().map_err(D::Error::custom)?;
        Ok(settings)
    }
}

impl Settings {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.allowed_principals.len() > 16 {
            return Err("too many allowed principals");
        }
        let mut unique = BTreeSet::new();
        for principal in &self.allowed_principals {
            if principal.is_empty()
                || principal.len() > 128
                || !principal.is_ascii()
                || principal
                    .bytes()
                    .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
                || !unique.insert(principal)
            {
                return Err("invalid allowed principal");
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn trusted_startup_defaults_and_sparse_product_overrides_are_explicit() {
        let settings: Settings = serde_json::from_value(json!({
            "allowed_principals":["amaterasu.nvhttp"],
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
    }

    #[test]
    fn public_replacements_cannot_change_grants_or_omit_preferences() {
        let base = serde_json::to_value(ProductSettings::default()).unwrap();
        for key in ["enabled", "allowed_principals", "unknown"] {
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
            json!({"max_clients":0}),
            json!({"max_clients":33}),
            json!({"max_clients":1.5}),
        ] {
            assert!(serde_json::from_value::<Settings>(value).is_err());
        }
    }
}
