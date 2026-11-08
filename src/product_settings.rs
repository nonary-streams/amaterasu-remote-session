//! Caller-scoped product preferences persisted through the selected Configuration
//! provider. One pending call and one management completion bound retained work.
use crate::settings::ProductSettings;
use amaterasu_module_sdk::{Command, ModuleResult, Response};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const CONFIGURATION: &str = "amaterasu.configuration/1";
const DOCUMENT_VERSION: u32 = 1;
const TIMEOUT_MS: u64 = 30_000;
const LOOKUP_RETENTION: Duration = Duration::from_secs(300);

enum Purpose {
    Load,
    Update {
        request: u64,
        expected_revision: u64,
        desired: ProductSettings,
    },
}

impl Purpose {
    fn parent(&self) -> Option<u64> {
        match self {
            Self::Load => None,
            Self::Update { request, .. } => Some(*request),
        }
    }
}

enum Outcome {
    Payload(Value),
    Failed(Value),
}

enum CallState {
    Submit { attempted: bool },
    Waiting(u64),
    CancellingRead(u64),
    Releasing { request: u64, outcome: Outcome },
}

struct PendingCall {
    purpose: Purpose,
    payload: Value,
    operation_id: String,
    state: CallState,
    abandoned: bool,
    first_sent: Option<Instant>,
}

/// Startup authorization is deliberately absent from this document. Initial
/// preferences apply only when no saved document exists; later packages cannot
/// replace a user's durable preferences by changing their startup defaults.
pub struct ProductSettingsStore {
    startup_id: String,
    defaults: ProductSettings,
    settings: ProductSettings,
    revision: u64,
    ready: bool,
    sequence: u64,
    pending: Option<PendingCall>,
    completion: Option<(u64, Value)>,
    shutting_down: bool,
}

impl ProductSettingsStore {
    pub fn new(startup_id: &str, defaults: ProductSettings) -> Self {
        Self {
            startup_id: startup_id.to_owned(),
            defaults: defaults.clone(),
            settings: defaults,
            revision: 0,
            ready: false,
            sequence: 0,
            pending: Some(PendingCall {
                purpose: Purpose::Load,
                payload: json!({"action":"product_get"}),
                operation_id: format!("remote-session.settings.load.{startup_id}.0"),
                state: CallState::Submit { attempted: false },
                abandoned: false,
                first_sent: None,
            }),
            completion: None,
            shutting_down: false,
        }
    }

    pub fn settings(&self) -> (&ProductSettings, u64) {
        (&self.settings, self.revision)
    }

    pub fn is_ready(&self) -> bool {
        self.ready && !self.shutting_down
    }

    pub fn begin_update(
        &mut self,
        request: u64,
        expected_revision: u64,
        desired: ProductSettings,
    ) -> Result<(), Value> {
        if !self.is_ready() {
            return Err(failure(
                "configuration_unavailable",
                "remote-session preferences are not loaded",
            ));
        }
        if self.pending.is_some() || self.completion.is_some() {
            return Err(failure(
                "settings_busy",
                "a settings update is still being settled",
            ));
        }
        desired
            .validate()
            .map_err(|message| failure("invalid_request", message))?;
        if expected_revision != self.revision {
            return Err(
                json!({"ok":false,"code":"revision_conflict","current_revision":self.revision}),
            );
        }
        expected_revision
            .checked_add(1)
            .ok_or_else(|| failure("resource_exhausted", "settings revision is exhausted"))?;
        let operation_id = self
            .next_operation_id("update")
            .map_err(|message| failure("resource_exhausted", &message))?;
        self.pending = Some(PendingCall {
            payload: json!({
                "action":"product_update", "version":DOCUMENT_VERSION,
                "expected_revision":expected_revision, "values":desired,
            }),
            purpose: Purpose::Update {
                request,
                expected_revision,
                desired,
            },
            operation_id,
            state: CallState::Submit { attempted: false },
            abandoned: false,
            first_sent: None,
        });
        Ok(())
    }

    pub fn take_completion(&mut self) -> Option<(u64, Value)> {
        self.completion.take()
    }

