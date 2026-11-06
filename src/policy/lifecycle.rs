//! Role teardown and effect synchronization from confirmed durable intent.
use super::*;

impl SessionPolicy {
    pub(super) fn revoke_client(
        &mut self,
        consumer: Option<&str>,
        client: &str,
        now: Instant,
        monitors: &mut impl Monitors,
    ) -> Reply {
        let mut changed = false;
        for owner in &mut self.desired.owners {
            if owner.client_id == client
                && consumer.is_none_or(|consumer| owner.consumer == consumer)
                && !owner.release_requested
            {
                owner.release_requested = true;
                changed = true;
            }
            for reservation in &mut owner.reservations {
                if reservation.borrower_id == client
                    && consumer.is_none_or(|consumer| reservation.consumer == consumer)
                    && reservation.transport_id.is_some()
                {
                    self.recovering
                        .insert((owner.session_id.clone(), reservation.id.clone()));
                    if !reservation.retirement_requested {
                        reservation.retirement_requested = true;
                        changed = true;
                    }
                }
            }
            let before = owner.reservations.len();
            owner.reservations.retain(|entry| {
                entry.borrower_id != client
                    || consumer.is_some_and(|consumer| entry.consumer != consumer)
                    || entry.transport_id.is_some()
            });
            changed |= owner.reservations.len() != before;
        }
        self.cache.forget(client);
        self.projections.forget(client);
        self.clients.remove(client);
        if changed {
            self.changed();
        }
        self.synchronize(now, monitors);
        let owned = self.desired.owners.iter().any(|owner| {
            owner.client_id == client && owner.role_active
                || owner
                    .reservations
                    .iter()
                    .any(|entry| entry.borrower_id == client)
        });
        let stored = self.durable.owners.iter().any(|owner| {
            owner.client_id == client && owner.role_active
                || owner
                    .reservations
                    .iter()
                    .any(|entry| entry.borrower_id == client)
        });
        if owned || stored || self.needs_persistence() {
            pending()
        } else {
            Reply::Acknowledged {}
        }
    }

    pub(super) fn observe(
        &mut self,
        caller: &str,
        observation: Observation,
        now: Instant,
        monitors: &mut impl Monitors,
    ) -> Reply {
        let client = &observation.client_id;
        let Some(index) = self.desired.owners.iter().position(|owner| {
            owner.session_id == observation.session_id && owner.generation == observation.generation
        }) else {
            return Reply::Acknowledged {};
        };
        let owner = &mut self.desired.owners[index];
        if matches!(
            observation.event,
            SessionEvent::Unpaired | SessionEvent::Shutdown
        ) {
            if owner.client_id != *client || owner.consumer != caller {
                return error("forbidden", "Only the retained owner may release its role");
            }
            let changed = !owner.release_requested;
            owner.release_requested = true;
            self.cache.forget(client);
            self.projections.forget(client);
            self.clients.remove(client);
            if changed {
                self.changed();
            }
            return pending();
        }
        if observation.event == SessionEvent::ExplicitDisconnect {
            if owner.client_id != *client || owner.consumer != caller {
                return error(
                    "forbidden",
                    "Only the retained owner may disconnect its role",
                );
            }
            if owner
                .reservations
                .iter()
                .any(|entry| entry.kind == ReservationKind::Auxiliary)
            {
                return error(
                    "transport_retirement_unconfirmed",
                    "Remote input/capture transport has not retired",
                );
            }
            if !owner.release_requested {
                owner.release_requested = true;
                self.changed();
            }
            self.synchronize(now, monitors);
            return self.disconnect_completion(client);
        }
        let Some(reservation) = owner.reservations.iter_mut().find(|entry| {
            entry.id == observation.reservation_id
                && entry.borrower_id == *client
                && entry.consumer == caller
        }) else {
            return Reply::Acknowledged {};
        };
        if reservation.transport_id.as_deref() != Some(&observation.transport_id)
            || reservation.runtime_epoch != observation.transport_epoch
        {
            return error(
                "transport_binding_conflict",
                "Remote source retirement does not match its accepted transport",
            );
        }
        match observation.event {
            SessionEvent::StreamStarted => {
                let mut changed = !reservation.started;
                reservation.started = true;
                if reservation.kind == ReservationKind::Auxiliary && !owner.role_committed {
                    owner.role_committed = true;
                    changed = true;
                }
                if changed {
                    self.changed();
                }
                for ((consumer, borrower, operation_id), operation) in &mut self.operations {
                    if consumer == caller
                        && borrower == client
                        && reservation_id(consumer, borrower, operation_id)
                            == observation.reservation_id
                    {
                        operation.committed = true;
                        operation.created_owner = None;
                    }
                }
            }
            SessionEvent::StreamEnded
            | SessionEvent::FailedStart
            | SessionEvent::ClientDisconnected => {
                let role = owner.role;
                let auxiliary = reservation.kind == ReservationKind::Auxiliary;
                owner
                    .reservations
                    .retain(|entry| entry.id != observation.reservation_id);
                self.recovering.remove(&(
                    observation.session_id.clone(),
                    observation.reservation_id.clone(),
                ));
                if auxiliary
                    && !owner
                        .reservations
                        .iter()
                        .any(|entry| entry.kind == ReservationKind::Auxiliary)
                    && (role == Role::Input
                        || self.settings.remote_monitor_disconnect_on_stream_end
                        || (observation.event == SessionEvent::ClientDisconnected
                            && self.settings.remote_monitor_disconnect_on_client_disconnect))
                {
                    owner.release_requested = true;
                }
                self.changed();
            }
            _ => {}
        }
        self.synchronize(now, monitors);
        if self.needs_persistence() {
            pending()
        } else {
            Reply::Acknowledged {}
        }
    }

