//! Saved relative placement for client-owned displays.
//!
//! The planner consumes an observed topology and changes only the requested
//! target. The display provider owns native application and readback; a plan is
//! never evidence that a display moved or that a topology reservation was held.
use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

const MAX_PLACEMENTS: usize = 64;
const MAX_DISPLAYS: usize = 16;
const MAX_LAYOUT_BYTES: usize = 16 * 1024;
const MAX_GAP: u32 = 10_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    pub version: u32,
    #[serde(deserialize_with = "unique_placements")]
    pub placements: BTreeMap<String, Placement>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            version: 1,
            placements: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    pub anchor_kind: AnchorKind,
    pub anchor_id: String,
    pub edge: Edge,
    pub alignment: Alignment,
    pub gap_px: u32,
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub preserve: bool,
    #[serde(default)]
    pub automatic_fallback: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorKind {
    Physical,
    Client,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    Left,
    Right,
    Above,
    Below,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Alignment {
    Start,
    Center,
    End,
}

/// Device rotation affects the desktop footprint, not the capture mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rotation {
    #[default]
    Identity,
    Rotate90,
    Rotate180,
    Rotate270,
}

/// Exact scale from the provider's observation, without floating-point rounding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scale {
    pub numerator: u32,
    pub denominator: u32,
}

impl Default for Scale {
    fn default() -> Self {
        Self {
            numerator: 1,
            denominator: 1,
        }
    }
}