    /// Advances one broker call without blocking. Call with ordinary `send`;
    /// this store explicitly retires every result, including cancelled calls.
    pub fn advance(
        &mut self,
        send: &mut impl FnMut(Command) -> ModuleResult<Response>,
    ) -> ModuleResult<()> {
        let Some(mut pending) = self.pending.take() else {
            return Ok(());
        };
        if let Some(parent) = pending.purpose.parent()
            && !pending.abandoned
            && !self.shutting_down
        {
            match send(Command::CheckRequest { request: parent }) {
                Ok(Response::Ok) => {}
                Ok(Response::Error { .. }) => pending.abandoned = true,
                Ok(_) | Err(_) => {
                    self.pending = Some(pending);
                    return Ok(());
                }
            }
        }
        if self.shutting_down {
            pending.abandoned = true;
        }
        if pending.abandoned && matches!(pending.purpose, Purpose::Update { .. }) {
            // The write may still commit after its caller has gone away.
            self.ready = false;
        }
        let state = std::mem::replace(&mut pending.state, CallState::Submit { attempted: false });
        let mut finished = None;
        match state {
            CallState::Submit { attempted } if pending.abandoned && !attempted => {
                finished = Some(Outcome::Failed(failure(
                    "request_ended",
                    "settings request ended before persistence",
                )));
            }
            CallState::Submit { attempted } => {
                pending.state = CallState::Submit { attempted: true };
                let command = if attempted {
                    Command::LookupRequest {
                        service: CONFIGURATION.into(),
                        payload: pending.payload.clone(),
                        operation_id: pending.operation_id.clone(),
                        parent: pending.purpose.parent(),
                    }
                } else {
                    pending.first_sent = Some(Instant::now());
                    Command::Request {
                        service: CONFIGURATION.into(),
                        payload: pending.payload.clone(),
                        timeout_ms: TIMEOUT_MS,
                        operation_id: pending.operation_id.clone(),
                        parent: pending.purpose.parent(),
                    }
                };
                match send(command) {
                    Ok(Response::Accepted { id }) => pending.state = CallState::Waiting(id),
                    Ok(Response::Error { code, message }) if !attempted && code != "conflict" => {
                        finished = Some(Outcome::Failed(failure(&code, &message)))
                    }
                    Ok(Response::Error { code, .. }) if !attempted && code == "conflict" => {
                        // An older operation may own this identity; its age is unknown.
                        pending.first_sent = None;
                        if matches!(pending.purpose, Purpose::Update { .. }) {
                            self.ready = false;
                        }
                    }
                    Ok(Response::Error { code, .. })
                        if attempted
                            && code == "not_found"
                            && pending
                                .first_sent
                                .is_some_and(|first| first.elapsed() < LOOKUP_RETENTION) =>
                    {
                        finished = Some(Outcome::Failed(failure(
                            "request_ended",
                            "settings request was not admitted",
                        )));
                    }
                    Ok(_) | Err(_) => {
                        if matches!(pending.purpose, Purpose::Update { .. }) {
                            self.ready = false;
                        }
                    }
                }
            }
            CallState::Waiting(request)
                if pending.abandoned && matches!(pending.purpose, Purpose::Load) =>
            {
                pending.state = CallState::CancellingRead(request);
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
                    Ok(Response::Error { code, message })
                        if matches!(pending.purpose, Purpose::Load)
                            && matches!(code.as_str(), "cancelled" | "deadline_exceeded") =>
                    {
                        pending.state = CallState::Releasing {
                            request,
                            outcome: Outcome::Failed(failure(&code, &message)),
                        };
                    }
                    Ok(Response::Error { code, message })
                        if matches!(pending.purpose, Purpose::Load) && code == "not_found" =>
                    {
                        finished = Some(Outcome::Failed(failure(&code, &message)));
                    }
                    Ok(Response::Pending) => {}
                    // Broker cancellation, expiry and absence do not prove an
                    // already delivered settings write finished executing.
                    Ok(_) | Err(_) => {
                        if matches!(pending.purpose, Purpose::Update { .. }) {
                            self.ready = false;
                        }
                    }
                }
            }
            CallState::CancellingRead(request) => {
                pending.state = CallState::CancellingRead(request);
                match send(Command::Cancel { request }) {
                    Ok(Response::Ok) => {
                        pending.state = CallState::Releasing {
                            request,
                            outcome: Outcome::Failed(failure(
                                "request_ended",
                                "settings read ended",
                            )),
                        };
                    }
                    Ok(Response::Error { code, .. }) if code == "not_found" => {
                        finished = Some(Outcome::Failed(failure(
                            "request_ended",
                            "settings read ended",
                        )));
                    }
                    _ => {}
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
            self.finish(pending.purpose, outcome, pending.abandoned)?;
        } else {
            self.pending = Some(pending);
        }
        Ok(())
    }

    /// Keep dependencies alive until this returns true. Unknown admissions are
    /// looked up without replay and kept until provider completion and release.
    pub fn quiesce(
        &mut self,
        send: &mut impl FnMut(Command) -> ModuleResult<Response>,
    ) -> ModuleResult<bool> {
        self.shutting_down = true;
        self.ready = false;
        self.advance(send)?;
        // The module must deliver any settled management update before it can
        // retire. `advance` may produce this completion while quiescing.
        Ok(self.pending.is_none() && self.completion.is_none())
    }

    fn finish(&mut self, purpose: Purpose, outcome: Outcome, abandoned: bool) -> ModuleResult<()> {
        if self.shutting_down {
            if let Purpose::Update {
                request,
                expected_revision,
                desired,
            } = purpose
            {
                let response = match outcome {
                    Outcome::Payload(payload) => match parse_document(&payload, &self.defaults) {
                        Some((settings, revision))
                            if expected_revision.checked_add(1) == Some(revision)
                                && settings == desired =>
                        {
                            self.settings = settings.clone();
                            self.revision = revision;
                            json!({"ok":true,"settings":settings,"revision":revision})
                        }
                        _ => {
                            self.ready = false;
                            configuration_error(&payload)
                        }
                    },
                    Outcome::Failed(response) => {
                        self.ready = false;
                        response
                    }
                };
                self.completion = Some((request, response));
            }
            return Ok(());
        }
        match purpose {
            Purpose::Load => {
                if !abandoned
                    && let Outcome::Payload(payload) = outcome
                    && let Some((settings, revision)) = parse_document(&payload, &self.defaults)
                {
                    self.settings = settings;
                    self.revision = revision;
                    self.ready = true;
                } else {
                    self.reload()?;
                }
            }
            Purpose::Update {
                request,
                expected_revision,
                desired,
            } => {
                let result = match outcome {
                    Outcome::Payload(payload) if !abandoned => {
                        match parse_document(&payload, &self.defaults) {
                            Some((settings, revision))
                                if expected_revision.checked_add(1) == Some(revision)
                                    && settings == desired =>
                            {
                                self.settings = settings.clone();
                                self.revision = revision;
                                self.ready = true;
                                json!({"ok":true,"settings":settings,"revision":revision})
                            }
                            _ => {
                                self.reload()?;
                                configuration_error(&payload)
                            }
                        }
                    }
                    Outcome::Payload(_) => {
                        self.reload()?;
                        failure(
                            "request_ended",
                            "settings request ended; saved values will be read again",
                        )
                    }
                    Outcome::Failed(error) => {
                        self.reload()?;
                        error
                    }
                };
                self.completion = Some((request, result));
            }
        }
        Ok(())
    }

    fn reload(&mut self) -> ModuleResult<()> {
        self.ready = false;
        self.pending = Some(PendingCall {
            purpose: Purpose::Load,
            payload: json!({"action":"product_get"}),
            operation_id: self.next_operation_id("reload")?,
            state: CallState::Submit { attempted: false },
            abandoned: false,
            first_sent: None,
        });
        Ok(())
    }

    fn next_operation_id(&mut self, label: &str) -> ModuleResult<String> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("settings operation identity exhausted")?;
        Ok(format!(
            "remote-session.settings.{label}.{}.{}",
            self.startup_id, self.sequence
        ))
    }
}