    pub(super) fn disconnect_completion(&self, client: &str) -> Reply {
        if self
            .desired
            .owners
            .iter()
            .find(|owner| owner.client_id == client)
            .is_none_or(|owner| !owner.role_active)
            && self
                .durable
                .owners
                .iter()
                .find(|owner| owner.client_id == client)
                .is_none_or(|owner| !owner.role_active)
        {
            Reply::Acknowledged {}
        } else {
            pending()
        }
    }

    pub(super) fn cancel_dispatch(
        &mut self,
        caller: &str,
        client: &str,
        operation: &str,
        now: Instant,
        monitors: &mut impl Monitors,
    ) -> Reply {
        let key = (caller.into(), client.into(), operation.into());
        if !self.operations.contains_key(&key) {
            let reservation = reservation_id(caller, client, operation);
            let binding = self
                .desired
                .owners
                .iter()
                .flat_map(|owner| &owner.reservations)
                .find(|entry| {
                    entry.id == reservation
                        && entry.consumer == caller
                        && entry.borrower_id == client
                });
            if binding.is_some_and(|entry| entry.started) {
                return Reply::Acknowledged {};
            }
            let created = self
                .owner(client)
                .filter(|owner| {
                    owner.consumer == caller
                        && !owner.role_committed
                        && owner.session_id
                            == owner_session_id(caller, client, operation, owner.generation)
                })
                .map(|owner| (owner.session_id.clone(), owner.generation));
            if binding.is_none() && created.is_none() {
                return Reply::Acknowledged {};
            }
            self.prune_operations(now);
            if self.operations.len() >= MAX_OPERATIONS {
                return error(
                    "operation_capacity_exhausted",
                    "Pending cancellation cannot be tracked safely",
                );
            }
            self.operations.insert(
                key.clone(),
                Operation {
                    fingerprint: [0; 32],
                    deadline: now + CONFIRMATION_WINDOW,
                    decision: None,
                    canceled: true,
                    created_owner: created,
                    replacement_authorized: false,
                    terminal_decision: None,
                    committed: false,
                    auxiliary_target: None,
                },
            );
        }
        let Some(pending_operation) = self.operations.get_mut(&key) else {
            return Reply::Acknowledged {};
        };
        if pending_operation.committed {
            return Reply::Acknowledged {};
        }
        pending_operation.canceled = true;
        let created = pending_operation.created_owner.clone();
        let mut changed = false;
        for owner in &mut self.desired.owners {
            let reservation = reservation_id(caller, client, operation);
            let bound = owner.reservations.iter_mut().find(|entry| {
                entry.id == reservation
                    && entry.borrower_id == client
                    && entry.transport_id.is_some()
            });
            if let Some(binding) = bound {
                if !binding.retirement_requested {
                    binding.retirement_requested = true;
                    self.recovering
                        .insert((owner.session_id.clone(), binding.id.clone()));
                    changed = true;
                }
                continue;
            }
            let before = owner.reservations.len();
            owner
                .reservations
                .retain(|entry| entry.id != reservation || entry.borrower_id != client);
            if created.as_ref().is_some_and(|(session, generation)| {
                owner.session_id == *session && owner.generation == *generation
            }) && owner.client_id == client
                && !owner
                    .reservations
                    .iter()
                    .any(|entry| entry.kind == ReservationKind::Auxiliary)
            {
                owner.release_requested = true;
                changed = true;
            }
            changed |= owner.reservations.len() != before;
        }
        if changed {
            self.changed();
        }
        self.synchronize(now, monitors);
        if self.needs_persistence() {
            return pending();
        }
        if created.as_ref().is_some_and(|(session, generation)| {
            self.owner(client).is_some_and(|owner| {
                owner.session_id == *session
                    && owner.generation == *generation
                    && owner.role_active
                    && !owner.role_committed
            })
        }) {
            return pending();
        }
        if self
            .operations
            .get(&key)
            .is_none_or(|operation| operation.terminal_decision.is_none())
        {
            self.operations.remove(&key);
        }
        Reply::Acknowledged {}
    }

