//! Moonlight synthetic controls and their client-specific authorization.
use super::*;

impl SessionPolicy {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn control_decision(
        &mut self,
        caller: &str,
        context: &Context,
        operation: &str,
        control: Control,
        configured: Option<&Application>,
        mode: Mode,
        now: Instant,
        monitors: &mut impl Monitors,
    ) -> (Option<Decision>, bool) {
        let role = self.role(&context.client.id);
        match control {
            Control::Resume | Control::RunningGame => {
                if !context.client.may_view {
                    return (Some(reject(403, "View permission denied")), false);
                }
                if role == Some(Role::Monitor) {
                    return self.auxiliary_decision(
                        caller,
                        context,
                        operation,
                        Role::Monitor,
                        None,
                        mode,
                        now,
                        monitors,
                    );
                }
                let Some(game) = &context.game else {
                    return (
                        Some(reject(
                            404,
                            "No running game or retained Remote Monitor belongs to this caller",
                        )),
                        false,
                    );
                };
                let source = match self.borrow_source(caller, context, operation, mode, monitors) {
                    Ok(source) => source,
                    Err(_) => return (None, false),
                };
                (
                    Some(Decision::ResumeApplication {
                        application_id: game.application.id.clone(),
                        source,
                    }),
                    false,
                )
            }
            Control::Input | Control::Monitor => {
                if !context.client.may_launch {
                    return (Some(reject(403, "Launch permission denied")), false);
                }
                let requested = if control == Control::Input {
                    Role::Input
                } else {
                    Role::Monitor
                };
                if requested == Role::Monitor
                    && configured.is_none()
                    && role == Some(Role::Monitor)
                    && self
                        .owner(&context.client.id)
                        .is_some_and(|owner| owner.profile_id != Control::Monitor.uuid())
                {
                    return (
                        Some(reject(
                            409,
                            "Disconnect Monitor before selecting a different Remote Monitor profile",
                        )),
                        false,
                    );
                }
                if role.is_some_and(|role| role != requested) {
                    return (
                        Some(reject(
                            409,
                            "Disconnect the current remote session before selecting another mode",
                        )),
                        false,
                    );
                }
                if role.is_none()
                    && configured.is_none()
                    && ((requested == Role::Input && !self.settings.show_remote_input)
                        || (requested == Role::Monitor && !self.settings.show_remote_monitor))
                {
                    return (
                        Some(reject(404, "This remote session control is disabled")),
                        false,
                    );
                }
                self.auxiliary_decision(
                    caller, context, operation, requested, configured, mode, now, monitors,
                )
            }
            Control::DisconnectInput | Control::DisconnectMonitor => {
                if !context.client.may_terminate {
                    return (Some(reject(403, "Terminate permission denied")), false);
                }
                let expected = if control == Control::DisconnectInput {
                    Role::Input
                } else {
                    Role::Monitor
                };
                if role.is_some_and(|role| role != expected) {
                    return (
                        Some(reject(
                            403,
                            "The requested remote session is not owned by this caller",
                        )),
                        false,
                    );
                }
                if let Some(owner) = self
                    .owner(&context.client.id)
                    .filter(|owner| owner.role_active)
                {
                    (
                        Some(Decision::StopAuxiliary {
                            session_id: owner.session_id.clone(),
                            generation: owner.generation,
                            completion: completion(control),
                        }),
                        false,
                    )
                } else {
                    (
                        Some(Decision::Complete {
                            status_code: 410,
                            message: completion(control).message,
                        }),
                        false,
                    )
                }
            }
            Control::Terminate => {
                if !context.client.may_terminate {
                    return (Some(reject(403, "Terminate permission denied")), false);
                }
                let Some(game) = &context.game else {
                    return (
                        Some(reject(404, "No running game is available to terminate")),
                        false,
                    );
                };
                let owner = game.owner_client_id == context.client.id;
                if !owner
                    && !self.settings.remote_monitor_terminate_on_first_request
                    && !self.confirm(
                        &context.client.id,
                        game.generation,
                        &game.application.id,
                        &game.application.id,
                        true,
                        now,
                    )
                {
                    return (
                        Some(reject(
                            410,
                            "This closes the running game while keeping Remote Monitor and Remote Input connected. Launch Terminate again within 60 seconds to confirm.",
                        )),
                        false,
                    );
                }
                if (owner || self.settings.remote_monitor_terminate_on_first_request)
                    && let Some(state) = self.clients.get_mut(&context.client.id)
                {
                    state.terminate = None;
                }
                (
                    Some(Decision::StopApplication {
                        application_id: game.application.id.clone(),
                        generation: game.generation,
                        completion: completion(control),
                    }),
                    false,
                )
            }
        }
    }
}
