//! Build target-only public display operations from verified native observations.
use super::layout::{AnchorKind, DisplayNode, Layout, MissingAnchor, Rotation, Scale};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub(super) struct Composition {
    pub payload: Value,
    pub needs_hdr_followup: bool,
    pub physical_anchors: Vec<Value>,
}

pub(super) struct TargetRequest<'a> {
    pub client: &'a str,
    pub identity: &'a str,
    pub reference: &'a str,
    pub target_id: &'a str,
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
    pub hdr: bool,
    pub layout: &'a Layout,
    pub known_targets: &'a BTreeMap<String, String>,
}

pub(super) fn prepare(
    target: TargetRequest<'_>,
    snapshot: Value,
    operation_id: &str,
    release: bool,
) -> Result<Composition, String> {
    if snapshot["ok"] != true {
        return Err("display_snapshot_unavailable".into());
    }
    let revision = snapshot["topology_revision"]
        .as_str()
        .filter(|revision| {
            !revision.is_empty() && revision.len() <= 128 && !revision.chars().any(char::is_control)
        })
        .ok_or("invalid_display_snapshot")?;
    if release {
        return Ok(Composition {
            payload: json!({
                "action":"scoped_release","operation_id":operation_id,
                "identity":target.identity,"reference_id":target.reference,
                "expected_topology_revision":revision,
            }),
            needs_hdr_followup: false,
            physical_anchors: Vec::new(),
        });
    }
    let displays = snapshot["configuration"]["displays"]
        .as_array()
        .filter(|displays| displays.len() <= 16)
        .ok_or("invalid_display_snapshot")?;
    let baseline = displays
        .iter()
        .map(|display| node(display, target.known_targets))
        .collect::<Result<Vec<_>, _>>()?;
    let physical_anchors = baseline.iter().filter(|display| display.client_id.is_none()).enumerate().map(|(index,display)| json!({
        "id":physical_alias(&display.id),"label":format!("Display {} ({}x{})",index+1,display.width,display.height),
    })).collect::<Vec<_>>();
    let mut layout = target.layout.clone();
    for placement in layout.placements.values_mut().filter(|placement| {
        placement.anchor_kind == AnchorKind::Physical && !placement.anchor_id.is_empty()
    }) {
        if let Some(anchor) = baseline.iter().find(|display| {
            display.client_id.is_none() && physical_alias(&display.id) == placement.anchor_id
        }) {
            placement.anchor_id = anchor.id.clone();
        }
    }
    let existing = baseline
        .iter()
        .find(|display| display.id == target.target_id);
    let needs_hdr_followup = existing.is_none() && target.hdr;
    let setting = json!({
        "id":target.target_id,"width":target.width,"height":target.height,
        "refresh":{"numerator":target.refresh_millihz,"denominator":1000},
        "x":existing.map_or(0,|display| display.x),
        "y":existing.map_or(0,|display| display.y),
        "rotation":"identity","hdr_enabled":target.hdr && !needs_hdr_followup,
    });
    let mut requested = node(&setting, target.known_targets)?;
    requested.client_id = Some(target.client.into());
    if let Some(existing) = existing {
        requested.scale = existing.scale;
        requested.logical_size = existing.logical_size;
    }
    let planned = layout.plan_target(
        target.client,
        &requested,
        &baseline,
        revision,
        if existing.is_some() {
            MissingAnchor::Preserve
        } else {
            MissingAnchor::AppendRight
        },
    )?;
    let selected = planned.configuration["displays"]
        .as_array()
        .and_then(|displays| {
            displays
                .iter()
                .find(|display| display["id"] == target.target_id)
        })
        .ok_or("planned_target_missing")?;
    let payload = json!({
        "action":"scoped_apply","operation_id":operation_id,
        "identity":target.identity,"reference_id":target.reference,
        "target":selected,"expected_topology_revision":revision,
        "primary":planned.primary_requested,
    });
    Ok(Composition {
        payload,
        needs_hdr_followup,
        physical_anchors,
    })
}

fn physical_alias(id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"amaterasu.remote-monitor.physical-anchor.v1\0");
    hash.update(id.as_bytes());
    format!("physical-{:x}", hash.finalize())
}