/// A provider-observed active display and its unchanged configuration entry.
/// `client_id` is absent for a physical anchor and is the authenticated owner for
/// a managed output. A friendly label must never be used as the identity.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayNode {
    pub id: String,
    pub client_id: Option<String>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: Scale,
    pub rotation: Rotation,
    /// Prefer directly observed logical dimensions when the provider has them.
    pub logical_size: Option<(u32, u32)>,
    pub configuration: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissingAnchor {
    /// Used by a normal game sharing an already arranged desktop.
    Preserve,
    /// Used when a newly created remote monitor has no live saved anchor.
    AppendRight,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DisplayApplyPlan {
    pub configuration: Value,
    pub expected_topology_revision: String,
    pub target_id: String,
    pub primary_requested: bool,
    pub placement_applied: bool,
    pub anchor_unavailable: bool,
}

impl DisplayApplyPlan {
    /// An ordinary public display operation. The caller must retain the returned
    /// lease, check completion and verify the full observed topology afterward.
    pub fn operation(&self, operation_id: &str) -> Result<Value, String> {
        validate_identifier(operation_id, 128)?;
        let mut operation = json!({
            "action":"apply",
            "configuration":self.configuration,
            "expected_topology_revision":self.expected_topology_revision,
            "operation_id":operation_id,
        });
        if self.primary_requested {
            operation["mode_policy"] = json!({
                "display_id":self.target_id,
                "configuration_option":"ensure_primary",
                "remapping":{"mixed":[],"resolution_only":[],"refresh_rate_only":[]},
                "context":{
                    "resolution_is_automatic":false,
                    "refresh_rate_is_automatic":false,
                    "client_display_mode_override":false,
                    "optimize_game_settings":false,
                },
            });
        }
        Ok(operation)
    }
}

impl Layout {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_LAYOUT_BYTES {
            return Err("layout_too_large".into());
        }
        let mut layout: Self =
            serde_json::from_slice(bytes).map_err(|_| "invalid_layout".to_owned())?;
        layout.validate_shape()?;
        for placement in layout.placements.values_mut() {
            // Retained editor metadata must not override a newly chosen anchor.
            if !placement.is_automatic() {
                placement.automatic_fallback = false;
            }
        }
        Ok(layout)
    }

    fn validate_shape(&self) -> Result<(), String> {
        if self.version != 1 || self.placements.len() > MAX_PLACEMENTS {
            return Err("unsupported_layout".into());
        }
        for (client, placement) in &self.placements {
            validate_identifier(client, 128)?;
            if placement.gap_px > MAX_GAP
                || (!placement.preserve && placement.anchor_id.is_empty())
                || placement.anchor_id.len() > 4096
                || placement.anchor_id.chars().any(char::is_control)
            {
                return Err("invalid_layout_placement".into());
            }
        }
        Ok(())
    }

    /// Validate a saved edit against the currently paired clients and the
    /// physical inventory. Missing live anchors during activation are handled by
    /// `plan_target`; editing a rule to an unknown identity is rejected here.
    pub fn validate(
        &self,
        known_clients: &BTreeSet<String>,
        physical_ids: &BTreeSet<String>,
    ) -> Result<(), String> {
        self.validate_shape()?;
        let mut primary_count = 0;
        for (client, placement) in &self.placements {
            if !known_clients.contains(client) {
                return Err("unknown_layout_client".into());
            }
            if placement.is_manual() {
                continue;
            }
            primary_count += usize::from(placement.primary);
            if primary_count > 1 {
                return Err("multiple_primary_displays".into());
            }
            if placement.is_automatic() {
                continue;
            }
            let known_anchor = match placement.anchor_kind {
                AnchorKind::Physical => physical_ids.contains(&placement.anchor_id),
                AnchorKind::Client => {
                    placement.anchor_id != *client && known_clients.contains(&placement.anchor_id)
                }
            };
            if !known_anchor {
                return Err("unknown_layout_anchor".into());
            }
        }
        // Each node has at most one outgoing edge. A bounded walk detects both
        // direct and indirect cycles without recursion or map-order dependence.
        for client in self.placements.keys() {
            let mut visited = BTreeSet::new();
            let mut current = client.as_str();
            while let Some(placement) = self.placements.get(current) {
                if placement.is_manual()
                    || placement.is_automatic()
                    || placement.anchor_kind != AnchorKind::Client
                {
                    break;
                }
                if !visited.insert(current) {
                    return Err("layout_anchor_cycle".into());
                }
                current = &placement.anchor_id;
            }
        }
        Ok(())
    }

    /// Remove only the departing client's rule. Direct dependents keep their
    /// independent primary preference and become automatic; transitive rules
    /// retain their valid intermediate client anchor.
    pub fn remove_client(&mut self, client_id: &str) {
        self.placements.remove(client_id);
        for placement in self.placements.values_mut() {
            if placement.anchor_kind == AnchorKind::Client && placement.anchor_id == client_id {
                placement.anchor_kind = AnchorKind::Physical;
                placement.anchor_id.clear();
                if !placement.preserve {
                    placement.preserve = true;
                    placement.automatic_fallback = true;
                }
            }
        }
    }

    /// Apply one client's saved rule to an immutable observed peer topology.
    /// The complete baseline is preserved, including output modes and HDR.
    pub fn plan_target(
        &self,
        client_id: &str,
        target: &DisplayNode,
        baseline: &[DisplayNode],
        expected_topology_revision: &str,
        missing_anchor: MissingAnchor,
    ) -> Result<DisplayApplyPlan, String> {
        self.validate_shape()?;
        validate_identifier(client_id, 128)?;
        validate_identifier(expected_topology_revision, 128)?;
        target.validate()?;
        if target.client_id.as_deref() != Some(client_id) {
            return Err("layout_target_owner_mismatch".into());
        }
        if baseline.len() > MAX_DISPLAYS {
            return Err("display_limit".into());
        }
        let mut ids = BTreeSet::new();
        let mut clients = BTreeSet::new();
        for node in baseline {
            node.validate()?;
            if !ids.insert(node.id.as_str())
                || node
                    .client_id
                    .as_deref()
                    .is_some_and(|client| !clients.insert(client))
            {
                return Err("ambiguous_layout_inventory".into());
            }
            if node.client_id.as_deref() == Some(client_id) && node.id != target.id {
                return Err("layout_target_identity_changed".into());
            }
        }
        let placement = self.placements.get(client_id);
        let mut coordinates = (target.x, target.y);
        let mut applied = false;
        let mut unavailable = false;
        let mut primary_requested = false;
        if let Some(placement) = placement.filter(|placement| !placement.is_manual()) {
            primary_requested = placement.primary;
            let anchor = baseline.iter().find(|node| match placement.anchor_kind {
                AnchorKind::Physical => node.client_id.is_none() && node.id == placement.anchor_id,
                AnchorKind::Client => {
                    node.client_id.as_deref() == Some(placement.anchor_id.as_str())
                }
            });
            if let Some(anchor) = anchor.filter(|anchor| anchor.id != target.id) {
                coordinates = relative_coordinates(target, anchor, placement)?;
                applied = true;
            } else {
                unavailable = !placement.is_automatic();
                if missing_anchor == MissingAnchor::AppendRight {
                    coordinates = append_right(target, baseline)?;
                    applied = true;
                }
            }
        } else if placement.is_none() && missing_anchor == MissingAnchor::AppendRight {
            coordinates = append_right(target, baseline)?;
            applied = true;
        }
        let mut target_configuration = target.configuration.clone();
        target_configuration["x"] = json!(coordinates.0);
        target_configuration["y"] = json!(coordinates.1);
        let mut displays: Vec<Value> = baseline
            .iter()
            .map(|node| {
                if node.id == target.id {
                    target_configuration.clone()
                } else {
                    node.configuration.clone()
                }
            })
            .collect();
        if !ids.contains(target.id.as_str()) {
            displays.push(target_configuration);
        }
        if displays.len() > MAX_DISPLAYS {
            return Err("display_limit".into());
        }
        Ok(DisplayApplyPlan {
            configuration: json!({"displays":displays}),
            expected_topology_revision: expected_topology_revision.into(),
            target_id: target.id.clone(),
            primary_requested,
            placement_applied: applied,
            anchor_unavailable: unavailable,
        })
    }
}

