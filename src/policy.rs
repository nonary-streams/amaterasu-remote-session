//! Remote-session product policy behind the public session-presentation API.
//!
//! The durable document is the authority for native ownership. `synchronize`
//! applies only a confirmed document; request admission alone cannot create or
//! release a display. Transport bindings are persisted before their API ack.
use crate::{
    cache::{Resolution, TileCache},
    identity::{Control, identify, valid_profile},
    projection::{Role, presentation},
    settings::ProductSettings,
    transactions::{ProjectionStore, error},
};
use amaterasu_module_sdk::session_presentation::{
    Application, Artwork, AuxiliaryStreamPlan, CaptureSource, Context, ControlCompletion, Decision,
    DispatchIntent, MAX_RECOVERY_BUDGET_MS, MAX_RECOVERY_ITEMS, Mode, Observation, RecoveryBinding,
    RecoveryWorkItem, Reply, Request, Selection, SessionEvent, SharedCapture,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::{Duration, Instant},
};

mod allocation;
mod artwork;
mod confirmation;
mod controls;
mod dispatch;
mod lifecycle;
mod recovery;
mod source;

const CONFIRMATION_WINDOW: Duration = Duration::from_secs(60);
const MAX_OPERATIONS: usize = 128;
const MAX_RESERVATIONS: usize = 32;

#[derive(Clone, Debug)]
pub(crate) struct MonitorStatus {
    pub generation: u64,
    pub ready: bool,
    pub source_ready: bool,
    pub source_mode: Option<Mode>,
    pub source_route: String,
    pub output: Option<String>,
    pub cleanup_pending: bool,
    pub reference_released: bool,
    pub source_reference_released: bool,
    pub error: Option<String>,
    pub retryable: bool,
}

