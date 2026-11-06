//! Moonlight compatibility identities owned by this provider, not by consumers.
use amaterasu_module_sdk::session_presentation::{Application, Selection};
use std::collections::BTreeSet;

const UUID_PREFIX: &str = "9a1c5a25-58fe-40e0-b9aa-7d3f0000000";
const MAX_WIRE_ID: u32 = i32::MAX as u32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Control {
    Resume,
    DisconnectMonitor,
    DisconnectInput,
    Terminate,
    Monitor,
    Input,
    RunningGame,
}

impl Control {
    fn ordinal(self) -> u32 {
        match self {
            Self::Resume => 1,
            Self::DisconnectMonitor => 2,
            Self::DisconnectInput => 3,
            Self::Terminate => 4,
            Self::Monitor => 5,
            Self::Input => 6,
            Self::RunningGame => 7,
        }
    }

    pub(crate) fn id(self, secondary: bool) -> String {
        let suffix = self.ordinal();
        let secondary = secondary
            && matches!(
                self,
                Self::Resume | Self::Terminate | Self::Monitor | Self::Input
            );
        (2_147_483_500_u32 + suffix + if secondary { 10 } else { 0 }).to_string()
    }

    pub(crate) fn uuid(self) -> String {
        format!("{UUID_PREFIX}{}", self.ordinal())
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Resume => "Resume",
            Self::DisconnectMonitor => "Disconnect Monitor",
            Self::DisconnectInput => "Disconnect Input",
            Self::Terminate => "Terminate",
            Self::Monitor => "Remote Monitor",
            Self::Input => "Remote Input",
            Self::RunningGame => "Running game",
        }
    }

    pub(crate) fn application(self, secondary: bool) -> Application {
        let rank = if secondary {
            match self {
                Self::Monitor => 4,
                Self::Input => 3,
                Self::Resume => 2,
                Self::Terminate => 1,
                _ => 0,
            }
        } else {
            0
        };
        Application {
            id: self.id(secondary),
            uuid: self.uuid(),
            title: format!("{}{}", " ".repeat(rank), self.title()),
            aliases: Vec::new(),
            visible: true,
            monitor_profile: false,
            properties: serde_json::json!({}),
        }
    }
}

const CONTROLS: [Control; 7] = [
    Control::Resume,
    Control::DisconnectMonitor,
    Control::DisconnectInput,
    Control::Terminate,
    Control::Monitor,
    Control::Input,
    Control::RunningGame,
];

/// Fixed numeric identities take precedence over an unrelated stale UUID.
pub(crate) fn identify(selection: &Selection) -> Option<Control> {
    if let Ok(id) = selection.id.parse::<u32>() {
        for control in CONTROLS {
            let primary = 2_147_483_500 + control.ordinal();
            let legacy = 2_147_483_600 + control.ordinal();
            let secondary = primary + 10;
            if id == primary
                || (control != Control::RunningGame && id == legacy)
                || (matches!(
                    control,
                    Control::Resume | Control::Terminate | Control::Monitor | Control::Input
                ) && id == secondary)
            {
                return Some(control);
            }
        }
    }
    let uuid = selection.uuid.as_deref()?;
    CONTROLS
        .into_iter()
        .find(|control| control.uuid().eq_ignore_ascii_case(uuid))
}

pub(crate) fn reserved_name(title: &str) -> bool {
    ["Remote Input", "Remote Monitor", "Terminate"]
        .into_iter()
        .any(|reserved| {
            title
                .trim_start_matches([' ', '\t', '\r', '\n'])
                .eq_ignore_ascii_case(reserved)
        })
}

pub(crate) fn occupied(id: &str, applications: &[Application]) -> bool {
    applications.iter().any(|application| {
        application.id == id || application.aliases.iter().any(|alias| alias == id)
    })
}

pub(crate) fn valid_profile(application: &Application, applications: &[Application]) -> bool {
    let uuid = application.uuid.as_bytes();
    application.monitor_profile
        && uuid.len() == 36
        && uuid.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
        && uuid
            .iter()
            .any(|byte| matches!(byte, b'1'..=b'9' | b'a'..=b'f' | b'A'..=b'F'))
        && identify(&Selection {
            id: String::new(),
            uuid: Some(application.uuid.clone()),
        })
        .is_none()
        && applications
            .iter()
            .filter(|other| other.uuid.eq_ignore_ascii_case(&application.uuid))
            .count()
            == 1
}

