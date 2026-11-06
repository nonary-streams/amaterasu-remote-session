//! Revisioned profile management through the selected Catalog provider.
//!
//! Profiles remain ordinary marked catalog identities. No second store can
//! diverge from the entries presented to Moonlight, and accepted monitor owners
//! retain their previously frozen properties after a profile edit or deletion.
use amaterasu_module_sdk::application_library::{CatalogProfileRequest, LibraryCursor};
use amaterasu_module_sdk::{Command, ModuleResult, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeSet, VecDeque};

const CATALOG: &str = "amaterasu.catalog/1";
const DISPLAY: &str = "amaterasu.display/1";
const MAX_PENDING: usize = 32;
const MAX_PROFILES: usize = 16;
const MAX_PAGES: usize = 1_024;
const PAGE_QUERY: &str = "remote_monitor_profiles";

#[path = "profiles/model.rs"]
mod model;
pub use model::{Profile, ProfileTarget};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    ProfilesGet {},
    ProfileUpsert {
        profile: Profile,
        expected_revision: u64,
    },
    ProfileDelete {
        id: String,
        expected_revision: u64,
    },
    ProfileTargets {},
}

enum Purpose {
    List {
        revision: Option<u64>,
        after: Option<String>,
        profiles: Vec<Profile>,
        pages: usize,
        write: Option<(Profile, u64)>,
    },
    Upsert {
        desired: Profile,
        expected_revision: u64,
    },
    Delete {
        id: String,
        expected_revision: u64,
    },
    Targets,
}

enum Call {
    Submit { attempted: bool },
    Waiting(u64),
    Cancelling(u64),
    Releasing { request: u64, payload: Value },
}

struct Pending {
    parent: u64,
    purpose: Purpose,
    service: &'static str,
    payload: Value,
    operation_id: String,
    call: Call,
    abandoned: bool,
}

pub struct ProfileManager {
    startup: String,
    sequence: u64,
    active: Option<Pending>,
    queue: VecDeque<Pending>,
    completions: VecDeque<(u64, Value)>,
    shutting_down: bool,
}

