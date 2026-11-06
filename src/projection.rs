//! Provider-owned client views. A view never grants permission to launch.
use crate::identity::{Control, identify, reserved_name, running_tile, valid_profile};
use amaterasu_module_sdk::session_presentation::{Application, Context, Presentation, Selection};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    Input,
    Monitor,
}

pub(crate) struct Projected {
    pub presentation: Presentation,
    pub entries: Vec<Application>,
    pub running_tile: Option<Application>,
}

pub(crate) fn presentation(
    context: &Context,
    role: Option<Role>,
    replacement_confirmation: bool,
) -> Presentation {
    let exposed = context.game.as_ref().filter(|game| {
        replacement_confirmation || (role.is_none() && game.owner_client_id == context.client.id)
    });
    Presentation {
        busy: exposed.is_some(),
        current_game: exposed.map(|game| game.application.id.clone()),
        current_game_uuid: exposed
            .map(|game| game.application.uuid.clone())
            .filter(|uuid| !uuid.is_empty()),
    }
}

pub(crate) fn project(
    context: &Context,
    applications: &[Application],
    role: Option<Role>,
    show_input: bool,
    show_monitor: bool,
) -> Result<Projected, &'static str> {
    if !context.client.paired {
        return Err("unauthenticated_client");
    }
    let mut configured: Vec<Application> = applications
        .iter()
        .filter(|application| {
            if !application.visible {
                return false;
            }
            if application.monitor_profile {
                return valid_profile(application, applications);
            }
            !reserved_name(&application.title)
                && !matches!(
                    identify(&Selection {
                        id: application.id.clone(),
                        uuid: Some(application.uuid.clone())
                    }),
                    Some(Control::Input | Control::Monitor)
                )
        })
        .cloned()
        .collect();
    let mut entries = Vec::with_capacity(configured.len() + 5);
    let mut tile = None;
    if role == Some(Role::Monitor) {
        entries.push(Control::Resume.application(false));
        entries.push(Control::DisconnectMonitor.application(false));
        configured.retain(|application| !application.monitor_profile);
        entries.extend(configured);
    } else {
        let secondary = context
            .game
            .as_ref()
            .is_some_and(|game| game.owner_client_id != context.client.id);
        if secondary {
            let game = &context
                .game
                .as_ref()
                .expect("secondary implies running game")
                .application;
            let running = running_tile(game, applications)?;
            tile = Some(running.clone());
            entries.push(running);
            entries.push(Control::Resume.application(true));
            entries.push(Control::Terminate.application(true));
        }
        entries.extend(configured);
        // A live or retiring Input owner keeps its reconnect entry even if the
        // operator hides new Input starts. A stale cached list stays useful.
        if show_input || role == Some(Role::Input) {
            entries.push(Control::Input.application(secondary));
        }
        if show_monitor {
            entries.push(Control::Monitor.application(secondary));
        }
    }
    Ok(Projected {
        presentation: presentation(context, role, false),
        entries,
        running_tile: tile,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use amaterasu_module_sdk::session_presentation::{Client, Game};

    fn app(id: &str, title: &str) -> Application {
        Application {
            id: id.into(),
            uuid: format!("uuid-{id}"),
            title: title.into(),
            aliases: vec![],
            visible: true,
            monitor_profile: false,
            properties: serde_json::json!({}),
        }
    }
    fn context(id: &str, running: bool) -> Context {
        Context {
            client: Client {
                id: id.into(),
                paired: true,
                may_list: true,
                may_view: true,
                may_launch: true,
                may_terminate: true,
            },
            game: running.then(|| Game {
                application: app("42", "Running game"),
                owner_client_id: "owner".into(),
                generation: 7,
                source: None,
            }),
        }
    }
    #[test]
    fn owner_and_secondary_clients_have_different_views_of_same_game() {
        let configured = vec![app("42", "Running game"), app("9", "Another game")];
        let owner = project(&context("owner", true), &configured, None, true, true).unwrap();
        assert!(owner.presentation.busy);
        assert_eq!(owner.presentation.current_game.as_deref(), Some("42"));
        assert_eq!(owner.entries.len(), 4);
        let secondary = project(&context("other", true), &configured, None, true, true).unwrap();
        assert!(!secondary.presentation.busy);
        assert!(secondary.presentation.current_game.is_none());
        assert_eq!(secondary.entries.len(), 7);
        assert_eq!(secondary.entries[0].title, "     Running game");
        assert_eq!(secondary.entries[1].id, Control::Resume.id(true));
        assert_eq!(secondary.entries[2].id, Control::Terminate.id(true));
        assert_eq!(secondary.entries[3].id, "42");
        assert_eq!(secondary.entries[5].id, Control::Input.id(true));
        assert_eq!(secondary.entries[6].id, Control::Monitor.id(true));
    }

    #[test]
    fn retained_monitor_does_not_strand_client_in_a_controls_only_cache() {
        let mut profile = app("8", "Private screen");
        profile.uuid = "c1234567-1234-4234-8234-123456789abc".into();
        profile.monitor_profile = true;
        let projected = project(
            &context("owner", true),
            &[app("42", "Running game"), profile],
            Some(Role::Monitor),
            true,
            true,
        )
        .unwrap();
        assert!(!projected.presentation.busy);
        assert_eq!(
            projected
                .entries
                .iter()
                .map(|entry| entry.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Resume", "Disconnect Monitor", "Running game"]
        );
    }

    #[test]
    fn input_reconnect_identity_survives_teardown_in_every_game_state() {
        for client in ["owner", "secondary"] {
            for running in [false, true] {
                let before = project(
                    &context(client, running),
                    &[],
                    Some(Role::Input),
                    true,
                    true,
                )
                .unwrap();
                let after = project(&context(client, running), &[], None, true, true).unwrap();
                let input = |entries: Vec<Application>| {
                    entries
                        .into_iter()
                        .find(|entry| {
                            identify(&Selection {
                                id: entry.id.clone(),
                                uuid: None,
                            }) == Some(Control::Input)
                        })
                        .unwrap()
                };
                assert_eq!(input(before.entries), input(after.entries));
            }
        }
    }

    #[test]
    fn reserved_names_and_markers_cannot_add_duplicate_builtin_controls() {
        let mut shadow = app("4", "Different label");
        shadow.uuid = Control::Input.uuid();
        let projected = project(
            &context("client", false),
            &[
                app("1", "Real game"),
                app("2", "    remote monitor"),
                app("3", "Remote Input"),
                shadow,
            ],
            None,
            true,
            true,
        )
        .unwrap();
        assert_eq!(projected.entries.len(), 3);
        assert_eq!(projected.entries[0].title, "Real game");
    }

    #[test]
    fn confirmation_exposes_busy_only_for_matching_pending_replacement() {
        let other = context("other", true);
        assert!(!presentation(&other, None, false).busy);
        assert!(presentation(&other, None, true).busy);
        assert!(!presentation(&context("owner", true), Some(Role::Monitor), false).busy);
    }
}