/// Stable compatibility seed, with complete current/alias occupancy supplied by
/// the trusted catalog export. Hidden entries still reserve their identities.
pub(crate) fn running_tile(
    game: &Application,
    applications: &[Application],
) -> Result<Application, &'static str> {
    let source = game
        .id
        .parse::<u32>()
        .map_err(|_| "invalid_game_identity")?;
    if source == 0 || source > MAX_WIRE_ID {
        return Err("invalid_game_identity");
    }
    let occupied: BTreeSet<&str> = applications
        .iter()
        .flat_map(|application| {
            std::iter::once(application.id.as_str())
                .chain(application.aliases.iter().map(String::as_str))
        })
        .collect();
    let seed = source ^ (1 << 30);
    // At most one collision per configured identity and its aliases, plus the
    // fixed controls and source. This avoids iterating the numeric ID domain.
    let attempts = occupied.len().saturating_add(32);
    let id = (0..attempts)
        .map(|offset| {
            ((u64::from(seed) + offset as u64).wrapping_sub(1) % u64::from(MAX_WIRE_ID) + 1) as u32
        })
        .find(|candidate| {
            let id = candidate.to_string();
            *candidate != source
                && !occupied.contains(id.as_str())
                && identify(&Selection { id, uuid: None }).is_none()
        })
        .ok_or("synthetic_identity_exhausted")?;
    Ok(Application {
        id: id.to_string(),
        uuid: Control::RunningGame.uuid(),
        title: format!("     {}", game.title),
        aliases: Vec::new(),
        visible: true,
        monitor_profile: false,
        properties: serde_json::json!({}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, uuid: &str) -> Application {
        Application {
            id: id.into(),
            uuid: uuid.into(),
            title: "Game".into(),
            visible: true,
            aliases: Vec::new(),
            monitor_profile: false,
            properties: serde_json::json!({}),
        }
    }

    #[test]
    fn current_ranked_and_legacy_controls_cannot_be_reinterpreted_as_apps() {
        for control in CONTROLS {
            assert_eq!(
                identify(&Selection {
                    id: control.id(false),
                    uuid: Some("unrelated".into())
                }),
                Some(control)
            );
            assert_eq!(
                identify(&Selection {
                    id: "42".into(),
                    uuid: Some(control.uuid())
                }),
                Some(control)
            );
            if control != Control::RunningGame {
                assert_eq!(
                    identify(&Selection {
                        id: (2_147_483_600 + control.ordinal()).to_string(),
                        uuid: None
                    }),
                    Some(control)
                );
            }
        }
        assert_eq!(
            identify(&Selection {
                id: "2147483515".into(),
                uuid: None
            }),
            Some(Control::Monitor)
        );
        assert_eq!(
            identify(&Selection {
                id: "2147483512".into(),
                uuid: None
            }),
            None
        );
    }

    #[test]
    fn invisible_rows_and_aliases_reserve_running_tile_ids() {
        let game = app("42", "game");
        let first = running_tile(&game, &[]).unwrap();
        let mut hidden = app(&first.id, "hidden");
        hidden.visible = false;
        hidden.aliases = vec![(first.id.parse::<u32>().unwrap() + 1).to_string()];
        let tile = running_tile(&game, &[game.clone(), hidden.clone()]).unwrap();
        assert!(!occupied(&tile.id, &[game, hidden]));
        assert!(tile.title.starts_with("     "));
    }

    #[test]
    fn monitor_profiles_require_unique_non_nil_non_control_uuid() {
        let mut profile = app("7", "a1234567-1234-4234-8234-123456789abc");
        profile.monitor_profile = true;
        assert!(valid_profile(&profile, std::slice::from_ref(&profile)));
        assert!(!valid_profile(
            &profile,
            &[profile.clone(), profile.clone()]
        ));
        profile.uuid = Control::Monitor.uuid();
        assert!(!valid_profile(&profile, std::slice::from_ref(&profile)));
        profile.uuid = "00000000-0000-0000-0000-000000000000".into();
        assert!(!valid_profile(&profile, std::slice::from_ref(&profile)));
        assert!(reserved_name("   rEmOtE iNpUt"));
        assert!(!reserved_name("Remote Input copy"));
    }
}
