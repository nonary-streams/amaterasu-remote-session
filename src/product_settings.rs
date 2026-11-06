//! Caller-scoped product preferences persisted through the selected Configuration
//! provider. One pending call and one management completion bound retained work.
use crate::settings::ProductSettings;
use amaterasu_module_sdk::{Command, ModuleResult, Response};
use serde_json::{Value, json};

const CONFIGURATION: &str = "amaterasu.configuration/1";
const DOCUMENT_VERSION: u32 = 1;
const TIMEOUT_MS: u64 = 30_000;

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
    Cancelling(u64),
    Releasing { request: u64, outcome: Outcome },
}

struct PendingCall {
    purpose: Purpose,
    payload: Value,
    operation_id: String,
    state: CallState,
    abandoned: bool,
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
        let state = std::mem::replace(&mut pending.state, CallState::Submit { attempted: false });
        let mut finished = None;
        match state {
            CallState::Submit { attempted } if pending.abandoned && !attempted => {
                finished = Some(Outcome::Failed(failure(
                    "request_ended",
                    "settings request ended before persistence",
                )));
            }
            CallState::Submit { .. } => {
                // An admission may have succeeded before transport failed. Keep
                // the exact operation identity even after its parent ends so
                // the broker can return that existing call for retirement.
                pending.state = CallState::Submit { attempted: true };
                match send(Command::Request {
                    service: CONFIGURATION.into(),
                    payload: pending.payload.clone(),
                    timeout_ms: TIMEOUT_MS,
                    operation_id: pending.operation_id.clone(),
                    parent: pending.purpose.parent(),
                }) {
                    Ok(Response::Accepted { id }) => {
                        pending.state = if pending.abandoned {
                            CallState::Cancelling(id)
                        } else {
                            CallState::Waiting(id)
                        }
                    }
                    Ok(Response::Error { code, message }) => {
                        finished = Some(Outcome::Failed(failure(&code, &message)))
                    }
                    Ok(_) | Err(_) => {}
                }
            }
            CallState::Waiting(request) if pending.abandoned => {
                pending.state = CallState::Cancelling(request);
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
                        finished = Some(Outcome::Failed(failure(&code, &message)))
                    }
                    Ok(Response::Error { code, message })
                        if matches!(code.as_str(), "cancelled" | "deadline_exceeded") =>
                    {
                        pending.state = CallState::Releasing {
                            request,
                            outcome: Outcome::Failed(failure(&code, &message)),
                        };
                    }
                    Ok(Response::Pending) | Ok(_) | Err(_) => {}
                }
            }
            CallState::Cancelling(request) => {
                pending.state = CallState::Cancelling(request);
                match send(Command::Cancel { request }) {
                    Ok(Response::Ok) => {
                        pending.state = CallState::Releasing {
                            request,
                            outcome: Outcome::Failed(failure(
                                "request_ended",
                                "settings persistence was cancelled; saved values will be read again",
                            )),
                        }
                    }
                    Ok(Response::Error { code, .. }) if code == "not_found" => {
                        finished = Some(Outcome::Failed(failure(
                            "request_ended",
                            "settings persistence no longer exists",
                        )))
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
            self.finish(pending.purpose, outcome, pending.abandoned)?;
        } else {
            self.pending = Some(pending);
        }
        Ok(())
    }

    /// Keep dependencies alive until this returns true. Unknown admissions are
    /// recovered with their original identity before cancellation and release.
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

    fn finish(&mut self, purpose: Purpose, outcome: Outcome, abandoned: bool) -> ModuleResult<()> {
        if self.shutting_down {
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
