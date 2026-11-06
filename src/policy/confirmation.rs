//! Bounded confirmation windows and client/session identity helpers.
use super::*;

impl SessionPolicy {
    pub(super) fn role(&self, client: &str) -> Option<Role> {
        self.owner(client)
            .filter(|owner| owner.role_active)
            .map(|owner| owner.role)
    }

    pub(super) fn owner(&self, client: &str) -> Option<&RetainedOwner> {
        self.desired
            .owners
            .iter()
            .find(|owner| owner.client_id == client)
    }

    pub(super) fn roles_active(&self) -> bool {
        self.desired.owners.iter().any(|owner| owner.role_active)
    }

    pub(super) fn changed(&mut self) {
        self.revision = self
            .revision
            .checked_add(1)
            .expect("owner journal revision exhausted");
    }

    pub(super) fn clear_replacement(&mut self, client: &str) {
        if let Some(state) = self.clients.get_mut(client) {
            state.replacement = None;
        }
    }

    pub(super) fn replacement_pending(&mut self, context: &Context, now: Instant) -> bool {
        let Some(game) = &context.game else {
            self.clear_replacement(&context.client.id);
            return false;
        };
        if self.roles_active() || !self.settings.remote_monitor_confirm_app_replacement {
            self.clear_replacement(&context.client.id);
            return false;
        }
        self.clients
            .get(&context.client.id)
            .and_then(|state| state.replacement.as_ref())
            .is_some_and(|confirmation| {
                confirmation.generation == game.generation
                    && confirmation.game_id == game.application.id
                    && now < confirmation.expires
            })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn confirm(
        &mut self,
        client: &str,
        generation: u64,
        game: &str,
        target: &str,
        terminate: bool,
        now: Instant,
    ) -> bool {
        if self.clients.len() >= 32 && !self.clients.contains_key(client) {
            return false;
        }
        let state = self.clients.entry(client.into()).or_default();
        let slot = if terminate {
            &mut state.terminate
        } else {
            &mut state.replacement
        };
        let confirmed = slot.take().is_some_and(|confirmation| {
            confirmation.generation == generation
                && confirmation.game_id == game
                && confirmation.target_id == target
                && now < confirmation.expires
        });
        if !confirmed {
            *slot = Some(Confirmation {
                generation,
                game_id: game.into(),
                target_id: target.into(),
                expires: now + CONFIRMATION_WINDOW,
            });
        }
        confirmed
    }

    pub(super) fn prune_operations(&mut self, now: Instant) {
        let active: BTreeSet<_> = self
            .desired
            .owners
            .iter()
            .flat_map(|owner| {
                owner.reservations.iter().map(|reservation| {
                    (
                        reservation.consumer.clone(),
                        reservation.borrower_id.clone(),
                        reservation.id.clone(),
                    )
                })
            })
            .collect();
        self.operations.retain(|(consumer, client, id), operation| {
            now < operation.deadline
                || operation
                    .created_owner
                    .as_ref()
                    .is_some_and(|(session, generation)| {
                        self.desired
                            .owners
                            .iter()
                            .chain(&self.durable.owners)
                            .any(|owner| {
                                owner.session_id == *session
                                    && owner.generation == *generation
                                    && owner.role_active
                            })
                    })
                || active.contains(&(
                    consumer.clone(),
                    client.clone(),
                    reservation_id(consumer, client, id),
                ))
        });
    }
}
