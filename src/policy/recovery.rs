//! Scoped, durable retirement work and independent proof reconciliation.
use super::*;

impl SessionPolicy {
    pub(crate) fn recovery_candidates(&self) -> Vec<RecoveryCandidate> {
        self.desired
            .owners
            .iter()
            .flat_map(|owner| {
                owner.reservations.iter().filter_map(|reservation| {
                    if !self
                        .recovering
                        .contains(&(owner.session_id.clone(), reservation.id.clone()))
                    {
                        return None;
                    }
                    Some(RecoveryCandidate {
                        client_id: owner.client_id.clone(),
                        borrower_id: reservation.borrower_id.clone(),
                        consumer: reservation.consumer.clone(),
                        session_id: owner.session_id.clone(),
                        generation: owner.generation,
                        reservation_id: reservation.id.clone(),
                        transport_id: reservation.transport_id.clone()?,
                        runtime_epoch: reservation.runtime_epoch.clone()?,
                        kind: reservation.kind,
                        application_operation_id: reservation.application_operation_id.clone(),
                        application_epoch: reservation.application_epoch.clone(),
                        application_generation: reservation.application_generation,
                    })
                })
            })
            .collect()
    }

    pub(crate) fn accept_retired(&mut self, candidate: &RecoveryCandidate) {
        let Some(owner) = self.desired.owners.iter_mut().find(|owner| {
            owner.client_id == candidate.client_id
                && owner.session_id == candidate.session_id
                && owner.generation == candidate.generation
        }) else {
            return;
        };
        let before = owner.reservations.len();
        owner.reservations.retain(|entry| {
            !(entry.id == candidate.reservation_id
                && entry.borrower_id == candidate.borrower_id
                && entry.consumer == candidate.consumer
                && entry.kind == candidate.kind
                && entry.transport_id.as_deref() == Some(&candidate.transport_id)
                && entry.runtime_epoch.as_deref() == Some(&candidate.runtime_epoch)
                && entry.application_operation_id == candidate.application_operation_id
                && entry.application_epoch == candidate.application_epoch
                && entry.application_generation == candidate.application_generation)
        });
        if owner.reservations.len() != before {
            if candidate.kind == ReservationKind::Auxiliary
                && !owner
                    .reservations
                    .iter()
                    .any(|entry| entry.kind == ReservationKind::Auxiliary)
                && (owner.role == Role::Input
                    || self.settings.remote_monitor_disconnect_on_stream_end
                    || self.settings.remote_monitor_disconnect_on_client_disconnect)
            {
                owner.release_requested = true;
            }
            self.recovering.remove(&(
                candidate.session_id.clone(),
                candidate.reservation_id.clone(),
            ));
            if self.settled_work.len() >= 1024 {
                self.settled_work.pop_front();
            }
            self.settled_work
                .push_back((candidate.consumer.clone(), recovery_id(candidate)));
            self.changed();
        }
    }

    pub(crate) fn shutdown(&mut self) {
        let mut changed = false;
        for owner in &mut self.desired.owners {
            if !owner.release_requested {
                owner.release_requested = true;
                changed = true;
            }
            for reservation in &mut owner.reservations {
                if reservation.transport_id.is_some() {
                    self.recovering
                        .insert((owner.session_id.clone(), reservation.id.clone()));
                    if !reservation.retirement_requested {
                        reservation.retirement_requested = true;
                        changed = true;
                    }
                }
            }
            let before = owner.reservations.len();
            owner
                .reservations
                .retain(|entry| entry.transport_id.is_some());
            changed |= before != owner.reservations.len();
        }
        if changed {
            self.changed();
        }
    }

    pub(crate) fn is_retired(&self) -> bool {
        self.desired.owners.is_empty()
            && self.durable.owners.is_empty()
            && !self.needs_persistence()
    }

