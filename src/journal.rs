//! Durable retained-owner intent. Native effects may begin only after the exact
//! journal document has committed, been read back, and its broker result retired.
use amaterasu_module_sdk::{Command, ModuleResult, Response};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const STATE: &str = "amaterasu.state/1";
const KEY: &str = "remote-session.owner-journal.v1";
const TIMEOUT_MS: u64 = 30_000;
const MAX_DOCUMENT_BYTES: usize = 15 * 1024;
const MAX_PAYLOAD_BYTES: usize = 16_384;
const MAX_OWNERS: usize = 32;
const MAX_RESERVATIONS: usize = 32;

enum Purpose {
    Load,
    Store {
        document: Value,
        expected_revision: u64,
    },
    Verify {
        document: Value,
        saved_revision: u64,
    },
}

enum Outcome {
    Payload(Value),
    Failed(String),
}

enum CallState {
    Submit { attempted: bool },
    Waiting(u64),
    Cancelling(u64),
    Releasing { request: u64, outcome: Outcome },
}

struct PendingCall {
    purpose: Purpose,
    payload: Value,
    operation_id: String,
    state: CallState,
}

pub struct OwnerJournal {
    startup_id: String,
    sequence: u64,
    revision: u64,
    snapshot: Value,
    ready: bool,
    pending: Option<PendingCall>,
    completion: Option<Result<(), String>>,
    last_error: Option<String>,
    shutting_down: bool,
}

