//! Retained, authenticated-client display ownership, independent of a transport.
//!
//! This provider uses the public virtual-display service. It never adopts an
//! unrelated connector or treats request admission as readiness or retirement.
mod composition;
mod existing;
pub mod layout;
mod reconfigure;

use amaterasu_module_sdk::virtual_display::{Operation, SERVICE, VirtualDisplayLayout};
use amaterasu_module_sdk::{Command, ModuleResult, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::json;
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
pub struct MonitorMode {
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
    pub hdr: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MonitorSnapshot {
    pub client_id: String,
    pub generation: u64,
    pub ready: bool,
    /// Existing authenticated source reservations remain valid after the role
    /// disconnects, until their own capture lifetimes have retired.
    pub source_ready: bool,
    pub source_mode: Option<MonitorMode>,
    pub source_route: String,
    pub retryable: bool,
    /// An exact provider-issued capture selector. No desktop fallback is valid.
    pub output: Option<String>,
    pub error: Option<String>,
    pub cleanup_pending: bool,
    pub reference_released: bool,
    /// All of this owner's source obligations, including protected composition,
    /// have retired. A borrowed physical output itself remains present.
    pub source_reference_released: bool,
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
    InspectSource,
    CreateSource,
    RemoveSource,
    VerifySourceRemove,
    CompositionSnapshot,
    CompositionApply,
    CompositionRelease,
    PrepareMode,
    ModeSnapshot,
    ModeApply,
    CommitMode,
    ModeReconcile,
    ExistingPreflight,
    AbortMode,
    CompositionStatus,
    CompositionReconcile,
}

struct Monitor {
    instance_nonce: String,
    client_id: String,
    generation: u64,
    request: MonitorRequest,
    identity: String,
    reference: String,
    source_reference: String,
    source_reference_owned: bool,
    source_reference_released: bool,
    display_target: Option<String>,
    display_payload: Option<Value>,
    scope_reserved: bool,
    scope_released: bool,
    scope_cleanup_only: bool,
    observed_retirement: Option<(bool, bool)>,
    scope_observed: bool,
    pending_scope: Option<(String, bool)>,
    composition_dirty: bool,
    hdr_followup: bool,
    layout_rules: layout::Layout,
    known_targets: BTreeMap<String, String>,
    physical_anchors: Vec<Value>,
    existing: Option<existing::ExistingTarget>,
    desired_request: Option<MonitorRequest>,
    mode_operation: Option<String>,
    mode_revision: u64,
    external_borrowed: bool,
    peer_captures: bool,
    failed_mode_request: Option<MonitorRequest>,
    mode_failure: Option<String>,
    queued_mode_request: Option<MonitorRequest>,
    layout: Option<VirtualDisplayLayout>,
    step: Step,
    call: Option<Call>,
    serial: u64,
    next_poll: Instant,
    ready: bool,
    source_ready: bool,
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
            ready: self.ready
                && !self.release_requested
                && self.mode_failure.is_none()
                && self.queued_mode_request.is_none(),
            source_ready: self.source_ready,
            source_mode: self.source_ready.then(|| {
                self.existing
                    .as_ref()
                    .and_then(|target| target.binding.as_ref())
                    .map(|binding| MonitorMode {
                        width: binding.mode.width,
                        height: binding.mode.height,
                        refresh_millihz: binding.mode.refresh_millihz,
                        hdr: binding.mode.hdr_enabled,
                    })
                    .unwrap_or(MonitorMode {
                        width: self.request.width,
                        height: self.request.height,
                        refresh_millihz: self.request.refresh_millihz,
                        hdr: self.request.hdr_enabled,
                    })
            }),
            source_route: self
                .existing
                .as_ref()
                .map(|target| {
                    target
                        .binding
                        .as_ref()
                        .map_or("default", |binding| binding.route_key.as_str())
                })
                .unwrap_or("managed_output")
                .into(),
            retryable: !self.effect_stalled
                && self.mode_failure.is_none()
                && (self.error.is_some() || (!self.ready && !self.reference_released)),
            output: self.source_ready.then(|| self.output.clone()).flatten(),
            error: self.error.clone().or_else(|| self.mode_failure.clone()),
            cleanup_pending: self.release_requested
                && (!self.reference_released
                    || (self.captures.is_empty()
                        && !self.source_reference_released
                        && !self.external_borrowed)),
            reference_released: self.reference_released,
            source_reference_released: self.source_reference_released,
            parked: self.parked,
            native_retired: self.native_retired,
        }
    }

    fn operation(&self) -> Operation {
        let reference = if self.source_step() {
            &self.source_reference
        } else {
            &self.reference
        };
        match self.step {
            Step::Inspect | Step::VerifyRemove | Step::InspectSource | Step::VerifySourceRemove => {
                Operation::Status {
                    identity: Some(self.identity.clone()),
                    reference_id: Some(reference.clone()),
                }
            }
            Step::Create | Step::CreateSource => Operation::Create {
                identity: self.identity.clone(),
                reference_id: Some(reference.clone()),
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
            Step::Remove | Step::RemoveSource => Operation::Remove {
                identity: self.identity.clone(),
                reference_id: Some(reference.clone()),
            },
            Step::CompositionSnapshot | Step::CompositionApply | Step::CompositionRelease => {
                unreachable!("display composition uses its selected service")
            }
            Step::CompositionStatus | Step::CompositionReconcile => {
                unreachable!("scope recovery uses public Display SDK")
            }
            Step::PrepareMode
            | Step::ModeSnapshot
            | Step::ModeApply
            | Step::CommitMode
            | Step::ModeReconcile => {
                unreachable!("retained mode uses its selected contract")
            }
            Step::AbortMode => unreachable!("retained mode abort uses Virtual Display SDK"),
            Step::ExistingPreflight => unreachable!("existing source preflight uses Video SDK"),
        }
    }

    fn source_step(&self) -> bool {
        matches!(
            self.step,
            Step::InspectSource
                | Step::CreateSource
                | Step::RemoveSource
                | Step::VerifySourceRemove
        )
    }

    fn queue(&mut self, now: Instant) -> Result<(), String> {
        if self.step == Step::CompositionSnapshot && !self.scope_observed {
            // Durable admission discovery needs no desktop access. In
            // particular, cleanup of a refused launch must not depend on a
            // global native snapshot before learning whether it owned a scope.
            self.step = Step::CompositionStatus;
        }
        match self.step {
            Step::Create | Step::CreateSource => {
                self.create_attempts = self.create_attempts.saturating_add(1)
            }
            Step::Remove | Step::RemoveSource => {
                self.remove_attempts = self.remove_attempts.saturating_add(1)
            }
            _ => {}
        }
        let serial = self
            .serial
            .checked_add(1)
            .ok_or("monitor_operation_id_exhausted")?;
        self.serial = serial;
        let (service, payload) = if self.step == Step::ExistingPreflight {
            ("amaterasu.video/1", self.existing_preflight_payload()?)
        } else if self.existing.is_some() {
            (
                amaterasu_module_sdk::display::SERVICE,
                self.existing_operation()?,
            )
        } else {
            match self.step {
                Step::CompositionStatus => (
                    amaterasu_module_sdk::display::SERVICE,
                    amaterasu_module_sdk::display::ScopedOperation::ScopedStatus {
                        identity: self.identity.clone(),
                        reference_id: self.source_reference.clone(),
                    }
                    .payload()?,
                ),
                Step::CompositionReconcile => (
                    amaterasu_module_sdk::display::SERVICE,
                    amaterasu_module_sdk::display::ScopedOperation::ScopedReconcile {
                        operation_id: self
                            .pending_scope
                            .as_ref()
                            .ok_or("scoped_operation_missing")?
                            .0
                            .clone(),
                        identity: self.identity.clone(),
                        reference_id: self.source_reference.clone(),
                    }
                    .payload()?,
                ),
                Step::CompositionSnapshot | Step::ModeSnapshot => (
                    amaterasu_module_sdk::display::SERVICE,
                    json!({"action":"snapshot"}),
                ),
                Step::CompositionApply | Step::CompositionRelease | Step::ModeApply => (
                    amaterasu_module_sdk::display::SERVICE,
                    self.display_payload
                        .clone()
                        .ok_or("display_plan_unavailable")?,
                ),
                Step::PrepareMode | Step::CommitMode | Step::AbortMode => {
                    (SERVICE, self.mode_operation()?)
                }
                Step::ModeReconcile => (
                    amaterasu_module_sdk::display::SERVICE,
                    self.reconcile_operation()?,
                ),
                _ => (
                    SERVICE,
                    serde_json::to_value(self.operation()).map_err(|error| error.to_string())?,
                ),
            }
        };
        if self.step == Step::CompositionApply {
            // A new admission supersedes every earlier absence proof, even
            // when its broker acknowledgement or native completion is lost.
            self.scope_released = false;
            self.scope_observed = false;
        }
        self.call = Some(Call::new(
            service,
            payload,
            format!(
                "remote-monitor-{}-{}-{}-{serial}",
                self.instance_nonce, self.identity, self.generation
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
        if self.call.is_none() {
            self.schedule(
                if self.reference_released {
                    if self.existing.is_some() && self.captures.is_empty() {
                        Step::RemoveSource
                    } else {
                        Step::InspectSource
                    }
                } else {
                    Step::Remove
                },
                now,
                Duration::ZERO,
            );
        }
    }

    fn accept(&mut self, result: Result<Value, String>, now: Instant) {
        if matches!(
            self.step,
            Step::PrepareMode
                | Step::ModeSnapshot
                | Step::ModeApply
                | Step::CommitMode
                | Step::ModeReconcile
                | Step::AbortMode
        ) {
            self.accept_mode(result, now);
            return;
        }
        if self.existing.is_some() {
            self.accept_existing(result, now);
            return;
        }
        let payload = match result {
            Ok(payload) if payload["ok"] == true => payload,
            Ok(payload) => {
                let code = provider_error_code(&payload);
                if self.step == Step::CompositionReconcile
                    && (code == "scoped_operation_not_found"
                        || code == "scoped_readback_changed"
                            && self
                                .pending_scope
                                .as_ref()
                                .is_some_and(|(_, pending)| !*pending))
                {
                    self.scope_observed = true;
                    self.pending_scope = None;
                    self.error = None;
                    self.schedule(Step::CompositionSnapshot, now, Duration::ZERO);
                    return;
                }
                if self.step == Step::CompositionRelease && code == "target_in_use" {
                    // Another authenticated source owns a protected read-only
                    // reference. Retire this role while retaining the shared
                    // native source until the other reference is released.
                    self.external_borrowed = true;
                    self.error = None;
                    self.next_poll = now + HEALTH_INTERVAL;
                    return;
                }
                self.ready = false;
                self.source_ready = false;
                self.output = None;
                self.error = Some(code.into());
                if matches!(
                    self.step,
                    Step::CompositionStatus | Step::CompositionReconcile
                ) {
                    self.next_poll = now + RETRY_INTERVAL;
                    return;
                }
                if matches!(self.step, Step::CompositionApply | Step::CompositionRelease) {
                    self.retry_composition(code, now);
                    return;
                }
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
                self.source_ready = false;
                self.output = None;
                self.error = Some(error);
                if matches!(
                    self.step,
                    Step::CompositionStatus | Step::CompositionReconcile
                ) {
                    self.next_poll = now + RETRY_INTERVAL;
                    return;
                }
                if matches!(self.step, Step::CompositionApply | Step::CompositionRelease) {
                    // The native transition may have committed. Its provider
                    // operation identity and exact payload remain unchanged.
                    self.next_poll = now + RETRY_INTERVAL;
                    return;
                }
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
        if matches!(
            self.step,
            Step::Create | Step::Remove | Step::CreateSource | Step::RemoveSource
        ) {
            // Native actors can still be creating/removing after admission.
            self.schedule(
                match self.step {
                    Step::RemoveSource => Step::VerifySourceRemove,
                    Step::CreateSource if !self.release_requested => Step::InspectSource,
                    _ if self.release_requested => Step::VerifyRemove,
                    _ => Step::Inspect,
                },
                now,
                Duration::ZERO,
            );
            return;
        }
        if matches!(
            self.step,
            Step::CompositionSnapshot
                | Step::CompositionApply
                | Step::CompositionRelease
                | Step::CompositionStatus
                | Step::CompositionReconcile
        ) {
            self.accept_composition(payload, now);
        } else {
            self.inspect(payload, now);
        }
    }

    fn retry_composition(&mut self, error: &str, now: Instant) {
        match error {
            "topology_changed" | "revision_conflict" | "topology_revision_conflict" => {
                self.schedule(Step::CompositionSnapshot, now, RETRY_INTERVAL)
            }
            "mutation_disabled"
            | "unsupported_platform"
            | "unsupported_primary"
            | "target_ownership_unverified" => {
                self.error = Some(error.into());
                self.effect_stalled = true;
            }
            _ => self.next_poll = now + RETRY_INTERVAL,
        }
    }

    fn inspect(&mut self, payload: Value, now: Instant) {
        let source_step = self.source_step();
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
            if source_step && reference_owned {
                self.source_reference_owned = true;
                if let Some(target) = monitor
                    .and_then(|monitor| monitor["ready"]["device_path"].as_str())
                    .filter(|target| {
                        !target.is_empty()
                            && target.len() <= 4096
                            && !target.chars().any(char::is_control)
                    })
                {
                    self.display_target = Some(target.into());
                }
                if !self.reference_released {
                    self.schedule(Step::Remove, now, Duration::ZERO);
                } else if self.captures.is_empty() {
                    self.schedule(
                        if self.display_target.is_some() {
                            Step::CompositionSnapshot
                        } else {
                            Step::RemoveSource
                        },
                        now,
                        Duration::ZERO,
                    );
                } else if let Some(monitor) = monitor {
                    self.accept_source_readiness(monitor, now);
                } else {
                    self.reject_status("source_reference_without_target", now);
                }
                return;
            }
            if !source_step && reference_owned {
                self.schedule_remove(now);
                return;
            }
            if !source_step {
                self.reference_released = true;
                // A restarted plugin has not yet observed its independent
                // source reference. Always reconcile that exact reference.
                self.schedule(Step::InspectSource, now, Duration::ZERO);
                return;
            } else {
                self.source_reference_owned = false;
                self.source_ready = false;
                self.output = None;
            }
            if let Some(monitor) = monitor {
                // Reference release does not establish native retirement while
                // an actor is still removing the target or reports failure.
                let state = monitor["state"].as_str().unwrap_or("invalid_status");
                self.error = (state == "cleanup_failed").then(|| "monitor_cleanup_failed".into());
                if state == "cleanup_failed" {
                    self.schedule_remove(now);
                } else {
                    self.schedule(
                        if source_step {
                            Step::VerifySourceRemove
                        } else {
                            Step::VerifyRemove
                        },
                        now,
                        RETRY_INTERVAL,
                    );
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
            self.observed_retirement = Some((self.parked, self.native_retired));
            if !self.captures.is_empty() {
                self.error = Some("monitor_capture_retirement_pending".into());
                self.schedule(Step::VerifySourceRemove, now, HEALTH_INTERVAL);
                return;
            }
            if self.scope_released {
                self.finish_retirement();
            } else {
                self.schedule(Step::CompositionSnapshot, now, Duration::ZERO);
            }
            return;
        }
        if !reference_owned {
            self.ready = false;
            self.output = None;
            if source_step && monitor.is_some() && self.create_attempts < MAX_EFFECT_ATTEMPTS {
                self.source_reference_owned = false;
                self.schedule(Step::CreateSource, now, Duration::ZERO);
            } else if monitor.is_some() {
                self.reject_status("monitor_reference_not_owned", now);
            } else if !self.captures.is_empty() {
                self.reject_status("monitor_source_lost_waiting_for_capture_retirement", now);
            } else if self.create_attempts >= MAX_EFFECT_ATTEMPTS {
                self.error = Some("monitor_creation_requires_retry".into());
                self.effect_stalled = true;
            } else {
                if !self.scope_released {
                    self.scope_cleanup_only = true;
                    self.schedule(Step::CompositionSnapshot, now, Duration::ZERO);
                } else {
                    self.schedule(Step::Create, now, Duration::ZERO);
                }
            }
            return;
        }
        let Some(monitor) = monitor else {
            self.reject_status("monitor_reference_without_target", now);
            return;
        };
        self.mode_revision = monitor["mode_revision"].as_u64().unwrap_or(0);
        if self.restore_mode_intent(monitor, now) {
            return;
        }
        if source_step {
            self.source_reference_owned = true;
            self.source_reference_released = false;
        }
        if monitor["state"] == "ready" {
            if let Some(target) = monitor["ready"]["device_path"].as_str().filter(|target| {
                !target.is_empty() && target.len() <= 4096 && !target.chars().any(char::is_control)
            }) {
                self.display_target = Some(target.into());
            }
            if !self.source_reference_owned {
                self.schedule(Step::InspectSource, now, Duration::ZERO);
                return;
            }
            if (!self.scope_reserved || self.composition_dirty)
                && self.captures.is_empty()
                && self.display_target.is_some()
            {
                self.schedule(Step::CompositionSnapshot, now, Duration::ZERO);
                return;
            }
        }
        if !self.accept_source_readiness(monitor, now) {
            return;
        }
        if source_step {
            self.ready = true;
            self.schedule(Step::Inspect, now, HEALTH_INTERVAL);
        } else if !self.source_reference_owned {
            self.schedule(Step::InspectSource, now, Duration::ZERO);
        } else {
            self.ready = true;
            self.schedule(Step::Inspect, now, HEALTH_INTERVAL);
        }
    }

    fn accept_composition(&mut self, payload: Value, now: Instant) {
        if self.step == Step::CompositionStatus {
            match amaterasu_module_sdk::display::scoped_status_from_reply(
                payload,
                &self.identity,
                &self.source_reference,
            ) {
                Ok(status) => {
                    self.scope_observed = true;
                    self.pending_scope = status
                        .operation_id
                        .map(|operation| (operation, status.pending));
                    if !status.reference_owned {
                        self.scope_reserved = false;
                        self.scope_released = true;
                        if self.captures.is_empty()
                            && (self.scope_cleanup_only
                                || self.release_requested && self.reference_released)
                        {
                            self.complete_scope_release(now);
                            return;
                        }
                    }
                    self.schedule(
                        if self.pending_scope.is_some() {
                            Step::CompositionReconcile
                        } else {
                            Step::CompositionSnapshot
                        },
                        now,
                        Duration::ZERO,
                    );
                }
                Err(error) => self.reject_status(&error, now),
            }
            return;
        }
        if self.step == Step::CompositionReconcile {
            if self
                .pending_scope
                .as_ref()
                .is_none_or(|(operation, _)| payload["operation_id"] != *operation)
                || !matches!(payload["state"].as_str(), Some("succeeded" | "not_applied"))
            {
                self.error = Some("scoped_recovery_unconfirmed".into());
                self.next_poll = now + RETRY_INTERVAL;
                return;
            }
            self.pending_scope = None;
            self.error = None;
            self.schedule(Step::CompositionSnapshot, now, Duration::ZERO);
            return;
        }
        if self.step == Step::CompositionSnapshot {
            if !self.scope_observed {
                self.schedule(Step::CompositionStatus, now, Duration::ZERO);
                return;
            }
            let release = self.scope_cleanup_only
                || self.release_requested && self.reference_released && self.captures.is_empty();
            let target_id = if release {
                self.display_target.as_deref().unwrap_or("")
            } else if let Some(target) = self.display_target.as_deref() {
                target
            } else {
                self.reject_status("monitor_target_unavailable", now);
                return;
            };
            let operation_id = format!(
                "remote-layout-{}-{}-{}-{}",
                self.instance_nonce, self.identity, self.generation, self.serial
            );
            let result = composition::prepare(
                composition::TargetRequest {
                    client: &self.client_id,
                    identity: &self.identity,
                    reference: &self.source_reference,
                    target_id,
                    width: self.request.width,
                    height: self.request.height,
                    refresh_millihz: self.request.refresh_millihz,
                    hdr: self.request.hdr_enabled,
                    layout: &self.layout_rules,
                    known_targets: &self.known_targets,
                },
                payload,
                &operation_id,
                release,
            );
            match result {
                Ok(composition) => {
                    if self.peer_captures && composition::translates_origin(&composition.payload) {
                        self.composition_dirty = true;
                        self.error = None;
                        self.schedule(Step::InspectSource, now, HEALTH_INTERVAL);
                        return;
                    }
                    if !release {
                        self.physical_anchors = composition.physical_anchors;
                    }
                    self.hdr_followup = composition.needs_hdr_followup;
                    self.display_payload = Some(composition.payload);
                    self.schedule(
                        if release {
                            Step::CompositionRelease
                        } else {
                            Step::CompositionApply
                        },
                        now,
                        Duration::ZERO,
                    );
                }
                Err(error) => self.reject_status(&error, now),
            }
            return;
        }
        if !(self.step == Step::CompositionRelease
            && payload["released"] == true
            && payload["already_complete"] == true)
            && self.display_payload.as_ref().is_none_or(|plan| {
                payload["operation_id"] != plan["operation_id"]
                    || payload["target_id"] != plan["target"]["id"]
                        && self.step == Step::CompositionApply
            })
        {
            self.reject_status("invalid_display_composition_result", now);
            return;
        }
        match self.step {
            Step::CompositionApply
                if payload["applied"] == true || payload["reconciled"] == true =>
            {
                self.scope_reserved = true;
                self.scope_released = false;
                self.scope_observed = true;
                self.composition_dirty = self.hdr_followup;
                self.schedule(Step::InspectSource, now, Duration::ZERO);
            }
            Step::CompositionRelease if payload["released"] == true => {
                self.complete_scope_release(now);
            }
            _ => self.reject_status("display_composition_unconfirmed", now),
        }
    }

    fn complete_scope_release(&mut self, now: Instant) {
        self.scope_reserved = false;
        self.scope_released = true;
        self.external_borrowed = false;
        if self.observed_retirement.is_some() {
            self.finish_retirement();
        } else if self.scope_cleanup_only {
            self.scope_cleanup_only = false;
            self.schedule(
                if self.release_requested {
                    Step::Remove
                } else {
                    Step::Create
                },
                now,
                Duration::ZERO,
            );
        } else {
            self.schedule(Step::RemoveSource, now, Duration::ZERO);
        }
    }

    fn finish_retirement(&mut self) {
        if let Some((parked, native_retired)) = self.observed_retirement.take() {
            self.parked = parked;
            self.native_retired = native_retired;
        }
        self.source_reference_released = true;
        self.source_ready = false;
        self.output = None;
        self.error = None;
    }

    fn accept_source_readiness(&mut self, monitor: &Value, now: Instant) -> bool {
        let specification = &monitor["specification"];
        if specification["width"] != self.request.width
            || specification["height"] != self.request.height
            || specification["refresh_millihz"] != self.request.refresh_millihz
            || specification["hdr_enabled"] != self.request.hdr_enabled
            || specification["name"] != "Remote Monitor"
        {
            self.reject_status("monitor_specification_mismatch", now);
            return false;
        }
        if monitor["state"] != "ready" {
            self.reject_status(
                monitor["error"].as_str().unwrap_or("monitor_not_ready"),
                now,
            );
            return false;
        }
        if monitor["ready"]["active"] != true || !valid_output(&monitor["capture_output"]) {
            self.reject_status("exact_monitor_capture_unavailable", now);
            return false;
        }
        self.source_ready = self.source_reference_owned;
        self.output = monitor["capture_output"].as_str().map(str::to_owned);
        self.error = None;
        if self.release_requested {
            self.schedule(Step::InspectSource, now, HEALTH_INTERVAL);
        }
        true
    }

    fn reject_status(&mut self, error: &str, now: Instant) {
        self.ready = false;
        self.source_ready = false;
        self.output = None;
        self.error = Some(error.into());
        self.schedule(
            if self.source_step() {
                if self.release_requested {
                    Step::VerifySourceRemove
                } else {
                    Step::InspectSource
                }
            } else if self.release_requested {
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
    layout: layout::Layout,
    instance_nonce: String,
    startup_error: Option<String>,
}

impl MonitorManager {
    pub fn new(settings: MonitorSettings) -> Self {
        let mut nonce = [0u8; 16];
        let startup_error = getrandom::fill(&mut nonce)
            .err()
            .map(|_| "monitor_entropy_unavailable".into());
        let instance_nonce = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        Self {
            settings,
            monitors: BTreeMap::new(),
            generations: BTreeMap::new(),
            shutting_down: false,
            layout: layout::Layout::default(),
            instance_nonce,
            startup_error,
        }
    }

    pub fn begin(
        &mut self,
        client: &str,
        generation: u64,
        request: MonitorRequest,
        now: Instant,
    ) -> Result<MonitorSnapshot, String> {
        if let Some(error) = &self.startup_error {
            return Err(error.clone());
        }
        if !valid_identity(client) || generation == 0 || self.shutting_down {
            return Err("invalid_monitor_owner".into());
        }
        request.validate()?;
        let existing = existing::selection(&request.profile_settings)?;
        if self.monitors.get(client).is_some_and(|existing| {
            existing.reference_released
                && existing.source_reference_released
                && generation > existing.generation
        }) {
            self.monitors.remove(client);
        }
        if let Some(existing) = self.monitors.get_mut(client) {
            if existing.generation != generation {
                return Err("monitor_generation_conflict".into());
            }
            if let Some(failed) = &existing.failed_mode_request {
                if failed == &request {
                    return Err(existing
                        .mode_failure
                        .clone()
                        .unwrap_or_else(|| "retained_mode_rejected".into()));
                }
                existing.failed_mode_request = None;
                existing.mode_failure = None;
            }
            if existing.request == request {
                existing.queued_mode_request =
                    existing.desired_request.as_ref().map(|_| request.clone());
            }
            if existing.request != request {
                if existing.request.profile_id != request.profile_id
                    || existing.request.profile_settings != request.profile_settings
                {
                    return Err("retained_monitor_profile_conflict".into());
                }
                if existing.existing.is_some() {
                    // Encoded client dimensions do not authorize modifying an
                    // explicitly borrowed physical source or its frozen mode.
                    existing.request = request;
                    return Ok(existing.snapshot());
                }
                if let Some(desired) = &existing.desired_request {
                    return if desired == &request {
                        Ok(existing.snapshot())
                    } else {
                        Err("monitor_mode_busy".into())
                    };
                }
                if !existing.ready
                    || !existing.source_reference_owned
                    || !existing.scope_reserved
                    || existing.display_target.is_none()
                {
                    if existing.release_requested || !existing.captures.is_empty() {
                        return Err("monitor_mode_busy".into());
                    }
                    // Finish reconciling the admitted initial target first.
                    // The queued request suppresses publication of its old mode.
                    existing.queued_mode_request = Some(request);
                    return Ok(existing.snapshot());
                }
                if !existing.captures.is_empty()
                    || existing.call.is_some()
                    || existing.release_requested
                {
                    return Err("monitor_mode_busy".into());
                }
                if existing.desired_request.is_none() {
                    existing.ready = false;
                    existing.source_ready = false;
                    existing.output = None;
                    existing.mode_operation = Some(existing.new_mode_operation(&request));
                    existing.desired_request = Some(request);
                    existing.schedule(Step::PrepareMode, now, Duration::ZERO);
                }
                return Ok(existing.snapshot());
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
        let source_reference = format!("remote-source-{identity}-{generation}");
        let monitor = Monitor {
            instance_nonce: self.instance_nonce.clone(),
            client_id: client.into(),
            generation,
            request,
            identity,
            reference,
            source_reference,
            source_reference_owned: false,
            source_reference_released: false,
            display_target: None,
            display_payload: None,
            scope_reserved: false,
            scope_released: false,
            scope_cleanup_only: false,
            observed_retirement: None,
            scope_observed: false,
            pending_scope: None,
            composition_dirty: true,
            hdr_followup: false,
            layout_rules: self.layout.clone(),
            known_targets: BTreeMap::new(),
            physical_anchors: Vec::new(),
            existing,
            desired_request: None,
            mode_operation: None,
            mode_revision: 0,
            external_borrowed: false,
            peer_captures: false,
            failed_mode_request: None,
            mode_failure: None,
            queued_mode_request: None,
            layout: Some(VirtualDisplayLayout::Extended),
            step: Step::Inspect,
            call: None,
            serial: 0,
            next_poll: now,
            ready: false,
            source_ready: false,
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
            || monitor.mode_failure.is_some()
            || !(monitor.ready && !monitor.release_requested
                || monitor.source_ready
                    && monitor.release_requested
                    && !monitor.captures.is_empty())
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
                monitor.schedule(
                    if monitor.reference_released {
                        if monitor.existing.is_some() {
                            Step::RemoveSource
                        } else if monitor.scope_reserved {
                            Step::CompositionSnapshot
                        } else {
                            Step::RemoveSource
                        }
                    } else {
                        Step::Remove
                    },
                    now,
                    Duration::ZERO,
                );
            }
        }
    }

    pub fn set_layout(&mut self, layout: layout::Layout) -> Result<(), String> {
        let bytes = serde_json::to_vec(&layout).map_err(|error| error.to_string())?;
        self.layout = layout::Layout::decode(&bytes)?;
        for monitor in self.monitors.values_mut() {
            monitor.layout_rules = self.layout.clone();
            monitor.composition_dirty = true;
            monitor.effect_stalled = false;
            if !monitor.release_requested && monitor.captures.is_empty() && monitor.call.is_none() {
                monitor.ready = false;
                monitor.next_poll = Instant::now();
            }
        }
        Ok(())
    }

    pub fn layout_status(&self) -> Value {
        let monitors=self.monitors.values().filter(|monitor|!monitor.source_reference_released).map(|monitor|json!({
            "client_id":monitor.client_id,
            "state":if monitor.error.is_some(){"failed"}else if monitor.composition_dirty||!monitor.scope_reserved{"pending"}else{"applied"},
            "error":monitor.error,
        })).collect::<Vec<_>>();
        let state = if monitors.iter().any(|monitor| monitor["state"] == "failed") {
            "failed"
        } else if monitors.iter().any(|monitor| monitor["state"] == "pending") {
            "pending"
        } else {
            "applied"
        };
        json!({"state":state,"monitors":monitors})
    }

    pub fn physical_anchors(&self) -> Vec<Value> {
        let mut anchors = BTreeMap::new();
        for anchor in self
            .monitors
            .values()
            .flat_map(|monitor| &monitor.physical_anchors)
        {
            if let Some(id) = anchor["id"].as_str() {
                anchors.insert(id.to_owned(), anchor.clone());
            }
        }
        anchors.into_values().collect()
    }

    pub fn apply_settings(&mut self, settings: MonitorSettings) {
        self.settings = settings;
    }

    pub fn update_settings(&mut self, settings: MonitorSettings) {
        self.apply_settings(settings);
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
            .any(|monitor| monitor.release_requested && !monitor.source_reference_released)
    }

    pub fn active_count(&self) -> usize {
        self.monitors
            .values()
            .filter(|monitor| !monitor.source_reference_released)
            .count()
    }

    pub fn tick(
        &mut self,
        now: Instant,
        send: &mut impl FnMut(Command) -> ModuleResult<Response>,
    ) -> ModuleResult<()> {
        let known_targets = self
            .monitors
            .values()
            .filter_map(|monitor| {
                monitor
                    .display_target
                    .as_ref()
                    .map(|target| (target.clone(), monitor.client_id.clone()))
            })
            .collect::<BTreeMap<_, _>>();
        let active_clients = self
            .monitors
            .values()
            .filter(|monitor| !monitor.captures.is_empty())
            .map(|monitor| monitor.client_id.clone())
            .collect::<BTreeSet<_>>();
        for monitor in self.monitors.values_mut() {
            monitor.known_targets = known_targets.clone();
            monitor.peer_captures = active_clients
                .iter()
                .any(|client| client != &monitor.client_id);
            if monitor.source_reference_released
                || monitor.effect_stalled
                || now < monitor.next_poll
            {
                continue;
            }
            if monitor.call.is_none()
                && let Err(error) = monitor.queue(now)
            {
                monitor.error = Some(error);
                monitor.ready = false;
                monitor.source_ready = false;
                monitor.effect_stalled = true;
                continue;
            }
            let outcome = monitor
                .call
                .as_mut()
                .expect("queued monitor call")
                .poll(now, send);
            if let Some(result) = outcome {
                monitor.call = None;
                monitor.accept(result, now);
                monitor.activate_queued_mode(now);
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
    fn new(service: &str, payload: Value, operation_id: String, now: Instant) -> Self {
        Self {
            command: Command::Request {
                service: service.into(),
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

fn provider_error_code(payload: &Value) -> &str {
    payload
        .get("code")
        .or_else(|| payload.get("error").and_then(|error| error.get("code")))
        .and_then(Value::as_str)
        .filter(|code| {
            !code.is_empty()
                && code.len() <= 128
                && code.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .unwrap_or("invalid_provider_error")
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