    pub(crate) fn authority_clients(&self) -> Vec<String> {
        self.desired
            .owners
            .iter()
            .flat_map(|owner| {
                std::iter::once(owner.client_id.clone()).chain(
                    owner
                        .reservations
                        .iter()
                        .map(|entry| entry.borrower_id.clone()),
                )
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub(crate) fn revoke_restored_client(
        &mut self,
        client: &str,
        now: Instant,
        monitors: &mut impl Monitors,
    ) -> Reply {
        self.revoke_client(None, client, now, monitors)
    }

    pub(super) fn recovery_work(&self, caller: &str, cursor: Option<&str>, limit: usize) -> Reply {
        if self.needs_persistence() {
            return pending();
        }
        if !(1..=MAX_RECOVERY_ITEMS).contains(&limit) {
            return error("invalid_recovery_limit", "Recovery page limit is invalid");
        }
        let offset = match cursor {
            None => 0,
            Some(cursor) => match cursor
                .strip_prefix("recovery-")
                .and_then(|cursor| cursor.split_once('-'))
                .and_then(|(revision, offset)| {
                    Some((revision.parse::<u64>().ok()?, offset.parse::<usize>().ok()?))
                }) {
                Some((revision, offset)) if revision == self.durable_revision => offset,
                _ => {
                    return error(
                        "recovery_cursor_changed",
                        "Recovery work changed; restart the scan",
                    );
                }
            },
        };
        let candidates: Vec<_> = self
            .recovery_candidates()
            .into_iter()
            .filter(|candidate| {
                candidate.consumer == caller
                    && self
                        .durable
                        .owners
                        .iter()
                        .find(|owner| {
                            owner.session_id == candidate.session_id
                                && owner.generation == candidate.generation
                        })
                        .and_then(|owner| {
                            owner
                                .reservations
                                .iter()
                                .find(|entry| entry.id == candidate.reservation_id)
                        })
                        .is_some_and(|entry| entry.retirement_requested)
            })
            .collect();
        let mut work = Vec::new();
        for candidate in &candidates {
            let make_item = |suffix: &str, binding: RecoveryBinding| RecoveryWorkItem {
                id: format!("{}-{suffix}", recovery_id(candidate)),
                client_id: candidate.borrower_id.clone(),
                session_id: candidate.session_id.clone(),
                generation: candidate.generation,
                reservation_id: candidate.reservation_id.clone(),
                binding,
                budget_ms: MAX_RECOVERY_BUDGET_MS,
            };
            if candidate.kind == ReservationKind::Application {
                let (Some(operation_id), Some(epoch), Some(application_generation)) = (
                    &candidate.application_operation_id,
                    &candidate.application_epoch,
                    candidate.application_generation,
                ) else {
                    return error(
                        "application_binding_missing",
                        "Application retirement proof is unavailable",
                    );
                };
                work.push(make_item(
                    "app",
                    RecoveryBinding::Application {
                        operation_id: operation_id.clone(),
                        epoch: epoch.clone(),
                        application_generation,
                    },
                ));
            }
            work.push(make_item(
                "transport",
                RecoveryBinding::Transport {
                    operation_id: candidate.transport_id.clone(),
                    epoch: candidate.runtime_epoch.clone(),
                },
            ));
        }
        if offset > work.len() {
            return error("invalid_recovery_cursor", "Recovery page range is invalid");
        }
        let end = (offset + limit).min(work.len());
        let items = work[offset..end].to_vec();
        let next_cursor =
            (end < work.len()).then(|| format!("recovery-{}-{end}", self.durable_revision));
        Reply::RecoveryWork { items, next_cursor }
    }

    pub(super) fn reconcile_recovery(&self, caller: &str, id: &str) -> Reply {
        let Some((id, kind)) = id
            .strip_suffix("-app")
            .map(|id| (id, true))
            .or_else(|| id.strip_suffix("-transport").map(|id| (id, false)))
        else {
            return error(
                "recovery_work_not_found",
                "Recovery work identity is invalid",
            );
        };
        if self.recovery_candidates().iter().any(|candidate| {
            candidate.consumer == caller
                && recovery_id(candidate) == id
                && (!kind || candidate.kind == ReservationKind::Application)
        }) {
            return pending();
        }
        if self
            .settled_work
            .iter()
            .any(|(consumer, work)| consumer == caller && work == id)
        {
            let still_stored = self.durable.owners.iter().any(|owner| {
                owner
                    .reservations
                    .iter()
                    .filter_map(|entry| candidate_from(owner, entry))
                    .any(|candidate| candidate.consumer == caller && recovery_id(&candidate) == id)
            });
            return if still_stored {
                pending()
            } else {
                Reply::Acknowledged {}
            };
        }
        error(
            "recovery_work_not_found",
            "Recovery work is no longer available for this consumer",
        )
    }
}
