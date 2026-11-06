//! Idempotent public action admission and ordinary application replacement.
use super::*;

impl SessionPolicy {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn dispatch(
        &mut self,
        caller: &str,
        context: Context,
        operation_id: String,
        intent: DispatchIntent,
        selection: Option<Selection>,
        mode: Option<Mode>,
        now: Instant,
        monitors: &mut impl Monitors,
    ) -> Reply {
        let fingerprint: [u8; 32] = Sha256::digest(
            serde_json::to_vec(&(&context, &intent, &selection, &mode))
                .expect("request serializes"),
        )
        .into();
        let key = (
            caller.to_owned(),
            context.client.id.clone(),
            operation_id.clone(),
        );
        if let Some(operation) = self.operations.get(&key) {
            if operation.fingerprint != fingerprint {
                return error(
                    "operation_conflict",
                    "Remote session request changed while pending",
                );
            }
            if operation.canceled {
                let terminal = operation.terminal_decision.clone();
                let result =
                    self.cancel_dispatch(caller, &context.client.id, &operation_id, now, monitors);
                return match (result, terminal) {
                    (Reply::Acknowledged {}, Some(decision)) => Reply::Dispatched { decision },
                    (reply, _) => reply,
                };
            }
            if let Some(decision) = &operation.decision {
                let decision = decision.clone();
                let result = self.deliver_decision(
                    caller,
                    &context.client.id,
                    &operation_id,
                    decision,
                    monitors,
                );
                return self.finish_failure(
                    caller,
                    &context.client.id,
                    &operation_id,
                    result,
                    now,
                    monitors,
                );
            }
        } else {
            self.prune_operations(now);
            if self.operations.len() >= MAX_OPERATIONS {
                return error(
                    "operation_capacity_exhausted",
                    "Too many remote session requests are pending",
                );
            }
            self.operations.insert(
                key.clone(),
                Operation {
                    fingerprint,
                    deadline: now + CONFIRMATION_WINDOW,
                    decision: None,
                    canceled: false,
                    created_owner: None,
                    replacement_authorized: false,
                    terminal_decision: None,
                    committed: false,
                    auxiliary_target: None,
                },
            );
        }
        if !context.client.paired {
            return error(
                "unauthenticated_client",
                "Paired transport authentication is required",
            );
        }
        let pending_auxiliary = self
            .operations
            .get(&key)
            .and_then(|operation| operation.auxiliary_target.clone());
        let (mut decision, created) = if let Some((session, generation, role)) = pending_auxiliary {
            if !self
                .owner(&context.client.id)
                .is_some_and(|owner| owner.session_id == session && owner.generation == generation)
            {
                (
                    Some(reject(
                        409,
                        "The accepted remote session changed before startup completed",
                    )),
                    false,
                )
            } else {
                self.auxiliary_decision(
                    caller,
                    &context,
                    &operation_id,
                    role,
                    None,
                    mode.expect("validated auxiliary mode"),
                    now,
                    monitors,
                )
            }
        } else {
            self.decide(
                caller,
                &context,
                &operation_id,
                intent,
                selection.as_ref(),
                mode,
                now,
                monitors,
            )
        };
        if let Some(Decision::Auxiliary { plan }) = &decision {
            let resume = intent == DispatchIntent::Resume
                || selection
                    .as_ref()
                    .and_then(identify)
                    .is_some_and(|control| {
                        matches!(control, Control::Resume | Control::RunningGame)
                    });
            if resume {
                decision = Some(Decision::ResumeAuxiliary { plan: plan.clone() });
            }
        }
        let created_owner = created
            .then(|| {
                self.owner(&context.client.id)
                    .map(|owner| (owner.session_id.clone(), owner.generation))
            })
            .flatten();
        let auxiliary_target = self
            .owner(&context.client.id)
            .filter(|owner| {
                owner.reservations.iter().any(|entry| {
                    entry.kind == ReservationKind::Auxiliary
                        && entry.consumer == caller
                        && entry.id == reservation_id(caller, &context.client.id, &operation_id)
                })
            })
            .map(|owner| (owner.session_id.clone(), owner.generation, owner.role));
        let Some(operation) = self.operations.get_mut(&key) else {
            return error(
                "operation_not_found",
                "Remote session request is unavailable",
            );
        };
        if created_owner.is_some() {
            operation.created_owner = created_owner;
        }
        if auxiliary_target.is_some() {
            operation.auxiliary_target = auxiliary_target;
        }
        if let Some(decision) = decision {
            operation.decision = Some(decision.clone());
            let result = self.deliver_decision(
                caller,
                &context.client.id,
                &operation_id,
                decision,
                monitors,
            );
            self.finish_failure(
                caller,
                &context.client.id,
                &operation_id,
                result,
                now,
                monitors,
            )
        } else {
            pending()
        }
    }

