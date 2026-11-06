//! Policy consumes logical source evidence. Native ownership stays in Monitor;
//! a selected capture provider must pass its pure preflight before new effects.
use crate::{
    monitor::{MonitorManager, MonitorRequest, MonitorSettings},
    policy::{MonitorStatus, Monitors, RetainedOwner},
    projection::Role,
    recovery::ReadOnlyCall,
    settings::ProductSettings,
};
use amaterasu_module_sdk::{
    Command, ModuleResult, Response,
    capture::{
        SourceClass, SourceMode, SourcePreflightRequest, SourceReadiness,
        decode_video_source_preflight, source_preflight_payload,
    },
    session_presentation::Mode,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);
type OwnerKey = (String, u64);

struct SourceProof {
    owner: RetainedOwner,
    route_key: &'static str,
    request: SourcePreflightRequest,
    ready: bool,
    terminal: bool,
    error: Option<String>,
    next_poll: Instant,
}

pub(crate) struct MonitorPort {
    pub(crate) manager: MonitorManager,
    authorizations: BTreeMap<String, bool>,
    authority_ready: bool,
    proofs: BTreeMap<OwnerKey, SourceProof>,
    pending: Option<(OwnerKey, SourcePreflightRequest, &'static str, ReadOnlyCall)>,
    startup_id: String,
    sequence: u64,
    shutting_down: bool,
}

impl MonitorPort {
    pub(crate) fn new(settings: &ProductSettings, startup_id: &str) -> Self {
        Self {
            manager: MonitorManager::new(monitor_settings(settings)),
            authorizations: BTreeMap::new(),
            authority_ready: false,
            proofs: BTreeMap::new(),
            pending: None,
            sequence: 0,
            shutting_down: false,
            startup_id: startup_id.into(),
        }
    }
    pub(crate) fn apply_settings(&mut self, settings: &ProductSettings) {
        self.manager.update_settings(monitor_settings(settings));
    }
    pub(crate) fn authorize_sources(
        &mut self,
        authorizations: BTreeMap<String, bool>,
        ready: bool,
    ) {
        let manager = &self.manager;
        self.proofs.retain(|(client, generation), _| {
            authorizations.contains_key(client)
                || manager.snapshot(client).is_some_and(|snapshot| {
                    snapshot.generation == *generation && !snapshot.source_reference_released
                })
        });
        self.authorizations = authorizations;
        self.authority_ready = ready;
    }

    pub(crate) fn tick(
        &mut self,
        now: Instant,
        send: &mut impl FnMut(Command) -> ModuleResult<Response>,
    ) -> ModuleResult<()> {
        if self.pending.is_none()
            && !self.shutting_down
            && let Some((key, proof)) = self
                .proofs
                .iter()
                .find(|(_, proof)| !proof.ready && !proof.terminal && now >= proof.next_poll)
        {
            self.sequence = self
                .sequence
                .checked_add(1)
                .ok_or("capture preflight identity exhausted")?;
            let command = Command::Request {
                service: "amaterasu.video/1".into(),
                payload: source_preflight_payload(proof.route_key, &proof.request)?,
                timeout_ms: PREFLIGHT_TIMEOUT.as_millis() as u64,
                operation_id: format!(
                    "remote-session.preflight.{}.{}",
                    self.startup_id, self.sequence
                ),
                parent: None,
            };
            self.pending = Some((
                key.clone(),
                proof.request.clone(),
                proof.route_key,
                ReadOnlyCall::new(command, now + PREFLIGHT_TIMEOUT),
            ));
        }
        if let Some((key, expected, route_key, mut call)) = self.pending.take() {
            match call.poll(now, self.shutting_down, send) {
                None => self.pending = Some((key, expected, route_key, call)),
                Some(result) => {
                    if let Some(proof) = self
                        .proofs
                        .get_mut(&key)
                        .filter(|proof| proof.request == expected && proof.route_key == route_key)
                    {
                        proof.next_poll = now + Duration::from_millis(250);
                        match result
                            .and_then(|payload| decode_video_source_preflight(&payload, &expected))
                        {
                            Ok(response) if response.is_ready() => {
                                proof.ready = true;
                                proof.terminal = false;
                                proof.error = None;
                            }
                            Ok(response) => {
                                proof.ready = false;
                                proof.terminal = response.state == SourceReadiness::Unsupported;
                                proof.error = response.reason;
                            }
                            Err(error) => {
                                proof.ready = false;
                                proof.terminal = false;
                                proof.error = Some(error);
                            }
                        }
                    }
                }
            }
        }
        self.manager.tick(now, send)
    }

    pub(crate) fn layout_status(&self) -> Value {
        let mut status = self.manager.layout_status();
        let preflights=self.proofs.iter().filter(|(_,proof)|!proof.ready).map(|((client,_),proof)|json!({"client_id":client,"state":if proof.error.is_some(){"failed"}else{"pending"},"error":proof.error})).collect::<Vec<_>>();
        if !preflights.is_empty() {
            status["state"] = json!(
                if preflights.iter().any(|proof| proof["state"] == "failed") {
                    "failed"
                } else {
                    "pending"
                }
            );
            status["capture_preflights"] = json!(preflights);
        }
        status
    }

    pub(crate) fn quiesce(
        &mut self,
        now: Instant,
        send: &mut impl FnMut(Command) -> ModuleResult<Response>,
    ) -> ModuleResult<bool> {
        self.shutting_down = true;
        self.tick(now, send)?;
        Ok(self.pending.is_none()
            && self.manager.active_count() == 0
            && !self.manager.has_pending_cleanup())
    }