    pub(crate) fn synchronize(&mut self, now: Instant, monitors: &mut impl Monitors) {
        let mut retained_holds: BTreeSet<_> = self
            .durable
            .owners
            .iter()
            .filter(|owner| owner.role == Role::Monitor)
            .flat_map(|owner| {
                owner
                    .reservations
                    .iter()
                    .map(|entry| (owner.client_id.clone(), owner.generation, entry.id.clone()))
            })
            .collect();
        for owner in self.durable.owners.clone() {
            if owner.role != Role::Monitor {
                continue;
            }
            if owner.reservations.iter().any(|reservation| {
                self.recovering
                    .contains(&(owner.session_id.clone(), reservation.id.clone()))
            }) {
                retained_holds.extend(
                    self.applied_holds
                        .iter()
                        .filter(|(client, generation, _)| {
                            client == &owner.client_id && *generation == owner.generation
                        })
                        .cloned(),
                );
                continue;
            }
            let already_selected = monitors.snapshot(&owner.client_id).is_some_and(|status| {
                status.generation == owner.generation
                    && status.source_ready
                    && owner.release_requested
            });
            if !already_selected && monitors.begin(&owner, now).is_err() {
                continue;
            }
            for reservation in &owner.reservations {
                let key = (
                    owner.client_id.clone(),
                    owner.generation,
                    reservation.id.clone(),
                );
                retained_holds.insert(key.clone());
                if !self.applied_holds.contains(&key)
                    && monitors
                        .capture_started(&owner.client_id, owner.generation, &reservation.id)
                        .is_ok()
                {
                    self.applied_holds.insert(key);
                }
            }
            if owner.release_requested {
                let _ = monitors.release(&owner.client_id, owner.generation, now);
            }
        }
        let retired: Vec<_> = self
            .applied_holds
            .difference(&retained_holds)
            .cloned()
            .collect();
        for (client, generation, reservation) in retired {
            monitors.capture_stopped(&client, generation, &reservation, now);
            self.applied_holds
                .remove(&(client, generation, reservation));
        }
        let mut changed = false;
        for owner in &mut self.desired.owners {
            let durable = self.durable.owners.iter().find(|entry| {
                entry.client_id == owner.client_id && entry.session_id == owner.session_id
            });
            if !owner.release_requested || durable.is_none_or(|entry| !entry.release_requested) {
                continue;
            }
            let role_retired = match owner.role {
                Role::Input => owner.reservations.is_empty(),
                Role::Monitor => monitors.snapshot(&owner.client_id).is_some_and(|status| {
                    status.generation == owner.generation
                        && status.reference_released
                        && !status.cleanup_pending
                }),
            };
            if role_retired && owner.role_active {
                owner.role_active = false;
                changed = true;
            }
        }
        self.desired.owners.retain(|owner| {
            if owner.role_active || !owner.reservations.is_empty() {
                return true;
            }
            let retired = owner.role == Role::Input
                || monitors.snapshot(&owner.client_id).is_some_and(|status| {
                    // Borrowed targets remain physically present. This proof
                    // retires the owner's source obligations, independently of
                    // whether the source's native device should still exist.
                    status.source_reference_released && !status.cleanup_pending
                });
            if retired {
                changed = true;
            }
            !retired
        });
        if changed {
            self.changed();
        }
    }
}