impl ProfileManager {
    pub fn new(startup: &str) -> Self {
        Self {
            startup: startup
                .chars()
                .filter(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
                })
                .take(64)
                .collect(),
            sequence: 0,
            active: None,
            queue: VecDeque::new(),
            completions: VecDeque::new(),
            shutting_down: false,
        }
    }

    pub fn handle(&mut self, request_id: u64, caller: &str, payload: Value) -> Result<(), Value> {
        if caller != "amaterasu.management" {
            return Err(failure(
                "forbidden",
                "profile management requires authenticated management",
            ));
        }
        if self.shutting_down
            || self.queue.len() + self.completions.len() + usize::from(self.active.is_some())
                >= MAX_PENDING
        {
            return Err(failure(
                "profiles_busy",
                "profile operations are still being settled",
            ));
        }
        let operation: Operation = serde_json::from_value(payload)
            .map_err(|_| failure("invalid_request", "request does not match the profile API"))?;
        let (purpose, service, payload) = match operation {
            Operation::ProfilesGet {} => (
                Purpose::List {
                    revision: None,
                    after: None,
                    profiles: Vec::new(),
                    pages: 0,
                    write: None,
                },
                CATALOG,
                list_payload(None),
            ),
            Operation::ProfileUpsert {
                mut profile,
                expected_revision,
            } => {
                profile.validate()?;
                if profile.id.is_empty() {
                    if !profile.uuid.is_empty() {
                        return Err(failure(
                            "invalid_profile",
                            "new profiles must not select an existing UUID",
                        ));
                    }
                    profile.uuid = new_uuid()?;
                } else if profile.uuid.is_empty() {
                    return Err(failure(
                        "invalid_profile",
                        "existing profile UUID is required",
                    ));
                }
                (
                    Purpose::List {
                        revision: None,
                        after: None,
                        profiles: Vec::new(),
                        pages: 0,
                        write: Some((profile, expected_revision)),
                    },
                    CATALOG,
                    list_payload(None),
                )
            }
            Operation::ProfileDelete {
                id,
                expected_revision,
            } => {
                if !valid_id(&id) {
                    return Err(failure("invalid_profile", "profile ID is invalid"));
                }
                let payload = serde_json::to_value(CatalogProfileRequest::ProfileDelete {
                    id: id.clone(),
                    expected_revision,
                })
                .map_err(|_| invalid_catalog())?;
                (
                    Purpose::Delete {
                        id,
                        expected_revision,
                    },
                    CATALOG,
                    payload,
                )
            }
            Operation::ProfileTargets {} => (
                Purpose::Targets,
                DISPLAY,
                json!({"action":"capture_targets"}),
            ),
        };
        let operation_id = self.next_operation()?;
        self.queue.push_back(Pending {
            parent: request_id,
            purpose,
            service,
            payload,
            operation_id,
            call: Call::Submit { attempted: false },
            abandoned: false,
        });
        Ok(())
    }

    pub fn take_completion(&mut self) -> Option<(u64, Value)> {
        self.completions.pop_front()
    }

    pub fn tick(
        &mut self,
        send: &mut impl FnMut(Command) -> ModuleResult<Response>,
    ) -> ModuleResult<()> {
        if self.active.is_none() {
            self.active = self.queue.pop_front();
        }
        let Some(mut pending) = self.active.take() else {
            return Ok(());
        };
        if self.shutting_down {
            pending.abandoned = true;
        }
        if !pending.abandoned {
            match send(Command::CheckRequest {
                request: pending.parent,
            }) {
                Ok(Response::Ok) => {}
                Ok(Response::Error { code, .. })
                    if matches!(
                        code.as_str(),
                        "request_ended" | "not_found" | "cancelled" | "deadline_exceeded"
                    ) =>
                {
                    pending.abandoned = true
                }
                Ok(_) | Err(_) => {
                    self.active = Some(pending);
                    return Ok(());
                }
            }
        }
        let state = std::mem::replace(&mut pending.call, Call::Submit { attempted: false });
        let mut completed = None;
        match state {
            Call::Submit { attempted: false } if pending.abandoned => {
                completed = Some(failure(
                    "request_ended",
                    "profile request ended before admission",
                ))
            }
            Call::Submit { .. } => {
                pending.call = Call::Submit { attempted: true };
                match send(Command::Request {
                    service: pending.service.into(),
                    payload: pending.payload.clone(),
                    timeout_ms: 30_000,
                    operation_id: pending.operation_id.clone(),
                    parent: Some(pending.parent),
                }) {
                    Ok(Response::Accepted { id }) => {
                        pending.call = if pending.abandoned {
                            Call::Cancelling(id)
                        } else {
                            Call::Waiting(id)
                        }
                    }
                    Ok(Response::Error { code, message }) => {
                        completed = Some(failure(&code, &message))
                    }
                    Ok(_) | Err(_) => {}
                }
            }
            Call::Waiting(request) if pending.abandoned => pending.call = Call::Cancelling(request),
            Call::Waiting(request) => {
                pending.call = Call::Waiting(request);
                match send(Command::TakeResult { request }) {
                    Ok(Response::Result { payload }) => {
                        pending.call = Call::Releasing { request, payload }
                    }
                    Ok(Response::Error { code, message }) if code == "not_found" => {
                        completed = Some(failure(&code, &message))
                    }
                    Ok(Response::Error { code, message })
                        if matches!(code.as_str(), "cancelled" | "deadline_exceeded") =>
                    {
                        pending.call = Call::Releasing {
                            request,
                            payload: failure(&code, &message),
                        }
                    }
                    Ok(_) | Err(_) => {}
                }
            }
            Call::Cancelling(request) => {
                pending.call = Call::Cancelling(request);
                match send(Command::Cancel { request }) {
                    Ok(Response::Ok) => {
                        pending.call = Call::Releasing {
                            request,
                            payload: failure(
                                "request_ended",
                                "profile request cancelled; reload profiles before retrying a mutation",
                            ),
                        }
                    }
                    Ok(Response::Error { code, .. }) if code == "not_found" => {
                        completed =
                            Some(failure("request_ended", "profile request no longer exists"))
                    }
                    Ok(_) | Err(_) => {}
                }
            }
            Call::Releasing { request, payload } => {
                match send(Command::ReleaseResult { request }) {
                    Ok(Response::Ok) => completed = Some(payload),
                    Ok(Response::Error { code, .. }) if code == "not_found" => {
                        completed = Some(payload)
                    }
                    Ok(_) | Err(_) => pending.call = Call::Releasing { request, payload },
                }
            }
        }
        if let Some(payload) = completed {
            if !pending.abandoned && !self.shutting_down {
                match self.finish(&mut pending, payload) {
                    Ok(Some(response)) => self.completions.push_back((pending.parent, response)),
                    Ok(None) => self.active = Some(pending),
                    Err(response) => self.completions.push_back((pending.parent, response)),
                }
            }
        } else {
            self.active = Some(pending);
        }
        Ok(())
    }

    pub fn quiesce(
        &mut self,
        send: &mut impl FnMut(Command) -> ModuleResult<Response>,
    ) -> ModuleResult<bool> {
        self.shutting_down = true;
        self.queue.clear();
        self.completions.clear();
        self.tick(send)?;
        Ok(self.active.is_none())
    }

    fn next_operation(&mut self) -> Result<String, Value> {
        self.sequence = self.sequence.checked_add(1).ok_or_else(|| {
            failure(
                "operation_exhausted",
                "profile operation identifiers exhausted",
            )
        })?;
        Ok(format!(
            "remote-session.profile.{}.{}",
            self.startup, self.sequence
        ))
    }

    fn finish(&mut self, pending: &mut Pending, payload: Value) -> Result<Option<Value>, Value> {
        if payload["ok"] != true {
            return Err(upstream_failure(&payload));
        }
        match &mut pending.purpose {
            Purpose::Targets => Ok(Some(project_targets(payload)?)),
            Purpose::Delete {
                id,
                expected_revision,
            } => {
                if payload["id"] != *id
                    || payload["deleted"] != true
                    || payload["revision"].as_u64() != expected_revision.checked_add(1)
                {
                    return Err(invalid_catalog());
                }
                Ok(Some(json!({"ok":true,"revision":payload["revision"]})))
            }
            Purpose::Upsert {
                desired,
                expected_revision,
            } => {
                let profile = Profile::from_identity(payload["profile"].clone())?;
                let mut expected = desired.clone();
                if expected.id.is_empty() {
                    expected.id = profile.id.clone();
                }
                if profile != expected
                    || payload["revision"].as_u64() != expected_revision.checked_add(1)
                {
                    return Err(invalid_catalog());
                }
                Ok(Some(
                    json!({"ok":true,"revision":payload["revision"],"profile":profile}),
                ))
            }
            Purpose::List {
                revision,
                after,
                profiles,
                pages,
                write,
            } => {
                let page_revision = payload["revision"].as_u64().ok_or_else(invalid_catalog)?;
                if revision.is_some_and(|revision| revision != page_revision) {
                    return Err(failure(
                        "revision_conflict",
                        "catalog changed while listing profiles",
                    ));
                }
                *revision = Some(page_revision);
                *pages += 1;
                if *pages > MAX_PAGES {
                    return Err(failure(
                        "profile_scan_exhausted",
                        "catalog profile scan exceeds its bound",
                    ));
                }
                let entries = payload["entries"]
                    .as_array()
                    .filter(|entries| entries.len() <= 32)
                    .ok_or_else(invalid_catalog)?;
                let mut last = after.clone();
                for row in entries {
                    let profile = Profile::from_identity(row.clone())?;
                    if last.as_ref().is_some_and(|last| profile.id <= *last) {
                        return Err(invalid_catalog());
                    }
                    last = Some(profile.id.clone());
                    profiles.push(profile);
                    if profiles.len() > MAX_PROFILES {
                        return Err(failure(
                            "profile_capacity_exhausted",
                            "at most 16 managed monitor profiles are supported",
                        ));
                    }
                }
                let cursor = payload.get("next_cursor").ok_or_else(invalid_catalog)?;
                if !cursor.is_null() {
                    let cursor: LibraryCursor =
                        serde_json::from_value(cursor.clone()).map_err(|_| invalid_catalog())?;
                    if cursor.revision != page_revision
                        || cursor.query != PAGE_QUERY
                        || !valid_id(&cursor.after)
                        || after.as_ref().is_some_and(|after| cursor.after <= *after)
                        || last.as_ref().is_some_and(|last| cursor.after < *last)
                    {
                        return Err(invalid_catalog());
                    }
                    *after = Some(cursor.after.clone());
                    pending.payload = list_payload(Some(cursor));
                } else if let Some((profile, expected_revision)) = write.take() {
                    if expected_revision != page_revision {
                        return Err(
                            json!({"ok":false,"code":"revision_conflict","current_revision":page_revision}),
                        );
                    }
                    if profile.id.is_empty() {
                        if profiles.len() == MAX_PROFILES {
                            return Err(failure(
                                "profile_capacity_exhausted",
                                "at most 16 managed monitor profiles are supported",
                            ));
                        }
                    } else if profiles
                        .iter()
                        .find(|existing| existing.id == profile.id)
                        .is_none_or(|existing| existing.uuid != profile.uuid)
                    {
                        return Err(failure(
                            "profile_identity_mismatch",
                            "profile changed or no longer exists",
                        ));
                    }
                    pending.payload = serde_json::to_value(CatalogProfileRequest::ProfileUpsert {
                        id: profile.id.clone(),
                        entry: profile.entry(),
                        expected_revision,
                    })
                    .map_err(|_| invalid_catalog())?;
                    pending.purpose = Purpose::Upsert {
                        desired: profile,
                        expected_revision,
                    };
                } else {
                    if profiles
                        .iter()
                        .map(|profile| profile.uuid.as_str())
                        .collect::<BTreeSet<_>>()
                        .len()
                        != profiles.len()
                    {
                        return Err(invalid_catalog());
                    }
                    let response = json!({"ok":true,"revision":page_revision,"profiles":profiles});
                    if !serde_json::to_vec(&response).is_ok_and(|bytes| bytes.len() <= 12_000) {
                        return Err(failure(
                            "profile_reply_too_large",
                            "profile reply exceeds its transport bound",
                        ));
                    }
                    return Ok(Some(response));
                }
                pending.operation_id = self.next_operation()?;
                pending.call = Call::Submit { attempted: false };
                Ok(None)
            }
        }
    }
}

