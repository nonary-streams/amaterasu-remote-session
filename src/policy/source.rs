//! Source reservations, transport/application binding, and exact readiness proof.
use super::*;

impl SessionPolicy {
    pub(super) fn borrow_source(
        &mut self,
        caller: &str,
        context: &Context,
        operation: &str,
        _mode: Mode,
        monitors: &mut impl Monitors,
    ) -> Result<Option<SharedCapture>, &'static str> {
        let borrower = &context.client.id;
        let source_owner = if let Some(owner) = self.owner(borrower).filter(|owner| {
            owner.role == Role::Monitor && owner.role_active && !owner.release_requested
        }) {
            owner.client_id.clone()
        } else if let Some(game) = &context.game {
            let Some(source) = &game.source else {
                return Ok(None);
            };
            let Some(owner) = self.desired.owners.iter().find(|owner| {
                owner.session_id == source.session_id && owner.generation == source.generation
            }) else {
                return Err("game_source_changed");
            };
            let Some(original) = owner
                .reservations
                .iter()
                .find(|entry| entry.id == source.reservation_id)
            else {
                return Err("game_source_changed");
            };
            if original.consumer != caller
                || original.kind != ReservationKind::Application
                || original.borrower_id != game.owner_client_id
                || original.application_generation != Some(game.generation)
                || !original.started
                || !monitors
                    .snapshot(&owner.client_id)
                    .is_some_and(|status| status.source_mode == Some(source.mode))
                || !self.source_matches(owner, &source.reservation_id, &source.capture, monitors)
            {
                return Err("game_source_unverified");
            }
            owner.client_id.clone()
        } else {
            return Ok(None);
        };
        let Some(owner) = self.owner(&source_owner) else {
            return Err("source_changed");
        };
        let Some(status) = monitors
            .snapshot(&source_owner)
            .filter(|status| status.generation == owner.generation && status.source_ready)
        else {
            return Err("monitor_not_ready");
        };
        let Some(output) = status.output else {
            return Err("monitor_not_ready");
        };
        let Some(source_mode) = status.source_mode else {
            return Err("monitor_source_mode_unavailable");
        };
        // A released role may still supply an already associated running game;
        // no arbitrary caller can select that source without its current Game.
        let allow_released = context
            .game
            .as_ref()
            .and_then(|game| game.source.as_ref())
            .is_some_and(|source| {
                source.session_id == owner.session_id && source.generation == owner.generation
            });
        self.reserve_source(
            caller,
            &source_owner,
            borrower,
            operation,
            ReservationKind::Borrowed,
            allow_released,
        )?;
        let owner = self
            .owner(&source_owner)
            .expect("borrowed source retains owner");
        let reservation = reservation_id(caller, borrower, operation);
        if !self.reservation_durable(owner, &reservation) {
            return Err("reservation_not_durable");
        }
        Ok(Some(SharedCapture {
            session_id: owner.session_id.clone(),
            generation: owner.generation,
            reservation_id: reservation,
            capture: CaptureSource::ExactOutput {
                output,
                route_key: status.source_route,
            },
            mode: source_mode,
        }))
    }

    pub(super) fn deliver_decision(
        &self,
        caller: &str,
        client: &str,
        operation: &str,
        decision: Decision,
        monitors: &impl Monitors,
    ) -> Reply {
        let source = match &decision {
            Decision::Auxiliary { plan } | Decision::ResumeAuxiliary { plan } => Some((
                plan.session_id.as_str(),
                plan.generation,
                plan.reservation_id.as_str(),
                Some(&plan.capture),
            )),
            Decision::LaunchApplication {
                source: Some(source),
                ..
            }
            | Decision::ResumeApplication {
                source: Some(source),
                ..
            }
            | Decision::ReplaceApplication {
                source: Some(source),
                ..
            } => Some((
                source.session_id.as_str(),
                source.generation,
                source.reservation_id.as_str(),
                Some(&source.capture),
            )),
            _ => None,
        };
        if let Some((session, generation, reservation, capture)) = source {
            let Some(owner) = self.desired.owners.iter().find(|owner| {
                owner.session_id == session
                    && owner.generation == generation
                    && owner.reservations.iter().any(|entry| {
                        entry.id == reservation
                            && entry.borrower_id == client
                            && entry.consumer == caller
                    })
            }) else {
                return error("source_changed", "Remote session source changed");
            };
            if reservation != reservation_id(caller, client, operation)
                || !self.reservation_durable(owner, reservation)
            {
                return pending();
            }
            if matches!(capture, Some(CaptureSource::ExactOutput { .. }))
                && !self.source_matches(
                    owner,
                    reservation,
                    capture.expect("source capture"),
                    monitors,
                )
            {
                if let Some(failure) = monitors
                    .snapshot(&owner.client_id)
                    .as_ref()
                    .and_then(terminal_monitor_failure)
                {
                    return Reply::Dispatched { decision: failure };
                }
                return pending();
            }
        }
        Reply::Dispatched { decision }
    }

    pub(super) fn reserve(
        &mut self,
        caller: &str,
        source_owner: &str,
        borrower: &str,
        operation: &str,
        kind: ReservationKind,
    ) -> Result<(), &'static str> {
        self.reserve_source(caller, source_owner, borrower, operation, kind, false)
    }

    pub(super) fn reserve_source(
        &mut self,
        caller: &str,
        source_owner: &str,
        borrower: &str,
        operation: &str,
        kind: ReservationKind,
        allow_released: bool,
    ) -> Result<(), &'static str> {
        let owner = self
            .desired
            .owners
            .iter_mut()
            .find(|owner| owner.client_id == source_owner)
            .ok_or("owner_not_found")?;
        let id = reservation_id(caller, borrower, operation);
        if let Some(previous) = owner
            .reservations
            .iter()
            .find(|reservation| reservation.id == id)
        {
            return if previous.consumer == caller
                && previous.borrower_id == borrower
                && (previous.kind == kind
                    || previous.kind == ReservationKind::Application
                        && kind == ReservationKind::Borrowed)
            {
                Ok(())
            } else {
                Err("reservation_conflict")
            };
        }
        if !allow_released && (owner.release_requested || !owner.role_active) {
            return Err("source_releasing");
        }
        if owner.reservations.len() >= MAX_RESERVATIONS {
            return Err("source_reservation_capacity_exhausted");
        }
        owner.reservations.push(Reservation {
            id,
            kind,
            borrower_id: borrower.into(),
            consumer: caller.into(),
            application_operation_id: None,
            application_epoch: None,
            application_generation: None,
            transport_id: None,
            runtime_epoch: None,
            started: false,
            retirement_requested: false,
        });
        self.changed();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn bind_transport(
        &mut self,
        caller: &str,
        client: &str,
        session: &str,
        generation: u64,
        reservation: &str,
        transport: &str,
        epoch: &str,
    ) -> Reply {
        let Some(owner) = self
            .desired
            .owners
            .iter_mut()
            .find(|owner| owner.session_id == session && owner.generation == generation)
        else {
            return error(
                "source_changed",
                "Remote source is no longer owned by this client",
            );
        };

        let Some(binding) = owner.reservations.iter_mut().find(|binding| {
            binding.id == reservation && binding.borrower_id == client && binding.consumer == caller
        }) else {
            return error(
                "reservation_not_found",
                "Remote source admission is unavailable",
            );
        };
        if let Some(previous) = &binding.transport_id {
            if previous != transport || binding.runtime_epoch.as_deref() != Some(epoch) {
                return error(
                    "transport_binding_conflict",
                    "Remote source admission is already bound to another transport",
                );
            }
        } else {
            binding.transport_id = Some(transport.into());
            binding.runtime_epoch = Some(epoch.into());
            self.changed();
        }
        let owner = self
            .desired
            .owners
            .iter()
            .find(|owner| owner.session_id == session && owner.generation == generation)
            .expect("bound owner");
        if self.reservation_durable(owner, reservation)
            && self
                .durable
                .owners
                .iter()
                .find(|entry| entry.session_id == session)
                .and_then(|entry| {
                    entry
                        .reservations
                        .iter()
                        .find(|entry| entry.id == reservation)
                })
                .is_some_and(|entry| {
                    entry.transport_id.as_deref() == Some(transport)
                        && entry.runtime_epoch.as_deref() == Some(epoch)
                })
        {
            Reply::Acknowledged {}
        } else {
            pending()
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn bind_application(
        &mut self,
        caller: &str,
        client: &str,
        session: &str,
        generation: u64,
        reservation: &str,
        operation: &str,
        epoch: &str,
        application_generation: u64,
    ) -> Reply {
        let Some(owner) = self
            .desired
            .owners
            .iter_mut()
            .find(|owner| owner.session_id == session && owner.generation == generation)
        else {
            return error("source_changed", "Retained source is unavailable");
        };
        let Some(binding) = owner.reservations.iter_mut().find(|entry| {
            entry.id == reservation
                && entry.borrower_id == client
                && entry.consumer == caller
                && entry.kind != ReservationKind::Auxiliary
        }) else {
            return error(
                "reservation_not_found",
                "Application source reservation is unavailable",
            );
        };
        if binding.transport_id.is_none() || binding.runtime_epoch.is_none() {
            return error(
                "transport_binding_required",
                "Persist the source transport before binding its application",
            );
        }
        if let Some(previous) = &binding.application_operation_id {
            if previous != operation
                || binding.application_epoch.as_deref() != Some(epoch)
                || binding.application_generation != Some(application_generation)
            {
                return error(
                    "application_binding_conflict",
                    "Retained source is already bound to another application",
                );
            }
        } else {
            binding.kind = ReservationKind::Application;
            binding.application_operation_id = Some(operation.into());
            binding.application_epoch = Some(epoch.into());
            binding.application_generation = Some(application_generation);
            self.changed();
        }
        let stored = self
            .durable
            .owners
            .iter()
            .find(|owner| owner.session_id == session && owner.generation == generation)
            .and_then(|owner| {
                owner
                    .reservations
                    .iter()
                    .find(|entry| entry.id == reservation)
            });
        if stored.is_some_and(|entry| {
            entry.application_operation_id.as_deref() == Some(operation)
                && entry.application_epoch.as_deref() == Some(epoch)
                && entry.application_generation == Some(application_generation)
        }) {
            Reply::Acknowledged {}
        } else {
            pending()
        }
    }

    pub(super) fn verify_source(
        &self,
        caller: &str,
        client: &str,
        source: SharedCapture,
        monitors: &impl Monitors,
    ) -> Reply {
        let Some(owner) = self.desired.owners.iter().find(|owner| {
            owner.session_id == source.session_id
                && owner.generation == source.generation
                && owner.reservations.iter().any(|entry| {
                    entry.id == source.reservation_id
                        && entry.borrower_id == client
                        && entry.consumer == caller
                })
        }) else {
            return error("source_changed", "Retained source identity changed");
        };
        if self.reservation_durable(owner, &source.reservation_id)
            && monitors
                .snapshot(&owner.client_id)
                .is_some_and(|status| status.source_mode == Some(source.mode))
            && self.source_matches(owner, &source.reservation_id, &source.capture, monitors)
        {
            Reply::Verified { source }
        } else {
            error(
                "source_not_ready",
                "Retained source reservation is not ready",
            )
        }
    }

    pub(super) fn source_matches(
        &self,
        owner: &RetainedOwner,
        reservation: &str,
        capture: &CaptureSource,
        monitors: &impl Monitors,
    ) -> bool {
        if owner.role != Role::Monitor
            || !owner
                .reservations
                .iter()
                .any(|entry| entry.id == reservation)
        {
            return false;
        }
        let Some(status) = monitors.snapshot(&owner.client_id) else {
            return false;
        };
        status.generation == owner.generation
            && status.source_ready
            && matches!(capture,CaptureSource::ExactOutput{output,route_key} if status.output.as_ref()==Some(output) && &status.source_route==route_key)
            && self.applied_holds.contains(&(
                owner.client_id.clone(),
                owner.generation,
                reservation.into(),
            ))
    }

    pub(super) fn reservation_durable(&self, owner: &RetainedOwner, reservation: &str) -> bool {
        let desired = owner
            .reservations
            .iter()
            .find(|entry| entry.id == reservation);
        let stored = self
            .durable
            .owners
            .iter()
            .find(|entry| {
                entry.client_id == owner.client_id
                    && entry.session_id == owner.session_id
                    && entry.generation == owner.generation
            })
            .and_then(|entry| {
                entry
                    .reservations
                    .iter()
                    .find(|entry| entry.id == reservation)
            });
        desired.is_some()
            && desired == stored
            && !self
                .recovering
                .contains(&(owner.session_id.clone(), reservation.into()))
    }
}
