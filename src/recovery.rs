//! Read-only authority and retirement evidence for durable source reservations.
//! Unknown operations and a changed epoch never imply native resource cleanup.
use crate::policy::{RecoveryCandidate, ReservationKind};
use amaterasu_module_sdk::{
    Command, ModuleResult, Response,
    pairing_authority::{self, Authorization},
    requests::RequestOptions,
    retirement::{self, Contract, OperationRef},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

enum CallState {
    Submit {
        attempted: bool,
    },
    Waiting(u64),
    Cancelling(u64),
    Releasing {
        request: u64,
        result: Result<Value, String>,
    },
}

/// One bounded read-only request. A result is returned only after its broker
/// tracking has retired, including lost admissions and cancelled queries.
pub(crate) struct ReadOnlyCall {
    command: Command,
    deadline: Instant,
    state: CallState,
}

impl ReadOnlyCall {
    pub(crate) fn new(command: Command, deadline: Instant) -> Self {
        Self {
            command,
            deadline,
            state: CallState::Submit { attempted: false },
        }
    }

    pub(crate) fn poll(
        &mut self,
        now: Instant,
        stopping: bool,
        send: &mut impl FnMut(Command) -> ModuleResult<Response>,
    ) -> Option<Result<Value, String>> {
        let state = std::mem::replace(&mut self.state, CallState::Submit { attempted: false });
        match state {
            CallState::Submit { attempted: false } if stopping => {
                Some(Err("observer_stopping".into()))
            }
            CallState::Submit { .. } => {
                self.state = CallState::Submit { attempted: true };
                match send(self.command.clone()) {
                    Ok(Response::Accepted { id }) => {
                        self.state = if stopping || now >= self.deadline {
                            CallState::Cancelling(id)
                        } else {
                            CallState::Waiting(id)
                        }
                    }
                    Ok(Response::Error { code, .. }) => return Some(Err(code)),
                    Ok(_) | Err(_) => {}
                }
                None
            }
            CallState::Waiting(request) if stopping || now >= self.deadline => {
                self.state = CallState::Cancelling(request);
                None
            }
            CallState::Waiting(request) => {
                self.state = CallState::Waiting(request);
                match send(Command::TakeResult { request }) {
                    Ok(Response::Result { payload }) => {
                        self.state = CallState::Releasing {
                            request,
                            result: Ok(payload),
                        }
                    }
                    Ok(Response::Error { code, .. }) if code == "not_found" => {
                        return Some(Err(code));
                    }
                    Ok(Response::Error { code, .. })
                        if matches!(code.as_str(), "cancelled" | "deadline_exceeded") =>
                    {
                        self.state = CallState::Releasing {
                            request,
                            result: Err(code),
                        }
                    }
                    Ok(_) | Err(_) => {}
                }
                None
            }
            CallState::Cancelling(request) => {
                self.state = CallState::Cancelling(request);
                match send(Command::Cancel { request }) {
                    Ok(Response::Ok) => {
                        self.state = CallState::Releasing {
                            request,
                            result: Err("observer_cancelled".into()),
                        }
                    }
                    Ok(Response::Error { code, .. }) if code == "not_found" => {
                        return Some(Err("observer_cancelled".into()));
                    }
                    Ok(_) | Err(_) => {}
                }
                None
            }
            CallState::Releasing { request, result } => {
                match send(Command::ReleaseResult { request }) {
                    Ok(Response::Ok) => Some(result),
                    Ok(Response::Error { code, .. }) if code == "not_found" => Some(result),
                    Ok(_) | Err(_) => {
                        self.state = CallState::Releasing { request, result };
                        None
                    }
                }
            }
        }
    }
}

pub(crate) struct Recovery {
    startup_id: String,
    sequence: u64,
    cursor: usize,
    next_poll: Instant,
    pending: Option<(RecoveryCandidate, Contract, ReadOnlyCall)>,
    retired: Option<RecoveryCandidate>,
    last_error: Option<String>,
    shutting_down: bool,
}

impl Recovery {
    pub(crate) fn new(startup_id: &str, now: Instant) -> Self {
        Self {
            startup_id: startup_id.into(),
            sequence: 0,
            cursor: 0,
            next_poll: now,
            pending: None,
            retired: None,
            last_error: None,
            shutting_down: false,
        }
    }
    pub(crate) fn take_retired(&mut self) -> Option<RecoveryCandidate> {
        self.retired.take()
    }
    pub(crate) fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub(crate) fn advance(
        &mut self,
        now: Instant,
        candidates: &[RecoveryCandidate],
        send: &mut impl FnMut(Command) -> ModuleResult<Response>,
    ) -> ModuleResult<()> {
        if self.pending.is_none()
            && self.retired.is_none()
            && !self.shutting_down
            && now >= self.next_poll
            && !candidates.is_empty()
        {
            self.cursor %= candidates.len();
            let candidate = candidates[self.cursor].clone();
            self.cursor = (self.cursor + 1) % candidates.len();
            self.next_poll = now + POLL_INTERVAL;
            let (contract, reference) = match operation_ref(&candidate) {
                Ok(query) => query,
                Err(error) => {
                    self.last_error = Some(error);
                    return Ok(());
                }
            };
            self.sequence = self
                .sequence
                .checked_add(1)
                .ok_or("retirement query identity exhausted")?;
            let operation_id = format!(
                "remote-session.recovery.{}.{}",
                self.startup_id, self.sequence
            );
            let command = retirement::status_command(
                RequestOptions {
                    service: contract.service(),
                    timeout_ms: QUERY_TIMEOUT.as_millis() as u64,
                    operation_id: &operation_id,
                    parent: None,
                },
                contract,
                &reference,
            )?;
            self.pending = Some((
                candidate,
                contract,
                ReadOnlyCall::new(command, now + QUERY_TIMEOUT),
            ));
        }
        if let Some((candidate, contract, mut call)) = self.pending.take() {
            match call.poll(now, self.shutting_down, send) {
                None => self.pending = Some((candidate, contract, call)),
                Some(result) => {
                    self.next_poll = now + POLL_INTERVAL;
                    if !self.shutting_down {
                        match result {
                            Ok(payload)
                                if proves_contract_retired(&candidate, contract, &payload) =>
                            {
                                if contract == Contract::Application {
                                    // An application root retains its original
                                    // transport as well as its execution source.
                                    self.sequence = self
                                        .sequence
                                        .checked_add(1)
                                        .ok_or("retirement query identity exhausted")?;
                                    let reference = transport_ref(&candidate)?;
                                    let operation_id = format!(
                                        "remote-session.recovery.{}.{}",
                                        self.startup_id, self.sequence
                                    );
                                    let command = retirement::status_command(
                                        RequestOptions {
                                            service: Contract::Transport.service(),
                                            timeout_ms: QUERY_TIMEOUT.as_millis() as u64,
                                            operation_id: &operation_id,
                                            parent: None,
                                        },
                                        Contract::Transport,
                                        &reference,
                                    )?;
                                    self.pending = Some((
                                        candidate,
                                        Contract::Transport,
                                        ReadOnlyCall::new(command, now + QUERY_TIMEOUT),
                                    ));
                                } else {
                                    self.last_error = None;
                                    self.retired = Some(candidate);
                                }
                            }
                            Ok(_) => self.last_error = Some("source_retirement_unconfirmed".into()),
                            Err(error) => self.last_error = Some(error),
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn quiesce(
        &mut self,
        now: Instant,
        send: &mut impl FnMut(Command) -> ModuleResult<Response>,
    ) -> ModuleResult<bool> {
        self.shutting_down = true;
        self.retired = None;
        self.advance(now, &[], send)?;
        Ok(self.pending.is_none())
    }
}

/// Pairing observations fence restored sources until their exact certificate
/// record is checked. Later revocations are observed without granting access.
pub(crate) struct Membership {
    startup_id: String,
    sequence: u64,
    cursor: usize,
    next_poll: Instant,
    pending: Option<(String, ReadOnlyCall)>,
    observed: Option<Authorization>,
    authorizations: BTreeMap<String, Authorization>,
    last_error: Option<String>,
    shutting_down: bool,
}

impl Membership {
    pub(crate) fn new(startup_id: &str, now: Instant) -> Self {
        Self {
            startup_id: startup_id.into(),
            sequence: 0,
            cursor: 0,
            next_poll: now,
            pending: None,
            observed: None,
            authorizations: BTreeMap::new(),
            last_error: None,
            shutting_down: false,
        }
    }
    pub(crate) fn take_observation(&mut self) -> Option<Authorization> {
        self.observed.take()
    }
    pub(crate) fn authorization(&self, client: &str) -> Option<&Authorization> {
        self.authorizations.get(client)
    }
    pub(crate) fn ready_for(&self, clients: &[String]) -> bool {
        clients
            .iter()
            .all(|client| self.authorizations.contains_key(client))
    }
    pub(crate) fn source_authorizations(&self) -> BTreeMap<String, bool> {
        self.authorizations
            .iter()
            .map(|(id, authority)| (id.clone(), authority.paired && authority.may_view))
            .collect()
    }
    pub(crate) fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub(crate) fn advance(
        &mut self,
        now: Instant,
        clients: &[String],
        send: &mut impl FnMut(Command) -> ModuleResult<Response>,
    ) -> ModuleResult<()> {
        self.authorizations.retain(|id, _| clients.contains(id));
        if self.pending.is_none()
            && self.observed.is_none()
            && !self.shutting_down
            && now >= self.next_poll
            && !clients.is_empty()
        {
            self.cursor %= clients.len();
            let client = clients
                .iter()
                .find(|client| !self.authorizations.contains_key(*client))
                .cloned()
                .unwrap_or_else(|| clients[self.cursor].clone());
            self.cursor = (self.cursor + 1) % clients.len();
            self.next_poll = now + POLL_INTERVAL;
            self.sequence = self
                .sequence
                .checked_add(1)
                .ok_or("membership query identity exhausted")?;
            let operation_id = format!(
                "remote-session.membership.{}.{}",
                self.startup_id, self.sequence
            );
            let request = pairing_authority::Request::ClientAuthorization {
                certificate_id: client.clone(),
            };
            let command = match pairing_authority::command(
                RequestOptions {
                    service: pairing_authority::SERVICE,
                    timeout_ms: QUERY_TIMEOUT.as_millis() as u64,
                    operation_id: &operation_id,
                    parent: None,
                },
                &request,
            ) {
                Ok(command) => command,
                Err(error) => {
                    self.last_error = Some(error);
                    return Ok(());
                }
            };
            self.pending = Some((client, ReadOnlyCall::new(command, now + QUERY_TIMEOUT)));
        }
        if let Some((client, mut call)) = self.pending.take() {
            match call.poll(now, self.shutting_down, send) {
                None => self.pending = Some((client, call)),
                Some(result) => {
                    self.next_poll = now + POLL_INTERVAL;
                    if !self.shutting_down {
                        match result.and_then(|payload| {
                            Authorization::parse(&client, &payload).map_err(str::to_owned)
                        }) {
                            Ok(authority) if clients.contains(&client) => {
                                self.last_error = None;
                                self.authorizations.insert(client, authority.clone());
                                self.observed = Some(authority);
                                if clients
                                    .iter()
                                    .any(|client| !self.authorizations.contains_key(client))
                                {
                                    self.next_poll = now;
                                }
                            }
                            Ok(_) => {}
                            Err(error) => {
                                self.authorizations.remove(&client);
                                self.last_error = Some(error);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn quiesce(
        &mut self,
        now: Instant,
        send: &mut impl FnMut(Command) -> ModuleResult<Response>,
    ) -> ModuleResult<bool> {
        self.shutting_down = true;
        self.observed = None;
        self.advance(now, &[], send)?;
        Ok(self.pending.is_none())
    }
}

fn operation_ref(candidate: &RecoveryCandidate) -> Result<(Contract, OperationRef), String> {
    if candidate
        .application_generation
        .is_some_and(|generation| generation == 0)
    {
        return Err("invalid_application_generation".into());
    }
    match candidate.kind {
        ReservationKind::Application => {
            let operation_id = candidate
                .application_operation_id
                .clone()
                .ok_or("application_retirement_binding_missing")?;
            let epoch = candidate
                .application_epoch
                .clone()
                .ok_or("application_retirement_epoch_missing")?;
            if candidate.application_generation.is_none() {
                return Err("application_retirement_binding_mismatch".into());
            }
            let reference = OperationRef {
                operation_id,
                epoch,
            };
            reference.validate(Contract::Application)?;
            Ok((Contract::Application, reference))
        }
        ReservationKind::Auxiliary | ReservationKind::Borrowed => {
            Ok((Contract::Transport, transport_ref(candidate)?))
        }
    }
}

fn proves_contract_retired(
    candidate: &RecoveryCandidate,
    contract: Contract,
    payload: &Value,
) -> bool {
    let reference = if contract == Contract::Transport {
        transport_ref(candidate)
    } else {
        operation_ref(candidate).map(|(_, reference)| reference)
    };
    let Ok(reference) = reference else {
        return false;
    };
    let Ok(status) = retirement::parse_status(contract, &reference, payload) else {
        return false;
    };
    status.retired
        && (contract != Contract::Transport || status.startup_reconciled)
        && status
            .application_generation
            .is_none_or(|generation| candidate.application_generation == Some(generation))
}

fn transport_ref(candidate: &RecoveryCandidate) -> Result<OperationRef, String> {
    let reference = OperationRef {
        operation_id: candidate.transport_id.clone(),
        epoch: candidate.runtime_epoch.clone(),
    };
    reference.validate(Contract::Transport)?;
    Ok(reference)
}

#[cfg(test)]
#[path = "recovery/tests.rs"]
mod tests;