fn node(
    configuration: &Value,
    known_targets: &BTreeMap<String, String>,
) -> Result<DisplayNode, String> {
    let id = configuration["id"]
        .as_str()
        .ok_or("invalid_display_snapshot")?;
    let number = |name: &str| {
        configuration[name]
            .as_u64()
            .and_then(|number| u32::try_from(number).ok())
            .filter(|number| *number > 0)
            .ok_or("invalid_display_snapshot".to_owned())
    };
    let position = |name: &str| {
        configuration[name]
            .as_i64()
            .and_then(|number| i32::try_from(number).ok())
            .ok_or("invalid_display_snapshot".to_owned())
    };
    let scale = match configuration.get("scale") {
        None => Scale::default(),
        Some(scale) => Scale {
            numerator: scale["numerator"]
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 0)
                .ok_or("invalid_display_scale")?,
            denominator: scale["denominator"]
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 0)
                .ok_or("invalid_display_scale")?,
        },
    };
    let logical_size = match (
        configuration.get("logical_width"),
        configuration.get("logical_height"),
    ) {
        (None, None) => None,
        (Some(_), Some(_)) => Some((number("logical_width")?, number("logical_height")?)),
        _ => return Err("invalid_logical_display_size".into()),
    };
    let rotation = match configuration["rotation"].as_str().unwrap_or("identity") {
        "identity" => Rotation::Identity,
        "rotate90" => Rotation::Rotate90,
        "rotate180" => Rotation::Rotate180,
        "rotate270" => Rotation::Rotate270,
        _ => return Err("invalid_display_rotation".into()),
    };
    Ok(DisplayNode {
        id: id.into(),
        client_id: known_targets.get(id).cloned(),
        x: position("x")?,
        y: position("y")?,
        width: number("width")?,
        height: number("height")?,
        scale,
        rotation,
        logical_size,
        configuration: configuration.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Value {
        json!({"ok":true,"topology_revision":"0123456789abcdef0123456789abcdef","configuration":{"displays":[{
            "id":"private-physical-path","width":1920,"height":1080,"x":0,"y":0,
            "rotation":"identity","refresh":{"numerator":60000,"denominator":1000},"hdr_enabled":false,
        }]}})
    }

    #[test]
    fn hdr_activation_is_split_from_color_change_and_public_anchors_are_opaque() {
        let layout = Layout::default();
        let known = BTreeMap::from([("private-owned-path".into(), "alice".into())]);
        let target = || TargetRequest {
            client: "alice",
            identity: "identity",
            reference: "source-reference",
            target_id: "private-owned-path",
            width: 1280,
            height: 720,
            refresh_millihz: 120000,
            hdr: true,
            layout: &layout,
            known_targets: &known,
        };
        let first = prepare(target(), snapshot(), "first", false).unwrap();
        assert!(first.needs_hdr_followup);
        assert_eq!(first.payload["target"]["hdr_enabled"], false);
        assert_eq!(first.payload["target"]["x"], 1920);
        assert_eq!(first.physical_anchors.len(), 1);
        assert!(
            first.physical_anchors[0]["id"]
                .as_str()
                .unwrap()
                .starts_with("physical-")
        );
        assert!(
            !serde_json::to_string(&first.physical_anchors)
                .unwrap()
                .contains("private-physical-path")
        );
        let mut active = snapshot();
        active["configuration"]["displays"]
            .as_array_mut()
            .unwrap()
            .push(first.payload["target"].clone());
        let second = prepare(target(), active, "second", false).unwrap();
        assert!(!second.needs_hdr_followup);
        assert_eq!(second.payload["target"]["hdr_enabled"], true);
    }

    #[test]
    fn releasing_composition_never_removes_or_restores_native_displays() {
        let layout = Layout::default();
        let known = BTreeMap::new();
        let target = TargetRequest {
            client: "alice",
            identity: "identity",
            reference: "source-reference",
            target_id: "private-owned-path",
            width: 1280,
            height: 720,
            refresh_millihz: 120000,
            hdr: false,
            layout: &layout,
            known_targets: &known,
        };
        let release = prepare(target, snapshot(), "release", true).unwrap();
        assert_eq!(release.payload["action"], "scoped_release");
        assert!(release.payload.get("target").is_none());
        assert_eq!(release.payload["reference_id"], "source-reference");
        assert_eq!(
            release.payload["expected_topology_revision"],
            "0123456789abcdef0123456789abcdef"
        );
    }
}
