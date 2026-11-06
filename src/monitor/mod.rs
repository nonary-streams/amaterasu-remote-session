//! Retained, authenticated-client display ownership, independent of a transport.
//!
//! This provider uses the public virtual-display service. It never adopts an
//! unrelated connector or treats request admission as readiness or retirement.
pub mod layout;

use amaterasu_module_sdk::virtual_display::{Operation, SERVICE, VirtualDisplayLayout};
use amaterasu_module_sdk::{Command, ModuleResult, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_millis(250);
const HEALTH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_CLIENT_HISTORY: usize = 128;
const MAX_CAPTURES: usize = 32;
const MAX_EFFECT_ATTEMPTS: u8 = 3;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorRequest {
    pub profile_id: String,
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
    pub hdr_enabled: bool,
    pub profile_settings: Value,
}

impl MonitorRequest {
    fn validate(&self) -> Result<(), String> {
        if !valid_identity(&self.profile_id)
            || !(320..=7680).contains(&self.width)
            || !(200..=4320).contains(&self.height)
            || !(1000..=1_000_000).contains(&self.refresh_millihz)
            || !self.profile_settings.is_object()
            || !serde_json::to_vec(&self.profile_settings).is_ok_and(|bytes| bytes.len() <= 16_384)
        {
            return Err("invalid_monitor_request".into());
        }
        if let Some(scale) = self.profile_settings.get("dd_virtual_display_scale") {
            let supported = matches!(
                scale.as_i64(),
                Some(
                    -1 | 0 | 100 | 125 | 150 | 175 | 200 | 225 | 250 | 300 | 350 | 400 | 450 | 500
                )
            );
            if !supported {
                return Err("invalid_monitor_scale".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct MonitorSettings {
    pub disconnect_on_stream_end: bool,
    pub disconnect_on_client_disconnect: bool,
    pub max_monitors: usize,
}

impl Default for MonitorSettings {
    fn default() -> Self {
        Self {
            disconnect_on_stream_end: false,
            disconnect_on_client_disconnect: false,
            max_monitors: 4,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MonitorSnapshot {
    pub client_id: String,
    pub generation: u64,
    pub ready: bool,
    pub retryable: bool,
    /// An exact provider-issued capture selector. No desktop fallback is valid.
    pub output: Option<String>,
    pub error: Option<String>,
    pub cleanup_pending: bool,
    pub reference_released: bool,
    /// A provider may safely retain a desktop anchor after reference release.
    pub parked: bool,
    /// True only after status proves the exact native target has disappeared.
    pub native_retired: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Inspect,
    Create,
    Remove,
    VerifyRemove,
}

struct Monitor {
    client_id: String,
    generation: u64,
    request: MonitorRequest,
    identity: String,
    reference: String,
    layout: Option<VirtualDisplayLayout>,
    step: Step,
    call: Option<Call>,
    serial: u64,
    next_poll: Instant,
    ready: bool,
    output: Option<String>,
    error: Option<String>,
    release_requested: bool,
    reference_released: bool,
    parked: bool,
    native_retired: bool,
    create_attempts: u8,
    remove_attempts: u8,
    effect_stalled: bool,
    captures: BTreeSet<String>,
}

impl Monitor {
    fn snapshot(&self) -> MonitorSnapshot {
        MonitorSnapshot {
            client_id: self.client_id.clone(),
            generation: self.generation,
            ready: self.ready && !self.release_requested,
            retryable: self.error.is_some() || (!self.ready && !self.reference_released),
            output: self.ready.then(|| self.output.clone()).flatten(),
            error: self.error.clone(),
            cleanup_pending: self.release_requested && !self.reference_released,
            reference_released: self.reference_released,
            parked: self.parked,
            native_retired: self.native_retired,
        }
    }

    fn operation(&self) -> Operation {
        match self.step {
            Step::Inspect | Step::VerifyRemove => Operation::Status {
                identity: Some(self.identity.clone()),
                reference_id: Some(self.reference.clone()),
            },
            Step::Create => Operation::Create {
                identity: self.identity.clone(),
                reference_id: Some(self.reference.clone()),
                name: "Remote Monitor".into(),
                width: self.request.width,
                height: self.request.height,
                refresh_millihz: self.request.refresh_millihz,
                hdr_enabled: self.request.hdr_enabled,
                dd_virtual_display_scale: self
                    .request
                    .profile_settings
                    .get("dd_virtual_display_scale")
                    .and_then(Value::as_i64)
                    .map(|scale| scale as i32),
                virtual_display_layout: self.layout,
                expected_topology_revision: None,
                expected_topology_domain_id: None,
            },
            Step::Remove => Operation::Remove {
                identity: self.identity.clone(),
                reference_id: Some(self.reference.clone()),
            },
        }
    }

    fn queue(&mut self, now: Instant) -> Result<(), String> {
        match self.step {
            Step::Create => self.create_attempts = self.create_attempts.saturating_add(1),
            Step::Remove => self.remove_attempts = self.remove_attempts.saturating_add(1),
            _ => {}
        }
        let serial = self
            .serial
            .checked_add(1)
            .ok_or("monitor_operation_id_exhausted")?;
        self.serial = serial;
        self.call = Some(Call::new(
            serde_json::to_value(self.operation()).map_err(|error| error.to_string())?,
            format!(
                "remote-monitor-{}-{}-{serial}",
                self.identity, self.generation
            ),
            now,
        ));
        Ok(())
    }

    fn schedule(&mut self, step: Step, now: Instant, delay: Duration) {
        self.step = step;
        self.next_poll = now + delay;
    }

    fn request_release(&mut self, now: Instant) {
        if self.effect_stalled {
            // A fresh authenticated release opens one more bounded native
            // cleanup window. Polling alone never resets a provider's budget.
            self.remove_attempts = 0;
            self.effect_stalled = false;
        }
        self.release_requested = true;
        self.ready = false;
        self.output = None;
        if self.call.is_none() && self.captures.is_empty() {
            self.schedule(Step::Remove, now, Duration::ZERO);
        }
    }

    fn accept(&mut self, result: Result<Value, String>, now: Instant) {
        let payload = match result {
            Ok(payload) if payload["ok"] == true => payload,
            Ok(payload) => {
                self.ready = false;
                self.output = None;
                self.error = Some(
                    payload["code"]
                        .as_str()
                        .unwrap_or("monitor_provider_rejected")
                        .into(),
                );
                // A rejected call is not proof of native resource absence.
                self.schedule(
                    if self.release_requested {
                        Step::VerifyRemove
                    } else {
                        Step::Inspect
                    },
                    now,
                    RETRY_INTERVAL,
                );
                return;
            }
            Err(error) => {
                self.ready = false;
                self.output = None;
                self.error = Some(error);
                self.schedule(
                    if self.release_requested {
                        Step::VerifyRemove
                    } else {
                        Step::Inspect
                    },
                    now,
                    RETRY_INTERVAL,
                );
                return;
            }
        };
        if matches!(self.step, Step::Create | Step::Remove) {
            // Native actors can still be creating/removing after admission.
            self.schedule(
                if self.release_requested {
                    Step::VerifyRemove
                } else {
                    Step::Inspect
                },
                now,
                Duration::ZERO,
            );
            return;
        }
        self.inspect(payload, now);
    }

    fn inspect(&mut self, payload: Value, now: Instant) {
        let Some(reference_owned) = payload["reference_owned"].as_bool() else {
            self.reject_status("invalid_monitor_status", now);
            return;
        };
        // Windows supplies identity publication before display activation;
        // Linux creates an explicitly extended output instead of its global default.
        self.layout = (payload["driver"] != "sunshine").then_some(VirtualDisplayLayout::Extended);
        let Some(monitors) = payload["monitors"]
            .as_array()
            .filter(|monitors| monitors.len() <= 8)
        else {
            self.reject_status("invalid_monitor_status", now);
            return;
        };
        let matching: Vec<_> = monitors
            .iter()
            .filter(|monitor| monitor["specification"]["identity"] == self.identity)
            .collect();
        if matching.len() > 1 {
            self.reject_status("ambiguous_monitor_identity", now);
            return;
        }
        let monitor = matching.first().copied();
        if self.release_requested {
            if reference_owned {
                self.schedule_remove(now);
                return;
            }
            if let Some(monitor) = monitor {
                // Reference release does not establish native retirement while
                // an actor is still removing the target or reports failure.
                let state = monitor["state"].as_str().unwrap_or("invalid_status");
                self.error = (state == "cleanup_failed").then(|| "monitor_cleanup_failed".into());
                if state == "cleanup_failed" {
                    self.schedule_remove(now);
                } else {
                    self.schedule(Step::VerifyRemove, now, RETRY_INTERVAL);
                }
                return;
            }
            if payload["parked"] == true {
                let verified = payload["parked_outputs"].as_array().is_some_and(|outputs| {
                    outputs
                        .iter()
                        .filter(|output| {
                            output["identity"] == self.identity && valid_output(&output["output"])
                        })
                        .count()
                        == 1
                });
                if !verified {
                    self.reject_status("invalid_parked_monitor_status", now);
                    return;
                }
                self.parked = true;
            } else {
                self.native_retired = true;
            }
            self.reference_released = true;
            self.error = None;
            return;
        }
        if !reference_owned {
            self.ready = false;
            self.output = None;
            if monitor.is_some() {
                self.reject_status("monitor_reference_not_owned", now);
            } else if self.create_attempts >= MAX_EFFECT_ATTEMPTS {
                self.error = Some("monitor_creation_requires_retry".into());
                self.effect_stalled = true;
            } else {
                self.schedule(Step::Create, now, Duration::ZERO);
            }
            return;
        }
        let Some(monitor) = monitor else {
            self.reject_status("monitor_reference_without_target", now);
            return;
        };
        let specification = &monitor["specification"];
        if specification["width"] != self.request.width
            || specification["height"] != self.request.height
            || specification["refresh_millihz"] != self.request.refresh_millihz
            || specification["hdr_enabled"] != self.request.hdr_enabled
            || specification["name"] != "Remote Monitor"
        {
            self.reject_status("monitor_specification_mismatch", now);
            return;
        }
        if monitor["state"] != "ready" {
            self.reject_status(
                monitor["error"].as_str().unwrap_or("monitor_not_ready"),
                now,
            );
            return;
        }
        if monitor["ready"]["active"] != true || !valid_output(&monitor["capture_output"]) {
            self.reject_status("exact_monitor_capture_unavailable", now);
            return;
        }
        self.ready = true;
        self.output = monitor["capture_output"].as_str().map(str::to_owned);
        self.error = None;
        self.schedule(Step::Inspect, now, HEALTH_INTERVAL);
    }

    fn reject_status(&mut self, error: &str, now: Instant) {
        self.ready = false;
        self.output = None;
        self.error = Some(error.into());
        self.schedule(
            if self.release_requested {
                Step::VerifyRemove
            } else {
                Step::Inspect
            },
            now,
            RETRY_INTERVAL,
        );
    }

    fn schedule_remove(&mut self, now: Instant) {
        if self.remove_attempts >= MAX_EFFECT_ATTEMPTS {
            self.error = Some("monitor_cleanup_requires_retry".into());
            self.effect_stalled = true;
        } else {
            self.schedule(Step::Remove, now, RETRY_INTERVAL);
        }
    }
}

/// Owns at most eight targets. All protocol identities passed here must come
/// from the authenticated pairing authority, never from request query strings.
pub struct MonitorManager {
    settings: MonitorSettings,
    monitors: BTreeMap<String, Monitor>,
    generations: BTreeMap<String, u64>,
    shutting_down: bool,
}

impl MonitorManager {
    pub fn new(settings: MonitorSettings) -> Self {
        Self {
            settings,
            monitors: BTreeMap::new(),
            generations: BTreeMap::new(),
            shutting_down: false,
        }
    }

    pub fn begin(
        &mut self,
        client: &str,
        generation: u64,
        request: MonitorRequest,
        now: Instant,
    ) -> Result<MonitorSnapshot, String> {
        if !valid_identity(client) || generation == 0 || self.shutting_down {
            return Err("invalid_monitor_owner".into());
        }
        request.validate()?;
        if self
            .monitors
            .get(client)
            .is_some_and(|existing| existing.reference_released && generation > existing.generation)
        {
            self.monitors.remove(client);
        }
        if let Some(existing) = self.monitors.get_mut(client) {
            if existing.generation != generation {
                return Err("monitor_generation_conflict".into());
            }
            if existing.request != request {
                return Err("retained_monitor_profile_conflict".into());
            }
            if existing.release_requested || existing.reference_released {
                return Err("monitor_cleanup_pending".into());
            }
            if existing.effect_stalled {
                existing.effect_stalled = false;
                existing.create_attempts = 0;
                existing.schedule(Step::Inspect, now, Duration::ZERO);
            }
            return Ok(existing.snapshot());
        }
        if self
            .generations
            .get(client)
            .is_some_and(|seen| generation <= *seen)
        {
            return Err("stale_monitor_generation".into());
        }
        if self.active_count() >= self.settings.max_monitors.clamp(1, 8)
            || (!self.generations.contains_key(client)
                && self.generations.len() >= MAX_CLIENT_HISTORY)
        {
            return Err("monitor_capacity_exhausted".into());
        }
        let identity = monitor_identity(client);
        let reference = format!("remote-monitor-{identity}-{generation}");
        let monitor = Monitor {
            client_id: client.into(),
            generation,
            request,
            identity,
            reference,
            layout: Some(VirtualDisplayLayout::Extended),
            step: Step::Inspect,
            call: None,
            serial: 0,
            next_poll: now,
            ready: false,
            output: None,
            error: None,
            release_requested: false,
            reference_released: false,
            parked: false,
            native_retired: false,
            create_attempts: 0,
            remove_attempts: 0,
            effect_stalled: false,
            captures: BTreeSet::new(),
        };
        let snapshot = monitor.snapshot();
        self.generations.insert(client.into(), generation);
        self.monitors.insert(client.into(), monitor);
        Ok(snapshot)
    }

    pub fn snapshot(&self, client: &str) -> Option<MonitorSnapshot> {
        self.monitors.get(client).map(Monitor::snapshot)
    }

    pub fn release(&mut self, client: &str, generation: u64, now: Instant) -> Result<(), String> {
        let Some(monitor) = self.monitors.get_mut(client) else {
            return if self
                .generations
                .get(client)
                .is_some_and(|seen| generation <= *seen)
            {
                Ok(())
            } else {
                Err("monitor_not_owned".into())
            };
        };
        if monitor.generation != generation {
            return Err("stale_monitor_generation".into());
        }
        monitor.request_release(now);
        Ok(())
    }

    pub fn capture_started(
        &mut self,
        client: &str,
        generation: u64,
        transport_id: &str,
    ) -> Result<(), String> {
        let monitor = self.monitors.get_mut(client).ok_or("monitor_not_owned")?;
        if monitor.generation != generation
            || !monitor.ready
            || monitor.release_requested
            || !valid_identity(transport_id)
        {
            return Err("monitor_capture_not_admitted".into());
        }
        if !monitor.captures.contains(transport_id) && monitor.captures.len() >= MAX_CAPTURES {
            return Err("monitor_capture_capacity_exhausted".into());
        }
        monitor.captures.insert(transport_id.into());
        Ok(())
    }

    pub fn capture_stopped(
        &mut self,
        client: &str,
        generation: u64,
        transport_id: &str,
        now: Instant,
    ) {
        if let Some(monitor) = self
            .monitors
            .get_mut(client)
            .filter(|monitor| monitor.generation == generation)
        {
            monitor.captures.remove(transport_id);
            if monitor.release_requested && monitor.captures.is_empty() && monitor.call.is_none() {
                monitor.schedule(Step::Remove, now, Duration::ZERO);
            }
        }
    }

    pub fn stream_end(&mut self, client: &str, generation: u64, now: Instant) {
        if self.settings.disconnect_on_stream_end {
            let _ = self.release(client, generation, now);
        }
    }

    pub fn client_disconnect(&mut self, client: &str, generation: u64, now: Instant) {
        if self.settings.disconnect_on_client_disconnect {
            let _ = self.release(client, generation, now);
        }
    }

    pub fn unpair(&mut self, client: &str, now: Instant) {
        if let Some(monitor) = self.monitors.get_mut(client) {
            monitor.request_release(now);
        }
    }

    pub fn shutdown(&mut self, now: Instant) {
        self.shutting_down = true;
        for monitor in self.monitors.values_mut() {
            monitor.request_release(now);
        }
    }

    pub fn has_pending_cleanup(&self) -> bool {
        self.monitors
            .values()
            .any(|monitor| monitor.release_requested && !monitor.reference_released)
    }

    pub fn active_count(&self) -> usize {
        self.monitors
            .values()
            .filter(|monitor| !monitor.reference_released)
            .count()
    }

    pub fn tick(
        &mut self,
        now: Instant,
        send: &mut impl FnMut(Command) -> ModuleResult<Response>,
    ) -> ModuleResult<()> {
        for monitor in self.monitors.values_mut() {
            if monitor.reference_released || monitor.effect_stalled || now < monitor.next_poll {
                continue;
            }
            if monitor.release_requested && !monitor.captures.is_empty() && monitor.call.is_none() {
                continue;
            }
            if monitor.call.is_none() {
                if monitor.release_requested && monitor.step != Step::VerifyRemove {
                    monitor.step = Step::Remove;
                }
                monitor.queue(now)?;
            }
            let outcome = monitor
                .call
                .as_mut()
                .expect("queued monitor call")
                .poll(now, send);
            if let Some(result) = outcome {
                monitor.call = None;
                monitor.accept(result, now);
            }
        }
        Ok(())
    }
}

struct Call {
    command: Command,
    request: Option<u64>,
    deadline: Instant,
    result: Option<Result<Value, String>>,
    cancelled: bool,
}

impl Call {
    fn new(payload: Value, operation_id: String, now: Instant) -> Self {
        Self {
            command: Command::Request {
                service: SERVICE.into(),
                payload,
                timeout_ms: REQUEST_TIMEOUT.as_millis() as u64,
                operation_id,
                parent: None,
            },
            request: None,
            deadline: now + REQUEST_TIMEOUT,
            result: None,
            cancelled: false,
        }
    }

    fn poll(
        &mut self,
        now: Instant,
        send: &mut impl FnMut(Command) -> ModuleResult<Response>,
    ) -> Option<Result<Value, String>> {
        let Some(request) = self.request else {
            // A lost admission may have committed. Retry the identical command,
            // including operation ID, until its broker request can be retired.
            match send(self.command.clone()) {
                Ok(Response::Accepted { id }) => self.request = Some(id),
                Ok(Response::Error { code, .. }) => return Some(Err(code)),
                _ => {}
            }
            return None;
        };
        if self.result.is_some() {
            return match send(Command::ReleaseResult { request }) {
                Ok(Response::Ok) => self.result.take(),
                Ok(Response::Error { code, .. }) if code == "not_found" => self.result.take(),
                _ => None,
            };
        }
        if now >= self.deadline || self.cancelled {
            self.cancelled = true;
            if settled(send(Command::Cancel { request })) {
                self.result = Some(Err("monitor_request_cancelled".into()));
            }
            return None;
        }
        match send(Command::TakeResult { request }) {
            Ok(Response::Pending) => None,
            Ok(Response::Result { payload }) => {
                self.result = Some(Ok(payload));
                None
            }
            _ => {
                self.cancelled = true;
                None
            }
        }
    }
}

fn settled(response: ModuleResult<Response>) -> bool {
    matches!(response, Ok(Response::Ok))
        || matches!(response, Ok(Response::Error { code, .. }) if code == "not_found")
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

fn valid_output(value: &Value) -> bool {
    value.as_str().is_some_and(|output| {
        !output.is_empty() && output.len() <= 4096 && !output.chars().any(char::is_control)
    })
}

fn monitor_identity(client: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"amaterasu.remote-session.monitor.identity.v1\0");
    hash.update((client.len() as u64).to_be_bytes());
    hash.update(client.as_bytes());
    let digest = hash.finalize();
    let mut bytes: [u8; 16] = digest[..16].try_into().expect("SHA-256 has sixteen bytes");
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[cfg(test)]
mod tests;