    fn preflight(&mut self, owner: &RetainedOwner, now: Instant) -> Result<(), String> {
        if !self.authority_ready || self.authorizations.get(&owner.client_id) != Some(&true) {
            return Err("monitor_client_authority_pending".into());
        }
        let key = (owner.client_id.clone(), owner.generation);
        self.proofs.retain(|(client, generation), _| {
            client != &owner.client_id || *generation == owner.generation
        });
        let existing = owner
            .profile_settings
            .pointer("/monitor_target/kind")
            .and_then(Value::as_str)
            == Some("existing");
        let (route_key, source_class) = if existing {
            ("desktop_output", SourceClass::DesktopOutput)
        } else {
            ("managed_output", SourceClass::ManagedOutput)
        };
        let request = SourcePreflightRequest {
            source_class,
            route: None,
            mode: SourceMode {
                width: owner.mode.width,
                height: owner.mode.height,
                refresh_millihz: owner.mode.refresh_millihz,
                hdr: owner.mode.hdr,
                bit_depth: None,
            },
        };
        if self
            .proofs
            .get(&key)
            .is_none_or(|proof| proof.route_key != route_key || proof.request != request)
        {
            self.proofs.insert(
                key.clone(),
                SourceProof {
                    owner: owner.clone(),
                    route_key,
                    request,
                    ready: false,
                    terminal: false,
                    error: None,
                    next_poll: now,
                },
            );
        }
        if !self.proofs[&key].ready {
            return Err("capture_source_preflight_pending".into());
        }
        Ok(())
    }
}

fn monitor_settings(settings: &ProductSettings) -> MonitorSettings {
    MonitorSettings {
        disconnect_on_stream_end: settings.remote_monitor_disconnect_on_stream_end,
        disconnect_on_client_disconnect: settings.remote_monitor_disconnect_on_client_disconnect,
        max_monitors: settings.max_clients,
    }
}

impl Monitors for MonitorPort {
    fn begin(&mut self, owner: &RetainedOwner, now: Instant) -> Result<(), String> {
        if owner.role != Role::Monitor {
            return Err("input_session_cannot_acquire_monitor".into());
        }
        // Cleanup never depends on a working capture backend. The actor has no
        // native effect until policy synchronously queues its persisted release.
        if !owner.release_requested {
            self.preflight(owner, now)?;
        }
        self.manager
            .begin(
                &owner.client_id,
                owner.generation,
                monitor_request(owner),
                now,
            )
            .map(|_| ())
    }

    fn snapshot(&self, client: &str) -> Option<MonitorStatus> {
        let Some(snapshot) = self.manager.snapshot(client) else {
            let proof = self
                .proofs
                .values()
                .find(|proof| proof.owner.client_id == client && proof.terminal)?;
            return Some(MonitorStatus {
                generation: proof.owner.generation,
                ready: false,
                source_ready: false,
                source_route: proof.route_key.into(),
                source_mode: None,
                output: None,
                cleanup_pending: false,
                reference_released: false,
                source_reference_released: false,
                error: proof.error.clone(),
                retryable: false,
            });
        };
        let proof = self.proofs.get(&(client.to_owned(), snapshot.generation));
        let preflight_ready = self
            .proofs
            .get(&(client.to_owned(), snapshot.generation))
            .is_some_and(|proof| proof.ready);
        let authorized = self.authority_ready
            && self.authorizations.get(client) == Some(&true)
            && preflight_ready;
        let source_authorized = self.authority_ready
            && preflight_ready
            && (self.authorizations.get(client) == Some(&true) || snapshot.reference_released);
        Some(MonitorStatus {
            generation: snapshot.generation,
            ready: snapshot.ready && authorized,
            source_ready: snapshot.source_ready && source_authorized,
            source_route: snapshot.source_route,
            source_mode: snapshot.source_mode.map(|mode| Mode {
                width: mode.width,
                height: mode.height,
                refresh_millihz: mode.refresh_millihz,
                hdr: mode.hdr,
            }),
            output: snapshot.output,
            cleanup_pending: snapshot.cleanup_pending,
            reference_released: snapshot.reference_released,
            source_reference_released: snapshot.source_reference_released,
            error: proof
                .and_then(|proof| proof.error.clone())
                .or(snapshot.error),
            retryable: match proof {
                Some(proof) if !proof.ready => !proof.terminal,
                _ => snapshot.retryable,
            },
        })
    }
    fn release(&mut self, client: &str, generation: u64, now: Instant) -> Result<(), String> {
        if self.manager.snapshot(client).is_none()
            && let Some(proof) = self.proofs.get(&(client.to_owned(), generation))
        {
            self.manager
                .begin(client, generation, monitor_request(&proof.owner), now)?;
        }
        self.manager.release(client, generation, now)
    }
    fn capture_started(
        &mut self,
        client: &str,
        generation: u64,
        reservation: &str,
    ) -> Result<(), String> {
        self.manager
            .capture_started(client, generation, reservation)
    }
    fn capture_stopped(&mut self, client: &str, generation: u64, reservation: &str, now: Instant) {
        self.manager
            .capture_stopped(client, generation, reservation, now);
    }
}

fn monitor_request(owner: &RetainedOwner) -> MonitorRequest {
    MonitorRequest {
        profile_id: owner.profile_id.clone(),
        width: owner.mode.width,
        height: owner.mode.height,
        refresh_millihz: owner.mode.refresh_millihz,
        hdr_enabled: owner.mode.hdr,
        profile_settings: owner.profile_settings.clone(),
    }
}

#[cfg(test)]
#[path = "monitor_port/tests.rs"]
mod tests;