impl OwnerJournal {
    pub fn new(startup_id: &str) -> Self {
        Self {
            startup_id: startup_id.into(),
            sequence: 0,
            revision: 0,
            snapshot: empty_document(),
            ready: false,
            pending: Some(PendingCall {
                purpose: Purpose::Load,
                payload: json!({"action":"get","key":KEY}),
                operation_id: format!("remote-session.journal.load.{startup_id}.0"),
                state: CallState::Submit { attempted: false },
            }),
            completion: None,
            last_error: None,
            shutting_down: false,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready && !self.shutting_down
    }

    pub fn is_busy(&self) -> bool {
        self.pending.is_some() || self.completion.is_some()
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// This remains the last verified document during persistence. Its retained
    /// owners and reservations are never inferred to have drained after restart.
    pub fn snapshot(&self) -> &Value {
        &self.snapshot
    }

    pub fn begin_store(&mut self, document: Value) -> Result<(), String> {
        validate_document(&document)?;
        if !self.is_ready() {
            return Err("owner_journal_unavailable".into());
        }
        if self.is_busy() {
            return Err("owner_journal_busy".into());
        }
        if document["next_generation"].as_u64() < self.snapshot["next_generation"].as_u64() {
            return Err("owner_journal_generation_regressed".into());
        }
        self.revision
            .checked_add(1)
            .ok_or("owner_journal_revision_exhausted")?;
        let payload =
            json!({"action":"set","key":KEY,"value":document,"expected_revision":self.revision});
        let pending = PendingCall {
            purpose: Purpose::Store {
                document,
                expected_revision: self.revision,
            },
            payload,
            operation_id: self.next_operation_id("store")?,
            state: CallState::Submit { attempted: false },
        };
        self.ready = false;
        self.pending = Some(pending);
        Ok(())
    }

    pub fn take_completion(&mut self) -> Option<Result<(), String>> {
        self.completion.take()
    }

    /// Uses ordinary RuntimeClient::send and explicitly releases terminal calls.
    /// Transport errors retain the exact operation ID until an accepted call can
    /// be recovered; an uncertain result never authorizes native work.
    pub fn advance(
        &mut self,
        send: &mut impl FnMut(Command) -> ModuleResult<Response>,
    ) -> ModuleResult<()> {
        let Some(mut pending) = self.pending.take() else {
            return Ok(());
        };
        let state = std::mem::replace(&mut pending.state, CallState::Submit { attempted: false });
        let mut finished = None;
        match state {
            CallState::Submit { attempted: false } if self.shutting_down => {
                finished = Some(Outcome::Failed("owner_journal_shutting_down".into()));
            }
            CallState::Submit { .. } => {
                pending.state = CallState::Submit { attempted: true };
                match send(Command::Request {
                    service: STATE.into(),
                    payload: pending.payload.clone(),
                    timeout_ms: TIMEOUT_MS,
                    operation_id: pending.operation_id.clone(),
                    parent: None,
                }) {
                    Ok(Response::Accepted { id }) => {
                        pending.state = if self.shutting_down {
                            CallState::Cancelling(id)
                        } else {
                            CallState::Waiting(id)
                        }
                    }
                    Ok(Response::Error { code, message }) => {
                        finished = Some(Outcome::Failed(format!("{code}: {message}")))
                    }
                    Ok(_) | Err(_) => {}
                }
            }
            CallState::Waiting(request) if self.shutting_down => {
                pending.state = CallState::Cancelling(request)
            }
            CallState::Waiting(request) => {
                pending.state = CallState::Waiting(request);
                match send(Command::TakeResult { request }) {
                    Ok(Response::Result { payload }) => {
                        pending.state = CallState::Releasing {
                            request,
                            outcome: Outcome::Payload(payload),
                        }
                    }
                    Ok(Response::Error { code, message }) if code == "not_found" => {
                        finished = Some(Outcome::Failed(format!("{code}: {message}")))
                    }
                    Ok(Response::Error { code, message })
                        if matches!(code.as_str(), "cancelled" | "deadline_exceeded") =>
                    {
                        pending.state = CallState::Releasing {
                            request,
                            outcome: Outcome::Failed(format!("{code}: {message}")),
                        };
                    }
                    Ok(_) | Err(_) => {}
                }
            }
            CallState::Cancelling(request) => {
                pending.state = CallState::Cancelling(request);
                match send(Command::Cancel { request }) {
                    Ok(Response::Ok) => {
                        pending.state = CallState::Releasing {
                            request,
                            outcome: Outcome::Failed("owner_journal_shutting_down".into()),
                        }
                    }
                    Ok(Response::Error { code, .. }) if code == "not_found" => {
                        finished = Some(Outcome::Failed("owner_journal_shutting_down".into()))
                    }
                    Ok(_) | Err(_) => {}
                }
            }
            CallState::Releasing { request, outcome } => {
                match send(Command::ReleaseResult { request }) {
                    Ok(Response::Ok) => finished = Some(outcome),
                    Ok(Response::Error { code, .. }) if code == "not_found" => {
                        finished = Some(outcome)
                    }
                    Ok(_) | Err(_) => pending.state = CallState::Releasing { request, outcome },
                }
            }
        }
        if let Some(outcome) = finished {
            self.finish(pending.purpose, outcome)?;
        } else {
            self.pending = Some(pending);
        }
        Ok(())
    }

    pub fn quiesce(
        &mut self,
        send: &mut impl FnMut(Command) -> ModuleResult<Response>,
    ) -> ModuleResult<bool> {
        self.shutting_down = true;
        self.ready = false;
        self.completion = None;
        self.advance(send)?;
        Ok(self.pending.is_none())
    }

    fn finish(&mut self, purpose: Purpose, outcome: Outcome) -> ModuleResult<()> {
        if self.shutting_down {
            return Ok(());
        }
        match purpose {
            Purpose::Load => match outcome {
                Outcome::Payload(payload) => match parse_document(&payload) {
                    Ok((document, revision)) => {
                        self.snapshot = document;
                        self.revision = revision;
                        self.ready = true;
                        self.last_error = None;
                    }
                    Err(error) => {
                        self.last_error = Some(error);
                        self.reload()?;
                    }
                },
                Outcome::Failed(error) => {
                    self.last_error = Some(error);
                    self.reload()?;
                }
            },
            Purpose::Store {
                document,
                expected_revision,
            } => {
                let saved_revision = expected_revision
                    .checked_add(1)
                    .ok_or("owner journal revision exhausted")?;
                match outcome {
                    Outcome::Payload(payload)
                        if payload["ok"] == true
                            && payload["revision"].as_u64() == Some(saved_revision) =>
                    {
                        self.pending = Some(PendingCall {
                            purpose: Purpose::Verify {
                                document,
                                saved_revision,
                            },
                            payload: json!({"action":"get","key":KEY}),
                            operation_id: self.next_operation_id("verify")?,
                            state: CallState::Submit { attempted: false },
                        });
                    }
                    Outcome::Payload(payload) => self.store_failed(state_error(&payload))?,
                    Outcome::Failed(error) => self.store_failed(error)?,
                }
            }
            Purpose::Verify {
                document,
                saved_revision,
            } => match outcome {
                Outcome::Payload(payload) => match parse_document(&payload) {
                    Ok((saved, revision)) if revision == saved_revision && saved == document => {
                        self.snapshot = saved;
                        self.revision = revision;
                        self.ready = true;
                        self.last_error = None;
                        self.completion = Some(Ok(()));
                    }
                    Ok(_) => self.store_failed("owner_journal_readback_mismatch".into())?,
                    Err(error) => self.store_failed(error)?,
                },
                Outcome::Failed(error) => self.store_failed(error)?,
            },
        }
        Ok(())
    }

    fn store_failed(&mut self, error: String) -> ModuleResult<()> {
        self.last_error = Some(error.clone());
        self.completion = Some(Err(error));
        self.reload()
    }

    fn reload(&mut self) -> ModuleResult<()> {
        self.ready = false;
        self.pending = Some(PendingCall {
            purpose: Purpose::Load,
            payload: json!({"action":"get","key":KEY}),
            operation_id: self.next_operation_id("reload")?,
            state: CallState::Submit { attempted: false },
        });
        Ok(())
    }

    fn next_operation_id(&mut self, stage: &str) -> ModuleResult<String> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("owner journal operation identity exhausted")?;
        Ok(format!(
            "remote-session.journal.{stage}.{}.{}",
            self.startup_id, self.sequence
        ))
    }
}

pub fn validate_document(document: &Value) -> Result<(), String> {
    validate_json_bounds(document)?;
    let object = document.as_object().ok_or("invalid_owner_journal")?;
    let next_generation = document["next_generation"]
        .as_u64()
        .filter(|generation| *generation > 0)
        .ok_or("invalid_owner_journal_generation")?;
    let owners = document["owners"]
        .as_array()
        .ok_or("invalid_owner_journal_owners")?;
    if object.len() != 3 || document["version"] != 1 || owners.len() > MAX_OWNERS {
        return Err("invalid_owner_journal".into());
    }
    let mut clients = BTreeSet::new();
    let mut sessions = BTreeSet::new();
    for owner in owners {
        let owner = owner.as_object().ok_or("invalid_owner_journal_owner")?;
        let client = bounded_identity(owner.get("client_id"))?;
        let session = bounded_identity(owner.get("session_id"))?;
        owner
            .get("generation")
            .and_then(Value::as_u64)
            .filter(|generation| *generation > 0 && *generation < next_generation)
            .ok_or("invalid_owner_journal_generation")?;
        if !clients.insert(client) || !sessions.insert(session) {
            return Err("duplicate_owner_journal_owner".into());
        }
        if let Some(settings) = owner.get("profile_settings")
            && (!settings.is_object()
                || serde_json::to_vec(settings)
                    .map_err(|e| e.to_string())?
                    .len()
                    > 4096)
        {
            return Err("invalid_owner_journal_profile".into());
        }
        if let Some(reservations) = owner.get("reservations") {
            let reservations = reservations
                .as_array()
                .filter(|reservations| reservations.len() <= MAX_RESERVATIONS)
                .ok_or("invalid_owner_journal_reservations")?;
            let mut ids = BTreeSet::new();
            for reservation in reservations {
                let record = reservation
                    .as_object()
                    .ok_or("invalid_owner_journal_reservation")?;
                if !ids.insert(bounded_identity(record.get("id"))?) {
                    return Err("duplicate_owner_journal_reservation".into());
                }
                for field in ["transport_id", "runtime_epoch"] {
                    if let Some(value) = record.get(field)
                        && !value.is_null()
                    {
                        bounded_identity(Some(value))?;
                    }
                }
            }
        }
    }
    let set = json!({"action":"set","key":KEY,"value":document,"expected_revision":u64::MAX});
    let get = json!({"ok":true,"revision":u64::MAX,"found":true,"value":document});
    let snapshot = json!({"ok":true,"revision":u64::MAX,"values":{KEY:document},"next_cursor":{"revision":u64::MAX,"after":KEY}});
    for envelope in [set, get, snapshot] {
        if serde_json::to_vec(&envelope)
            .map_err(|e| e.to_string())?
            .len()
            > MAX_PAYLOAD_BYTES
        {
            return Err("owner_journal_capacity_exhausted".into());
        }
    }
    Ok(())
}

fn validate_json_bounds(document: &Value) -> Result<(), String> {
    let mut pending = vec![(document, 0usize)];
    while let Some((value, depth)) = pending.pop() {
        if depth > 32 {
            return Err("owner_journal_too_deep".into());
        }
        match value {
            Value::Array(values) => pending.extend(values.iter().map(|value| (value, depth + 1))),
            Value::Object(values) => {
                pending.extend(values.values().map(|value| (value, depth + 1)))
            }
            _ => {}
        }
    }
    let bytes = serde_json::to_vec(document).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err("owner_journal_capacity_exhausted".into());
    }
    Ok(())
}

