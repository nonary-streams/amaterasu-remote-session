//! Retained auxiliary ownership admission and immutable profile selection.
use super::*;

impl SessionPolicy {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn auxiliary_decision(
        &mut self,
        caller: &str,
        context: &Context,
        operation: &str,
        role: Role,
        configured: Option<&Application>,
        mode: Mode,
        _now: Instant,
        monitors: &mut impl Monitors,
    ) -> (Option<Decision>, bool) {
        let client = &context.client.id;
        let requested_profile = configured
            .filter(|application| application.monitor_profile)
            .map(|application| application.uuid.clone());
        let mut created = false;
        if let Some(owner) = self.owner(client) {
            if owner.consumer != caller {
                return (
                    Some(reject(
                        403,
                        "Remote session belongs to another protocol consumer",
                    )),
                    false,
                );
            }
            if !owner.role_active || owner.release_requested {
                return (
                    Some(reject(409, "The previous remote source is still retiring")),
                    false,
                );
            }
            if owner.role != role {
                return (
                    Some(reject(
                        409,
                        "Disconnect the current remote session before selecting another mode",
                    )),
                    false,
                );
            }
            if let Some(profile) = requested_profile.as_deref()
                && owner.profile_id != profile
            {
                return (
                    Some(reject(
                        409,
                        "Disconnect Monitor before selecting a different Remote Monitor profile",
                    )),
                    false,
                );
            }
            let desired_mode = match resolve_profile_mode(mode, &owner.profile_settings) {
                Ok(mode) => mode,
                Err(code) => return (Some(reject(400, code)), false),
            };
            if owner.mode != desired_mode {
                if !owner.reservations.is_empty() {
                    return (
                        Some(reject(
                            409,
                            "The retained display mode can change after its current captures retire",
                        )),
                        false,
                    );
                }
                self.desired
                    .owners
                    .iter_mut()
                    .find(|owner| owner.client_id == *client)
                    .expect("validated owner")
                    .mode = desired_mode;
                self.changed();
            }
        } else {
            if self.desired.owners.len() >= self.settings.max_clients {
                return (
                    Some(reject(503, "Remote session client capacity is exhausted")),
                    false,
                );
            }
            let generation = self.desired.next_generation;
            let Some(next) = generation.checked_add(1) else {
                return (
                    Some(reject(503, "Remote session generation is exhausted")),
                    false,
                );
            };
            self.desired.next_generation = next;
            let session_id = owner_session_id(caller, client, operation, generation);
            let profile_settings = configured
                .filter(|application| application.monitor_profile)
                .map(|application| application.properties.clone())
                .unwrap_or_else(|| json!({}));
            let mode = match resolve_profile_mode(mode, &profile_settings) {
                Ok(mode) => mode,
                Err(code) => return (Some(reject(400, code)), false),
            };
            self.desired.owners.push(RetainedOwner {
                client_id: client.clone(),
                consumer: caller.into(),
                session_id,
                generation,
                role,
                role_active: true,
                role_committed: false,
                mode,
                profile_id: requested_profile.unwrap_or_else(|| {
                    if role == Role::Monitor {
                        Control::Monitor.uuid()
                    } else {
                        Control::Input.uuid()
                    }
                }),
                profile_settings,
                release_requested: false,
                reservations: Vec::new(),
            });
            self.changed();
            created = true;
            self.clear_replacement(client);
        }
        if let Err(code) = self.reserve(
            caller,
            client,
            client,
            operation,
            ReservationKind::Auxiliary,
        ) {
            return (Some(reject(503, code)), created);
        }
        let Some(owner) = self.owner(client) else {
            return (Some(reject(503, "Remote session is unavailable")), created);
        };
        if !self.reservation_durable(owner, &reservation_id(caller, client, operation)) {
            return (None, created);
        }
        let capture = if role == Role::Input {
            CaptureSource::SyntheticBlack
        } else {
            if let Some(failure) = monitors
                .snapshot(client)
                .filter(|status| status.generation == owner.generation)
                .as_ref()
                .and_then(terminal_monitor_failure)
            {
                return (Some(failure), created);
            }
            let Some(status) = monitors
                .snapshot(client)
                .filter(|status| status.generation == owner.generation && status.ready)
            else {
                return (None, created);
            };
            let Some(output) = status.output else {
                return (None, created);
            };
            CaptureSource::ExactOutput {
                output,
                route_key: status.source_route,
            }
        };
        let source_mode = if role == Role::Monitor {
            monitors
                .snapshot(client)
                .and_then(|status| status.source_mode)
        } else {
            None
        };
        if role == Role::Monitor && source_mode.is_none() {
            return (None, created);
        }
        let plan = AuxiliaryStreamPlan {
            session_id: owner.session_id.clone(),
            generation: owner.generation,
            reservation_id: reservation_id(caller, client, operation),
            capture,
            source_mode,
            mode,
            stream_audio: role == Role::Monitor && !self.settings.remote_monitor_mute_audio,
            host_audio: role == Role::Monitor,
        };
        (Some(Decision::Auxiliary { plan }), created)
    }
}