impl Placement {
    fn is_automatic(&self) -> bool {
        self.preserve
            && self.automatic_fallback
            && self.anchor_kind == AnchorKind::Physical
            && self.anchor_id.is_empty()
    }

    fn is_manual(&self) -> bool {
        self.preserve && !self.is_automatic()
    }
}

impl DisplayNode {
    fn validate(&self) -> Result<(), String> {
        validate_identifier(&self.id, 4096)?;
        if let Some(client) = &self.client_id {
            validate_identifier(client, 128)?;
        }
        self.logical_dimensions()?;
        if !self.configuration.is_object()
            || self.configuration["id"] != self.id
            || self.configuration["width"] != self.width
            || self.configuration["height"] != self.height
            || self.configuration["x"] != self.x
            || self.configuration["y"] != self.y
        {
            return Err("invalid_display_configuration".into());
        }
        Ok(())
    }

    pub fn logical_dimensions(&self) -> Result<(i64, i64), String> {
        if self.width == 0
            || self.height == 0
            || self.width > 1_000_000
            || self.height > 1_000_000
            || self.scale.numerator == 0
            || self.scale.denominator == 0
        {
            return Err("invalid_display_geometry".into());
        }
        if let Some((width, height)) = self.logical_size {
            if width == 0 || height == 0 || width > 1_000_000 || height > 1_000_000 {
                return Err("invalid_display_geometry".into());
            }
            return Ok((i64::from(width), i64::from(height)));
        }
        let (width, height) = if matches!(self.rotation, Rotation::Rotate90 | Rotation::Rotate270) {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        };
        let scale = |pixels: u32| {
            // Round the footprint outward, so fractional scaling cannot overlap
            // the next output by truncating its final logical pixel.
            let numerator = u64::from(pixels) * u64::from(self.scale.denominator);
            numerator.div_ceil(u64::from(self.scale.numerator))
        };
        let (width, height) = (scale(width), scale(height));
        if width == 0 || height == 0 || width > 1_000_000 || height > 1_000_000 {
            return Err("invalid_display_geometry".into());
        }
        Ok((width as i64, height as i64))
    }
}

fn relative_coordinates(
    target: &DisplayNode,
    anchor: &DisplayNode,
    placement: &Placement,
) -> Result<(i32, i32), String> {
    let (target_width, target_height) = target.logical_dimensions()?;
    let (anchor_width, anchor_height) = anchor.logical_dimensions()?;
    let (anchor_x, anchor_y) = (i64::from(anchor.x), i64::from(anchor.y));
    let gap = i64::from(placement.gap_px);
    let aligned = |origin: i64, anchor_size: i64, target_size: i64| match placement.alignment {
        Alignment::Start => origin,
        Alignment::Center => origin + (anchor_size - target_size) / 2,
        Alignment::End => origin + anchor_size - target_size,
    };
    let (x, y) = match placement.edge {
        Edge::Left => (
            anchor_x - target_width - gap,
            aligned(anchor_y, anchor_height, target_height),
        ),
        Edge::Right => (
            anchor_x + anchor_width + gap,
            aligned(anchor_y, anchor_height, target_height),
        ),
        Edge::Above => (
            aligned(anchor_x, anchor_width, target_width),
            anchor_y - target_height - gap,
        ),
        Edge::Below => (
            aligned(anchor_x, anchor_width, target_width),
            anchor_y + anchor_height + gap,
        ),
    };
    checked_position(x, y)
}