fn parse_document(payload: &Value, defaults: &ProductSettings) -> Option<(ProductSettings, u64)> {
    if payload.get("ok")?.as_bool() != Some(true) {
        return None;
    }
    let revision = payload.get("revision")?.as_u64()?;
    let found = payload.get("found")?.as_bool()?;
    if !found {
        if revision != 0
            || !payload.get("version")?.is_null()
            || !payload.get("values")?.as_object()?.is_empty()
        {
            return None;
        }
        defaults.validate().ok()?;
        return Some((defaults.clone(), 0));
    }
    if revision == 0 || payload.get("version")?.as_u64() != Some(u64::from(DOCUMENT_VERSION)) {
        return None;
    }
    let settings: ProductSettings = serde_json::from_value(payload.get("values")?.clone()).ok()?;
    settings.validate().ok()?;
    Some((settings, revision))
}

fn configuration_error(payload: &Value) -> Value {
    let nested = payload.get("error");
    let code = payload
        .get("code")
        .or_else(|| nested.and_then(|error| error.get("code")))
        .and_then(Value::as_str)
        .unwrap_or("configuration_failed");
    let message = payload
        .get("message")
        .or_else(|| nested.and_then(|error| error.get("message")))
        .and_then(Value::as_str)
        .unwrap_or("configuration returned an invalid settings document");
    let mut response = failure(code, message);
    if let Some(revision) = payload
        .get("current_revision")
        .or_else(|| nested.and_then(|error| error.get("current_revision")))
        .and_then(Value::as_u64)
    {
        response["current_revision"] = json!(revision);
    }
    response
}

fn failure(code: &str, message: &str) -> Value {
    json!({"ok":false,"code":code.chars().take(128).collect::<String>(),"message":message.chars().take(512).collect::<String>()})
}

#[cfg(test)]
#[path = "product_settings/tests.rs"]
mod tests;