fn list_payload(cursor: Option<LibraryCursor>) -> Value {
    serde_json::to_value(CatalogProfileRequest::ProfileList {
        cursor,
        limit: Some(32),
    })
    .expect("profile list serializes")
}
fn printable(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}
fn valid_id(id: &str) -> bool {
    id.len() == 20
        && id.starts_with("app-")
        && id[4..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && id[4..].bytes().any(|byte| byte != b'0')
}
fn valid_uuid(uuid: &str) -> bool {
    let bytes = uuid.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
        && bytes
            .iter()
            .any(|byte| matches!(byte,b'1'..=b'9'|b'a'..=b'f'|b'A'..=b'F'))
}
fn new_uuid() -> Result<String, Value> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| failure("identity_unavailable", "cannot allocate a profile UUID"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}
fn failure(code: &str, message: &str) -> Value {
    json!({"ok":false,"code":code,"message":message})
}
fn invalid_catalog() -> Value {
    failure(
        "invalid_catalog_response",
        "catalog returned invalid profile metadata",
    )
}
fn upstream_failure(payload: &Value) -> Value {
    let code = payload["error"]["code"]
        .as_str()
        .or_else(|| payload["code"].as_str())
        .filter(|code| printable(code, 128))
        .unwrap_or("provider_unavailable");
    let message = payload["error"]["message"]
        .as_str()
        .or_else(|| payload["message"].as_str())
        .filter(|message| printable(message, 512))
        .unwrap_or("profile provider request failed");
    let mut result = failure(code, message);
    if let Some(revision) = payload["error"]["current_revision"]
        .as_u64()
        .or_else(|| payload["current_revision"].as_u64())
    {
        result["current_revision"] = revision.into();
    }
    result
}

fn project_targets(payload: Value) -> Result<Value, Value> {
    let targets = payload["targets"]
        .as_array()
        .filter(|targets| targets.len() <= 32)
        .ok_or_else(invalid_catalog)?;
    let mut result = Vec::new();
    let mut selectors = BTreeSet::new();
    for target in targets {
        let selector = target["selector"]
            .as_str()
            .filter(|selector| printable(selector, 256))
            .ok_or_else(invalid_catalog)?;
        let label = target["label"]
            .as_str()
            .filter(|label| printable(label, 160))
            .ok_or_else(invalid_catalog)?;
        let ready = target["ready"].as_bool().ok_or_else(invalid_catalog)?;
        if !selectors.insert(selector) {
            return Err(invalid_catalog());
        }
        let mut row = json!({"selector":selector,"label":label,"ready":ready});
        if let Some(error) = target.get("error").filter(|error| !error.is_null()) {
            let error = error
                .as_str()
                .filter(|error| printable(error, 256))
                .ok_or_else(invalid_catalog)?;
            row["error"] = error.into();
        }
        if let Some(mode) = target.get("mode").filter(|mode| !mode.is_null()) {
            let width = mode["width"]
                .as_u64()
                .filter(|value| (320..=7680).contains(value))
                .ok_or_else(invalid_catalog)?;
            let height = mode["height"]
                .as_u64()
                .filter(|value| (200..=4320).contains(value))
                .ok_or_else(invalid_catalog)?;
            let refresh = mode["refresh_millihz"]
                .as_u64()
                .filter(|value| (1000..=1_000_000).contains(value))
                .ok_or_else(invalid_catalog)?;
            let hdr = mode["hdr_enabled"].as_bool().ok_or_else(invalid_catalog)?;
            row["mode"] =
                json!({"width":width,"height":height,"refresh_millihz":refresh,"hdr_enabled":hdr});
        }
        result.push(row);
    }
    let response = json!({"ok":true,"targets":result});
    if !serde_json::to_vec(&response).is_ok_and(|bytes| bytes.len() <= 12_000) {
        return Err(failure(
            "target_reply_too_large",
            "display target reply exceeds its transport bound",
        ));
    }
    Ok(response)
}

#[cfg(test)]
#[path = "profiles/tests.rs"]
mod tests;