    fn finish_failure(
        &mut self,
        caller: &str,
        client: &str,
        operation: &str,
        result: Reply,
        now: Instant,
        monitors: &mut impl Monitors,
    ) -> Reply {
        let Reply::Dispatched {
            decision: Decision::Reject { .. },
        } = &result
        else {
            return result;
        };
        let key = (caller.to_owned(), client.to_owned(), operation.to_owned());
        let owns_created = self
            .operations
            .get(&key)
            .is_some_and(|operation| operation.created_owner.is_some());
        let reservation = reservation_id(caller, client, operation);
        let owns_reservation = self.desired.owners.iter().any(|owner| {
            owner.reservations.iter().any(|entry| {
                entry.id == reservation && entry.borrower_id == client && entry.consumer == caller
            })
        });
        if !owns_created && !owns_reservation {
            return result;
        }
        let Reply::Dispatched { decision } = result else {
            unreachable!()
        };
        if let Some(pending) = self.operations.get_mut(&key) {
            pending.terminal_decision = Some(decision.clone());
            pending.canceled = true;
        }
        match self.cancel_dispatch(caller, client, operation, now, monitors) {
            Reply::Acknowledged {} => Reply::Dispatched { decision },
            reply => reply,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn decide(
        &mut self,
        caller: &str,
        context: &Context,
        operation: &str,
        intent: DispatchIntent,
        selection: Option<&Selection>,
        mode: Option<Mode>,
        now: Instant,
        monitors: &mut impl Monitors,
    ) -> (Option<Decision>, bool) {
        if intent == DispatchIntent::Cancel {
            return (Some(self.cancel_decision(context)), false);
        }
        let selected = selection.cloned().unwrap_or_else(|| Selection {
            id: Control::Resume.id(false),
            uuid: None,
        });
        let catalog = self.projections.catalog(caller);
        let applications = catalog.as_deref().map(Vec::as_slice).unwrap_or(&[]);
        let cached = self
            .cache
            .resolve(&context.client.id, &selected, applications);
        if cached == Resolution::Unavailable {
            return (
                Some(reject(
                    404,
                    "The application for this cached tile is no longer available",
                )),
                false,
            );
        }
        let cached_id = match cached {
            Resolution::Target(id) => Some(id),
            _ => None,
        };
        let mut control = identify(&selected);
        if cached_id.as_ref().is_some_and(|id| {
            context
                .game
                .as_ref()
                .is_none_or(|game| game.application.id != *id)
        }) {
            control = None;
        }
        let configured = applications.iter().find(|application| {
            application.id == selected.id
                || application.aliases.contains(&selected.id)
                || selected
                    .uuid
                    .as_deref()
                    .is_some_and(|uuid| !uuid.is_empty() && application.uuid == uuid)
        });
        if control.is_none()
            && let Some(id) = &cached_id
            && context
                .game
                .as_ref()
                .is_some_and(|game| game.application.id == *id)
        {
            control = Some(Control::RunningGame);
        }
        if control.is_none()
            && configured.is_some_and(|application| valid_profile(application, applications))
        {
            control = Some(Control::Monitor);
        }
        if let Some(control) = control {
            return self.control_decision(
                caller,
                context,
                operation,
                control,
                configured,
                mode.expect("validated streaming mode"),
                now,
                monitors,
            );
        }
        if !context.client.may_launch {
            return (Some(reject(403, "Launch permission denied")), false);
        }
        let application_id = cached_id.or_else(|| {
            configured
                .filter(|application| application.visible)
                .map(|application| application.id.clone())
        });
        let Some(application_id) = application_id else {
            return (
                Some(reject(
                    if catalog.is_some() { 404 } else { 503 },
                    if catalog.is_some() {
                        "The requested application was not found"
                    } else {
                        "The complete application inventory is not ready"
                    },
                )),
                false,
            );
        };
        if let Some(game) = &context.game {
            if self.roles_active() {
                self.clear_replacement(&context.client.id);
                return (
                    Some(reject(
                        409,
                        "A remote session is connected. Use Terminate before replacing the current app.",
                    )),
                    false,
                );
            }
            let key = (
                caller.to_owned(),
                context.client.id.clone(),
                operation.to_owned(),
            );
            let already_authorized = self
                .operations
                .get(&key)
                .is_some_and(|operation| operation.replacement_authorized);
            if self.settings.remote_monitor_confirm_app_replacement
                && !already_authorized
                && !self.confirm(
                    &context.client.id,
                    game.generation,
                    &game.application.id,
                    &application_id,
                    false,
                    now,
                )
            {
                return (
                    Some(reject(
                        410,
                        "Another app is running. Launch the selected app again within 60 seconds to confirm replacement.",
                    )),
                    false,
                );
            }
            if !self.settings.remote_monitor_confirm_app_replacement {
                self.clear_replacement(&context.client.id);
            }
            if let Some(operation) = self.operations.get_mut(&key) {
                operation.replacement_authorized = true;
            }
            let source = match self.borrow_source(
                caller,
                context,
                operation,
                mode.expect("validated mode"),
                monitors,
            ) {
                Ok(source) => source,
                Err(_) => return (None, false),
            };
            return (
                Some(Decision::ReplaceApplication {
                    application_id,
                    expected_current_generation: game.generation,
                    source,
                }),
                false,
            );
        }
        let source = match self.borrow_source(
            caller,
            context,
            operation,
            mode.expect("validated mode"),
            monitors,
        ) {
            Ok(source) => source,
            Err(_) => return (None, false),
        };
        (
            Some(Decision::LaunchApplication {
                application_id,
                source,
            }),
            false,
        )
    }

    pub(super) fn cancel_decision(&self, context: &Context) -> Decision {
        if !context.client.may_terminate {
            return reject(403, "Terminate permission denied");
        }
        if let Some(owner) = self
            .owner(&context.client.id)
            .filter(|owner| owner.role_active)
        {
            return Decision::StopAuxiliary {
                session_id: owner.session_id.clone(),
                generation: owner.generation,
                completion: ControlCompletion {
                    status_code: 200,
                    message: "Remote session disconnected".into(),
                },
            };
        }
        let Some(game) = &context.game else {
            return Decision::Complete {
                status_code: 200,
                message: "Application is already stopped".into(),
            };
        };
        if self.roles_active() && game.owner_client_id != context.client.id {
            return reject(
                403,
                "Only the running-game owner may cancel this game while remote sessions are active",
            );
        }
        Decision::StopApplication {
            application_id: game.application.id.clone(),
            generation: game.generation,
            completion: ControlCompletion {
                status_code: 200,
                message: "Application stopped".into(),
            },
        }
    }
}