fn append_right(target: &DisplayNode, baseline: &[DisplayNode]) -> Result<(i32, i32), String> {
    let mut right = None;
    for node in baseline.iter().filter(|node| node.id != target.id) {
        let (width, _) = node.logical_dimensions()?;
        let edge = i64::from(node.x) + width;
        right = Some(right.map_or(edge, |previous: i64| previous.max(edge)));
    }
    checked_position(right.unwrap_or(i64::from(target.x)), i64::from(target.y))
}

fn checked_position(x: i64, y: i64) -> Result<(i32, i32), String> {
    Ok((
        i32::try_from(x).map_err(|_| "display_coordinate_overflow")?,
        i32::try_from(y).map_err(|_| "display_coordinate_overflow")?,
    ))
}

fn validate_identifier(value: &str, max_bytes: usize) -> Result<(), String> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err("invalid_layout_identity".into());
    }
    Ok(())
}

fn unique_placements<'de, D>(deserializer: D) -> Result<BTreeMap<String, Placement>, D::Error>
where
    D: Deserializer<'de>,
{
    struct PlacementsVisitor;
    impl<'de> Visitor<'de> for PlacementsVisitor {
        type Value = BTreeMap<String, Placement>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a bounded object of unique client placements")
        }

        fn visit_map<M>(self, mut entries: M) -> Result<Self::Value, M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut placements = BTreeMap::new();
            while let Some((client, placement)) = entries.next_entry::<String, Placement>()? {
                if placements.len() == MAX_PLACEMENTS
                    || placements.insert(client, placement).is_some()
                {
                    return Err(M::Error::custom("duplicate or excessive layout placements"));
                }
            }
            Ok(placements)
        }
    }
    deserializer.deserialize_map(PlacementsVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(
        id: &str,
        client: Option<&str>,
        width: u32,
        height: u32,
        x: i32,
        y: i32,
    ) -> DisplayNode {
        DisplayNode {
            id: id.into(),
            client_id: client.map(str::to_owned),
            x,
            y,
            width,
            height,
            scale: Scale::default(),
            rotation: Rotation::Identity,
            logical_size: None,
            configuration: json!({
                "id":id,"width":width,"height":height,"x":x,"y":y,
                "refresh":{"numerator":60000,"denominator":1000},
                "rotation":"identity","hdr_enabled":true,
            }),
        }
    }

    fn placement(anchor_kind: AnchorKind, anchor_id: &str, edge: Edge) -> Placement {
        Placement {
            anchor_kind,
            anchor_id: anchor_id.into(),
            edge,
            alignment: Alignment::Center,
            gap_px: 0,
            primary: false,
            preserve: false,
            automatic_fallback: false,
        }
    }

    fn known(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn layout_edits_reject_cycles_unknown_anchors_and_multiple_primaries() {
        let mut layout = Layout::default();
        layout
            .placements
            .insert("a".into(), placement(AnchorKind::Client, "b", Edge::Right));
        layout
            .placements
            .insert("b".into(), placement(AnchorKind::Client, "a", Edge::Below));
        assert_eq!(
            layout.validate(&known(&["a", "b"]), &known(&["physical"])),
            Err("layout_anchor_cycle".into())
        );
        layout.placements.get_mut("b").unwrap().anchor_kind = AnchorKind::Physical;
        layout.placements.get_mut("b").unwrap().anchor_id = "missing".into();
        assert_eq!(
            layout.validate(&known(&["a", "b"]), &known(&["physical"])),
            Err("unknown_layout_anchor".into())
        );
        layout.placements.get_mut("b").unwrap().anchor_id = "physical".into();
        assert!(
            layout
                .validate(&known(&["a", "b"]), &known(&["physical"]))
                .is_ok()
        );
        for rule in layout.placements.values_mut() {
            rule.primary = true;
        }
        assert_eq!(
            layout.validate(&known(&["a", "b"]), &known(&["physical"])),
            Err("multiple_primary_displays".into())
        );
    }

    #[test]
    fn duplicate_keys_and_invalid_shapes_fail_before_planning() {
        let rule =
            serde_json::to_string(&placement(AnchorKind::Physical, "p", Edge::Right)).unwrap();
        let duplicate = format!("{{\"version\":1,\"placements\":{{\"a\":{rule},\"a\":{rule}}}}}");
        assert!(Layout::decode(duplicate.as_bytes()).is_err());
        for value in [
            json!({"version":2,"placements":{}}),
            json!({"version":1,"placements":{"a":{"anchor_kind":"physical","anchor_id":"p","edge":"right","alignment":"center","gap_px":-1}}}),
            json!({"version":1,"placements":{},"unknown":true}),
        ] {
            assert!(Layout::decode(&serde_json::to_vec(&value).unwrap()).is_err());
        }
    }

    #[test]
    fn logical_scale_and_rotation_place_one_target_without_touching_peers() {
        let mut physical = node("physical", None, 3840, 2160, -1920, 200);
        physical.scale = Scale {
            numerator: 2,
            denominator: 1,
        };
        let mut target = node("remote", Some("a"), 1920, 1080, 50, 60);
        target.rotation = Rotation::Rotate90;
        let peer = node("peer", Some("b"), 2560, 1440, 2500, -200);
        let baseline = vec![physical.clone(), peer.clone()];
        let layout = Layout {
            version: 1,
            placements: BTreeMap::from([(
                "a".into(),
                placement(AnchorKind::Physical, "physical", Edge::Right),
            )]),
        };
        let plan = layout
            .plan_target("a", &target, &baseline, "revision", MissingAnchor::Preserve)
            .unwrap();
        assert_eq!(plan.configuration["displays"][0], physical.configuration);
        assert_eq!(plan.configuration["displays"][1], peer.configuration);
        assert_eq!(plan.configuration["displays"][2]["x"], 0);
        assert_eq!(plan.configuration["displays"][2]["y"], -220);
        assert!(plan.placement_applied);
        assert!(!plan.anchor_unavailable);
    }

    #[test]
    fn each_edge_and_alignment_uses_live_client_anchor_geometry() {
        let anchor = node("peer-output", Some("b"), 2000, 1000, 100, 200);
        let target = node("target", Some("a"), 1000, 600, 0, 0);
        for (edge, alignment, expected) in [
            (Edge::Left, Alignment::Start, (-910, 200)),
            (Edge::Right, Alignment::End, (2110, 600)),
            (Edge::Above, Alignment::Center, (600, -410)),
            (Edge::Below, Alignment::End, (1100, 1210)),
        ] {
            let mut rule = placement(AnchorKind::Client, "b", edge);
            rule.alignment = alignment;
            rule.gap_px = 10;
            let layout = Layout {
                version: 1,
                placements: BTreeMap::from([("a".into(), rule)]),
            };
            let plan = layout
                .plan_target(
                    "a",
                    &target,
                    std::slice::from_ref(&anchor),
                    "revision",
                    MissingAnchor::Preserve,
                )
                .unwrap();
            assert_eq!(plan.configuration["displays"][1]["x"], expected.0);
            assert_eq!(plan.configuration["displays"][1]["y"], expected.1);
        }
    }

    #[test]
    fn missing_anchor_preserves_normal_position_and_appends_new_monitor_only() {
        let physical = node("physical", None, 2000, 1000, -500, 300);
        let target = node("remote", Some("a"), 1000, 600, -3000, -400);
        let layout = Layout {
            version: 1,
            placements: BTreeMap::from([(
                "a".into(),
                placement(AnchorKind::Client, "missing", Edge::Right),
            )]),
        };
        let kept = layout
            .plan_target(
                "a",
                &target,
                std::slice::from_ref(&physical),
                "r",
                MissingAnchor::Preserve,
            )
            .unwrap();
        assert_eq!(kept.configuration["displays"][1]["x"], -3000);
        assert_eq!(kept.configuration["displays"][1]["y"], -400);
        assert!(!kept.placement_applied);
        assert!(kept.anchor_unavailable);
        let appended = layout
            .plan_target(
                "a",
                &target,
                std::slice::from_ref(&physical),
                "r",
                MissingAnchor::AppendRight,
            )
            .unwrap();
        assert_eq!(
            appended.configuration["displays"][0],
            physical.configuration
        );
        assert_eq!(appended.configuration["displays"][1]["x"], 1500);
        assert_eq!(appended.configuration["displays"][1]["y"], -400);
    }

    #[test]
    fn manual_rules_preserve_position_and_unpair_detaches_only_direct_dependents() {
        let mut rule = placement(AnchorKind::Client, "a", Edge::Right);
        rule.primary = true;
        let mut layout = Layout {
            version: 1,
            placements: BTreeMap::from([
                (
                    "a".into(),
                    placement(AnchorKind::Physical, "physical", Edge::Right),
                ),
                ("b".into(), rule),
                ("c".into(), placement(AnchorKind::Client, "b", Edge::Below)),
            ]),
        };
        layout.remove_client("a");
        assert!(!layout.placements.contains_key("a"));
        assert!(layout.placements["b"].is_automatic());
        assert!(layout.placements["b"].primary);
        assert_eq!(layout.placements["c"].anchor_id, "b");
        assert!(
            layout
                .validate(&known(&["b", "c"]), &BTreeSet::new())
                .is_ok()
        );
        let mut manual = placement(AnchorKind::Physical, "", Edge::Right);
        manual.preserve = true;
        manual.primary = true;
        layout.placements.insert("b".into(), manual);
        let target = node("target", Some("b"), 1000, 600, -20, 40);
        let plan = layout
            .plan_target("b", &target, &[], "r", MissingAnchor::AppendRight)
            .unwrap();
        assert_eq!(plan.configuration["displays"][0]["x"], -20);
        assert!(!plan.primary_requested);
    }

    #[test]
    fn stale_automatic_marker_cannot_override_an_explicit_new_anchor() {
        let bytes = br#"{"version":1,"placements":{"a":{"anchor_kind":"physical","anchor_id":"new","edge":"right","alignment":"center","gap_px":0,"preserve":true,"automatic_fallback":true}}}"#;
        let layout = Layout::decode(bytes).unwrap();
        assert!(!layout.placements["a"].automatic_fallback);
        assert!(layout.placements["a"].is_manual());
    }

    #[test]
    fn invalid_inventory_ownership_and_coordinate_overflow_never_produce_commands() {
        let target = node("target", Some("a"), 1000, 600, 0, 0);
        let physical = node("physical", None, 2000, 1000, i32::MAX - 10, 0);
        let layout = Layout {
            version: 1,
            placements: BTreeMap::from([(
                "a".into(),
                placement(AnchorKind::Physical, "physical", Edge::Right),
            )]),
        };
        assert_eq!(
            layout
                .plan_target(
                    "a",
                    &target,
                    std::slice::from_ref(&physical),
                    "r",
                    MissingAnchor::Preserve
                )
                .unwrap_err(),
            "display_coordinate_overflow"
        );
        assert!(
            layout
                .plan_target("b", &target, &[], "r", MissingAnchor::Preserve)
                .is_err()
        );
        assert!(
            layout
                .plan_target(
                    "a",
                    &target,
                    &[physical.clone(), physical],
                    "r",
                    MissingAnchor::Preserve
                )
                .is_err()
        );
    }

    #[test]
    fn operations_retain_snapshot_revision_and_explicit_primary_intent() {
        let mut rule = placement(AnchorKind::Physical, "p", Edge::Below);
        rule.primary = true;
        let layout = Layout {
            version: 1,
            placements: BTreeMap::from([("a".into(), rule)]),
        };
        let target = node("t", Some("a"), 1000, 600, 0, 0);
        let anchor = node("p", None, 2000, 1000, 0, 0);
        let plan = layout
            .plan_target(
                "a",
                &target,
                &[anchor],
                "observed-revision",
                MissingAnchor::Preserve,
            )
            .unwrap();
        let operation = plan.operation("layout-operation").unwrap();
        assert_eq!(operation["expected_topology_revision"], "observed-revision");
        assert_eq!(
            operation["mode_policy"]["configuration_option"],
            "ensure_primary"
        );
        assert_eq!(operation["mode_policy"]["display_id"], "t");
        assert!(plan.operation("bad\noperation").is_err());
    }
}