fn bounded_identity(value: Option<&Value>) -> Result<&str, String> {
    value
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
        })
        .ok_or_else(|| "invalid_owner_journal_identity".into())
}

fn empty_document() -> Value {
    json!({"version":1,"next_generation":1,"owners":[]})
}

fn parse_document(payload: &Value) -> Result<(Value, u64), String> {
    if payload["ok"] != true {
        return Err(state_error(payload));
    }
    let revision = payload["revision"]
        .as_u64()
        .ok_or("invalid_owner_journal_revision")?;
    match payload["found"].as_bool() {
        Some(true) => {
            let document = payload
                .get("value")
                .ok_or("missing_owner_journal_document")?;
            validate_document(document)?;
            if revision == 0 {
                return Err("invalid_owner_journal_revision".into());
            }
            Ok((document.clone(), revision))
        }
        Some(false) if revision == 0 && payload.get("value") == Some(&Value::Null) => {
            Ok((empty_document(), 0))
        }
        // A missing journal in a previously written namespace cannot reset the
        // generation ledger while native resources may still exist.
        Some(false) => Err("owner_journal_missing_after_prior_state".into()),
        None => Err("invalid_owner_journal_response".into()),
    }
}

fn state_error(payload: &Value) -> String {
    let code = payload["error"]["code"]
        .as_str()
        .unwrap_or("owner_journal_state_failed");
    let message = payload["error"]["message"]
        .as_str()
        .unwrap_or("State returned an invalid owner journal response");
    format!(
        "{}: {}",
        code.chars().take(128).collect::<String>(),
        message.chars().take(512).collect::<String>()
    )
}

#[cfg(all(test, feature = "core-integration-tests"))]
#[path = "journal/tests.rs"]
mod tests;