pub(crate) trait Monitors {
    fn begin(&mut self, owner: &RetainedOwner, now: Instant) -> Result<(), String>;
    fn snapshot(&self, client: &str) -> Option<MonitorStatus>;
    fn release(&mut self, client: &str, generation: u64, now: Instant) -> Result<(), String>;
    fn capture_started(
        &mut self,
        client: &str,
        generation: u64,
        reservation: &str,
    ) -> Result<(), String>;
    fn capture_stopped(&mut self, client: &str, generation: u64, reservation: &str, now: Instant);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReservationKind {
    Auxiliary,
    Borrowed,
    Application,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reservation {
    pub id: String,
    pub kind: ReservationKind,
    pub borrower_id: String,
    pub consumer: String,
    pub application_operation_id: Option<String>,
    pub application_epoch: Option<String>,
    pub application_generation: Option<u64>,
    pub transport_id: Option<String>,
    pub runtime_epoch: Option<String>,
    pub started: bool,
    pub retirement_requested: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetainedOwner {
    pub client_id: String,
    pub consumer: String,
    pub session_id: String,
    pub generation: u64,
    pub role: Role,
    pub role_active: bool,
    pub role_committed: bool,
    pub mode: Mode,
    pub profile_id: String,
    pub profile_settings: Value,
    pub release_requested: bool,
    pub reservations: Vec<Reservation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    next_generation: u64,
    owners: Vec<RetainedOwner>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            version: 1,
            next_generation: 1,
            owners: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RecoveryCandidate {
    pub client_id: String,
    pub borrower_id: String,
    pub consumer: String,
    pub session_id: String,
    pub generation: u64,
    pub reservation_id: String,
    pub transport_id: String,
    pub runtime_epoch: String,
    pub kind: ReservationKind,
    pub application_operation_id: Option<String>,
    pub application_epoch: Option<String>,
    pub application_generation: Option<u64>,
}

#[derive(Clone)]
struct Confirmation {
    generation: u64,
    game_id: String,
    target_id: String,
    expires: Instant,
}

#[derive(Default)]
struct ClientState {
    terminate: Option<Confirmation>,
    replacement: Option<Confirmation>,
}

struct Operation {
    fingerprint: [u8; 32],
    deadline: Instant,
    decision: Option<Decision>,
    canceled: bool,
    created_owner: Option<(String, u64)>,
    replacement_authorized: bool,
    terminal_decision: Option<Decision>,
    committed: bool,
    auxiliary_target: Option<(String, u64, Role)>,
}

pub(crate) struct SessionPolicy {
    settings: ProductSettings,
    desired: Snapshot,
    durable: Snapshot,
    revision: u64,
    durable_revision: u64,
    clients: BTreeMap<String, ClientState>,
    projections: ProjectionStore,
    cache: TileCache,
    operations: BTreeMap<(String, String, String), Operation>,
    applied_holds: BTreeSet<(String, u64, String)>,
    recovering: BTreeSet<(String, String)>,
    settled_work: VecDeque<(String, String)>,
}

impl SessionPolicy {
    pub(crate) fn new(settings: ProductSettings) -> Self {
        Self {
            settings,
            desired: Snapshot::default(),
            durable: Snapshot::default(),
            revision: 0,
            durable_revision: 0,
            clients: BTreeMap::new(),
            projections: ProjectionStore::default(),
            cache: TileCache::default(),
            operations: BTreeMap::new(),
            applied_holds: BTreeSet::new(),
            recovering: BTreeSet::new(),
            settled_work: VecDeque::new(),
        }
    }

    pub(crate) fn restore(&mut self, value: Value) -> Result<(), String> {
        let snapshot: Snapshot =
            serde_json::from_value(value).map_err(|_| "invalid_owner_journal")?;
        validate_snapshot(&snapshot)?;
        self.desired = snapshot.clone();
        self.durable = snapshot;
        self.revision = 0;
        self.durable_revision = 0;
        self.recovering.clear();
        let mut recovering_changed = false;
        for owner in &mut self.desired.owners {
            for reservation in &mut owner.reservations {
                self.recovering
                    .insert((owner.session_id.clone(), reservation.id.clone()));
                if reservation.transport_id.is_some() && !reservation.retirement_requested {
                    reservation.retirement_requested = true;
                    recovering_changed = true;
                }
            }
        }
        // An unbound admission cannot have reached RTSP: BindTransport is a
        // durable barrier before effects. Remove only these proven inert holds.
        let mut changed = recovering_changed;
        for owner in &mut self.desired.owners {
            owner.reservations.retain(|reservation| {
                if reservation.transport_id.is_none() {
                    changed = true;
                    self.recovering
                        .remove(&(owner.session_id.clone(), reservation.id.clone()));
                    false
                } else {
                    true
                }
            });
        }
        if changed {
            self.changed();
        }
        Ok(())
    }

    pub(crate) fn desired_document(&self) -> (u64, Value) {
        (
            self.revision,
            serde_json::to_value(&self.desired).expect("owner document serializes"),
        )
    }

    pub(crate) fn needs_persistence(&self) -> bool {
        self.revision != self.durable_revision
    }

    pub(crate) fn accept_durable(&mut self, revision: u64, value: Value) -> Result<(), String> {
        let snapshot: Snapshot =
            serde_json::from_value(value).map_err(|_| "invalid_owner_journal")?;
        validate_snapshot(&snapshot)?;
        if revision > self.revision || revision < self.durable_revision {
            return Err("owner_journal_revision_changed".into());
        }
        self.durable = snapshot;
        self.durable_revision = revision;
        Ok(())
    }

    pub(crate) fn apply_settings(&mut self, settings: ProductSettings) {
        self.settings = settings;
    }

    pub(crate) fn handle(
        &mut self,
        caller: &str,
        request: Request,
        now: Instant,
        monitors: &mut impl Monitors,
    ) -> Reply {
        self.projections.expire(now);
        let previous = self.desired.clone();
        let previous_revision = self.revision;
        let response = match request {
            Request::Describe {} => Reply::Described {
                interface_major: 1,
                interface_minor: 0,
            },
            Request::BeginProject {
                context,
                catalog_revision,
            } => self
                .projections
                .begin(caller, context, catalog_revision, now),
            Request::AppendProject {
                client_id,
                projection_id,
                entries,
                final_page,
            } => {
                let role = self.role(&client_id);
                self.projections.append(
                    caller,
                    &client_id,
                    &projection_id,
                    entries,
                    final_page,
                    role,
                    self.settings.show_remote_input,
                    self.settings.show_remote_monitor,
                    &mut self.cache,
                )
            }
            Request::ReadProject {
                client_id,
                projection_id,
                offset,
                limit,
            } => self
                .projections
                .read(caller, &client_id, &projection_id, offset, limit),
            Request::ReleaseProject {
                client_id,
                projection_id,
            } => self.projections.release(caller, &client_id, &projection_id),
            Request::Info { context } => {
                let pending = self.replacement_pending(&context, now);
                Reply::Info {
                    presentation: presentation(&context, self.role(&context.client.id), pending),
                }
            }
            Request::Dispatch {
                context,
                operation_id,
                intent,
                selection,
                mode,
            } => self.dispatch(
                caller,
                context,
                operation_id,
                intent,
                selection,
                mode,
                now,
                monitors,
            ),
            Request::CancelDispatch {
                client_id,
                operation_id,
            } => self.cancel_dispatch(caller, &client_id, &operation_id, now, monitors),
            Request::VerifySource { client_id, source } => {
                self.verify_source(caller, &client_id, source, monitors)
            }
            Request::BindTransport {
                client_id,
                session_id,
                generation,
                reservation_id,
                transport_id,
                transport_epoch,
            } => self.bind_transport(
                caller,
                &client_id,
                &session_id,
                generation,
                &reservation_id,
                &transport_id,
                &transport_epoch,
            ),
            Request::BindApplication {
                client_id,
                session_id,
                generation,
                reservation_id,
                application_operation_id,
                application_epoch,
                application_generation,
            } => self.bind_application(
                caller,
                &client_id,
                &session_id,
                generation,
                &reservation_id,
                &application_operation_id,
                &application_epoch,
                application_generation,
            ),
            Request::RevokeClient { client_id } => {
                self.revoke_client(Some(caller), &client_id, now, monitors)
            }
            Request::RecoveryWork { cursor, limit } => {
                self.recovery_work(caller, cursor.as_deref(), limit)
            }
            Request::ReconcileRecovery { work_id } => self.reconcile_recovery(caller, &work_id),
            Request::Observe {
                client_id,
                session_id,
                generation,
                reservation_id,
                transport_id,
                transport_epoch,
                event,
            } => self.observe(
                caller,
                Observation {
                    client_id,
                    session_id,
                    generation,
                    reservation_id,
                    transport_id,
                    transport_epoch,
                    event,
                },
                now,
                monitors,
            ),
            Request::Artwork { context, selection } => self.artwork(caller, &context, &selection),
        };
        if let Err(code) = crate::journal::validate_document(&self.desired_document().1) {
            for operation in self.operations.values_mut() {
                if operation
                    .created_owner
                    .as_ref()
                    .is_some_and(|(session, generation)| {
                        !previous.owners.iter().any(|owner| {
                            owner.session_id == *session && owner.generation == *generation
                        })
                    })
                {
                    operation.created_owner = None;
                    operation.auxiliary_target = None;
                }
            }
            self.desired = previous;
            self.revision = previous_revision;
            return error("owner_journal_capacity_exhausted", &code);
        }
        response
    }
}

fn pending() -> Reply {
    Reply::Pending { retry_after_ms: 50 }
}
fn reservation_id(caller: &str, client: &str, operation: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"remote-session-reservation-v1\0");
    digest.update(caller.as_bytes());
    digest.update([0]);
    digest.update(client.as_bytes());
    digest.update([0]);
    digest.update(operation.as_bytes());
    format!("reservation-{:x}", digest.finalize())
}
fn owner_session_id(caller: &str, client: &str, operation: &str, generation: u64) -> String {
    let mut digest = Sha256::new();
    digest.update(b"remote-session-creator-v1\0");
    digest.update(caller.as_bytes());
    digest.update([0]);
    digest.update(client.as_bytes());
    digest.update([0]);
    digest.update(operation.as_bytes());
    format!("remote-{:x}-{generation}", digest.finalize())
}

fn recovery_id(candidate: &RecoveryCandidate) -> String {
    format!(
        "retirement-{:x}",
        Sha256::digest(serde_json::to_vec(candidate).expect("recovery identity serializes"))
    )
}
fn candidate_from(owner: &RetainedOwner, reservation: &Reservation) -> Option<RecoveryCandidate> {
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
}
fn reject(status_code: u16, message: &str) -> Decision {
    Decision::Reject {
        status_code,
        message: message.into(),
    }
}
fn terminal_monitor_failure(status: &MonitorStatus) -> Option<Decision> {
    let reason = status
        .error
        .as_ref()
        .filter(|_| !status.retryable && !status.cleanup_pending)?;
    Some(reject(
        503,
        &format!(
            "Remote Monitor is unavailable: {}",
            reason.chars().take(192).collect::<String>()
        ),
    ))
}
fn completion(control: Control) -> ControlCompletion {
    ControlCompletion {
        status_code: 410,
        message: match control {
            Control::DisconnectMonitor => "Remote Monitor has been disconnected.",
            Control::DisconnectInput => "Remote Input has been disconnected.",
            Control::Terminate => {
                "The running game was terminated. Remote Monitor and Remote Input are still connected."
            }
            _ => "Remote session control completed.",
        }
        .into(),
    }
}

fn resolve_profile_mode(requested: Mode, properties: &Value) -> Result<Mode, &'static str> {
    let Some(profile) = properties.get("remote_monitor") else {
        return Ok(requested);
    };
    let mode = Mode {
        width: profile["width"]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .ok_or("invalid_monitor_profile_mode")?,
        height: profile["height"]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .ok_or("invalid_monitor_profile_mode")?,
        refresh_millihz: profile["refresh_millihz"]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .ok_or("invalid_monitor_profile_mode")?,
        hdr: profile["hdr_enabled"]
            .as_bool()
            .ok_or("invalid_monitor_profile_mode")?,
    };
    mode.validate()?;
    Ok(mode)
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<(), String> {
    if snapshot.version != 1 || snapshot.next_generation == 0 || snapshot.owners.len() > 32 {
        return Err("invalid_owner_journal".into());
    }
    let mut clients = BTreeSet::new();
    let mut sessions = BTreeSet::new();
    for owner in &snapshot.owners {
        if owner.consumer.is_empty()
            || owner.consumer.len() > 128
            || owner.client_id.is_empty()
            || owner.client_id.len() > 128
            || !clients.insert(owner.client_id.as_str())
            || owner.session_id.is_empty()
            || owner.session_id.len() > 128
            || !sessions.insert(owner.session_id.as_str())
            || owner.generation == 0
            || owner.generation >= snapshot.next_generation
            || owner.reservations.len() > MAX_RESERVATIONS
            || !owner.profile_settings.is_object()
            || !serde_json::to_vec(&owner.profile_settings).is_ok_and(|value| value.len() <= 4096)
            || owner.mode.validate().is_err()
        {
            return Err("invalid_owner_journal".into());
        }
        let mut reservations = BTreeSet::new();
        for reservation in &owner.reservations {
            if reservation.consumer.is_empty()
                || reservation.consumer.len() > 128
                || reservation.borrower_id.is_empty()
                || reservation.borrower_id.len() > 128
                || reservation.id.is_empty()
                || reservation.id.len() > 128
                || !reservations.insert(reservation.id.as_str())
                || reservation.transport_id.is_some() != reservation.runtime_epoch.is_some()
                || reservation
                    .transport_id
                    .as_ref()
                    .is_some_and(|id| id.is_empty() || id.len() > 128)
                || reservation
                    .runtime_epoch
                    .as_ref()
                    .is_some_and(|id| id.is_empty() || id.len() > 128)
                || reservation.application_operation_id.is_some()
                    != reservation.application_epoch.is_some()
                || reservation.application_operation_id.is_some()
                    != reservation.application_generation.is_some()
                || reservation.kind == ReservationKind::Application
                    && reservation.application_generation.is_none()
            {
                return Err("invalid_owner_journal".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
