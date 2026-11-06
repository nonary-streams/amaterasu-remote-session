//! Independently selected remote-session policy, durable ownership and monitor
//! orchestration. The host shell supplies only the ordinary managed-module ABI.
mod artwork;
mod cache;
mod identity;
mod journal;
pub mod monitor;
mod monitor_port;
mod policy;
mod product_settings;
pub mod profiles;
mod projection;
mod recovery;
mod settings;
mod transactions;

use amaterasu_module_sdk::session_presentation::{self as presentation, Reply, Request};
use amaterasu_module_sdk::{
    ApiMethod, Command, ManagedModule, ModuleResult, Response, RuntimeClient, operation_api,
};
use journal::OwnerJournal;
use monitor::layout::Layout;
use monitor_port::MonitorPort;
use policy::{Monitors, SessionPolicy};
use product_settings::ProductSettingsStore;
use profiles::ProfileManager;
use recovery::{Membership, Recovery};
use serde::Deserialize;
use serde_json::{Value, json};
use settings::{ProductSettings, Settings};
use std::{
    collections::BTreeSet,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const PUBLIC_SERVICE: &str = "nonary.remote-session/1";
const MANAGEMENT: &str = "amaterasu.management";
const REQUESTS_PER_TICK: usize = 16;
const MAX_PUBLIC_BYTES: usize = 16_000;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum PublicOperation {
    SettingsGet {},
    SettingsUpdate {
        settings: ProductSettings,
        expected_revision: u64,
    },
    LayoutGet {},
    LayoutUpdate {
        layout: Value,
        expected_revision: u64,
    },
}

#[derive(Clone, Copy)]
enum PreferencesReply {
    Settings,
    Layout,
}

struct RemoteSessionModule {
    runtime: RuntimeClient,
    startup: Settings,
    preferences: ProductSettingsStore,
    profiles: ProfileManager,
    journal: OwnerJournal,
    policy: SessionPolicy,
    monitors: MonitorPort,
    recovery: Recovery,
    membership: Membership,
    restored: bool,
    applied_settings_revision: Option<u64>,
    pending_preferences: Option<(u64, PreferencesReply)>,
    pending_durable: Option<(u64, Value)>,
    persistence_error: Option<String>,
    shutting_down: bool,
}

impl ManagedModule for RemoteSessionModule {
    fn start(runtime: RuntimeClient) -> ModuleResult<Self> {
        let Response::Result { payload } = runtime.send(Command::GetSettings {})? else {
            return Err("remote-session startup settings unavailable".into());
        };
        let startup: Settings = serde_json::from_value(payload)
            .map_err(|error| format!("invalid remote-session startup settings: {error}"))?;
        startup.validate()?;
        for method in presentation::methods().into_iter().chain(public_methods()) {
            // API registration selects the exact approved interface. No raw
            // service registration creates an unrestricted operation endpoint.
            runtime.require_success(Command::RegisterApi { method })?;
        }
        let startup_id = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let now = Instant::now();
        Ok(Self {
            runtime,
            preferences: ProductSettingsStore::new(&startup_id, startup.product.clone()),
            profiles: ProfileManager::new(&startup_id),
            journal: OwnerJournal::new(&startup_id),
            policy: SessionPolicy::new(startup.product.clone()),
            monitors: MonitorPort::new(&startup.product, &startup_id),
            recovery: Recovery::new(&startup_id, now),
            startup,
            restored: false,
            applied_settings_revision: None,
            membership: Membership::new(&startup_id, now),
            pending_preferences: None,
            pending_durable: None,
            persistence_error: None,
            shutting_down: false,
        })
    }

    fn tick(&mut self) -> ModuleResult<()> {
        let now = Instant::now();
        self.advance(now)?;
        for _ in 0..REQUESTS_PER_TICK {
            match self.runtime.send(Command::Poll {})? {
                Response::Pending => break,
                Response::Request {
                    id,
                    caller,
                    service,
                    payload,
                    ..
                } => self.handle_request(id, &caller, &service, payload, now)?,
                _ => return Err("remote-session broker polling failed".into()),
            }
        }
        self.persist_desired()?;
        Ok(())
    }

    fn quiesce(&mut self) -> ModuleResult<bool> {
        self.shutting_down = true;
        let now = Instant::now();
        self.advance(now)?;
        if !self.restored || self.persistence_error.is_some() {
            return Ok(false);
        }
        self.policy.shutdown();
        self.persist_desired()?;
        if !self.policy.is_retired() || self.pending_durable.is_some() {
            return Ok(false);
        }
        let runtime = &self.runtime;
        let profiles = self
            .profiles
            .quiesce(&mut |command| runtime.send(command))?;
        let recovery = self
            .recovery
            .quiesce(now, &mut |command| runtime.send(command))?;
        let membership = self
            .membership
            .quiesce(now, &mut |command| runtime.send(command))?;
        let monitors = self
            .monitors
            .quiesce(now, &mut |command| runtime.send(command))?;
        let preferences = self
            .preferences
            .quiesce(&mut |command| runtime.send(command))?;
        let journal = self.journal.quiesce(&mut |command| runtime.send(command))?;
        Ok(profiles && recovery && membership && monitors && preferences && journal)
    }

    fn health(&self) -> ModuleResult<()> {
        if let Some(error) = &self.persistence_error {
            return Err(error.clone());
        }
        if let Some(error) = self.journal.last_error() {
            return Err(format!("owner journal: {error}"));
        }
        if self.restored
            && !self.policy.recovery_candidates().is_empty()
            && let Some(error) = self.recovery.last_error()
        {
            return Err(format!("source retirement: {error}"));
        }
        if self.restored
            && !self.membership.ready_for(&self.policy.authority_clients())
            && let Some(error) = self.membership.last_error()
        {
            return Err(format!("retained client authority: {error}"));
        }
        Ok(())
    }
}

impl RemoteSessionModule {
    fn advance(&mut self, now: Instant) -> ModuleResult<()> {
        let runtime = &self.runtime;
        self.preferences
            .advance(&mut |command| runtime.send(command))?;
        self.journal.advance(&mut |command| runtime.send(command))?;
        self.profiles.tick(&mut |command| runtime.send(command))?;
        self.apply_preferences()?;
        if !self.restored && self.preferences.is_ready() && self.journal.is_ready() {
            self.policy.restore(self.journal.snapshot().clone())?;
            self.restored = true;
            if !self.startup.enabled {
                self.policy.shutdown();
            }
        }
        if let Some(result) = self.journal.take_completion() {
            match (self.pending_durable.take(), result) {
                (Some((revision, document)), Ok(())) if self.journal.snapshot() == &document => {
                    self.policy.accept_durable(revision, document)?
                }
                (_, Err(error)) => {
                    self.persistence_error =
                        Some(format!("owner journal persistence unconfirmed: {error}"))
                }
                _ => {
                    self.persistence_error =
                        Some("owner journal completion did not match its requested document".into())
                }
            }
        }
        if self.restored {
            let clients = self.policy.authority_clients();
            let runtime = &self.runtime;
            self.membership
                .advance(now, &clients, &mut |command| runtime.send(command))?;
            self.monitors.authorize_sources(
                self.membership.source_authorizations(),
                self.membership.ready_for(&clients),
            );
            if let Some(authority) = self.membership.take_observation()
                && (!authority.paired || !authority.may_view)
            {
                self.policy.revoke_restored_client(
                    &authority.certificate_id,
                    now,
                    &mut self.monitors,
                );
            }
            let candidates = self.policy.recovery_candidates();
            let runtime = &self.runtime;
            self.recovery
                .advance(now, &candidates, &mut |command| runtime.send(command))?;
            if let Some(candidate) = self.recovery.take_retired() {
                self.policy.accept_retired(&candidate);
            }
            if self.shutting_down {
                self.policy.shutdown();
            }
            if self.persistence_error.is_none()
                && !self.policy.needs_persistence()
                && self.pending_durable.is_none()
                && self.membership.ready_for(&self.policy.authority_clients())
            {
                self.policy.synchronize(now, &mut self.monitors);
            }
        }
        let runtime = &self.runtime;
        self.monitors
            .tick(now, &mut |command| runtime.send(command))?;
        while let Some((request, response)) = self.profiles.take_completion() {
            self.complete(request, bounded_public(response))?;
        }
        if let Some((request, mut response)) = self.preferences.take_completion() {
            if self
                .pending_preferences
                .take()
                .is_some_and(|(expected, kind)| {
                    expected == request && matches!(kind, PreferencesReply::Layout)
                })
                && response["ok"] == true
            {
                response = self.layout_response();
            }
            self.complete(request, bounded_public(response))?;
        }
        self.persist_desired()
    }

    fn apply_preferences(&mut self) -> ModuleResult<()> {
        if !self.preferences.is_ready() {
            return Ok(());
        }
        let (preferences, revision) = self.preferences.settings();
        if self.applied_settings_revision == Some(revision) {
            return Ok(());
        }
        let layout = Layout::decode(
            &serde_json::to_vec(&preferences.remote_monitor_layout)
                .map_err(|error| error.to_string())?,
        )?;
        self.monitors.manager.set_layout(layout)?;
        self.monitors.apply_settings(preferences);
        self.policy.apply_settings(preferences.clone());
        self.applied_settings_revision = Some(revision);
        Ok(())
    }

    fn persist_desired(&mut self) -> ModuleResult<()> {
        if !self.restored
            || !self.policy.needs_persistence()
            || self.pending_durable.is_some()
            || !self.journal.is_ready()
            || self.journal.is_busy()
            || self.persistence_error.is_some()
        {
            return Ok(());
        }
        let (revision, document) = self.policy.desired_document();
        match self.journal.begin_store(document.clone()) {
            Ok(()) => self.pending_durable = Some((revision, document)),
            Err(error) => {
                self.persistence_error = Some(format!("owner journal admission failed: {error}"))
            }
        }
        Ok(())
    }

    fn handle_request(
        &mut self,
        id: u64,
        caller: &str,
        service: &str,
        payload: Value,
        now: Instant,
    ) -> ModuleResult<()> {
        if !matches!(
            self.runtime.send(Command::CheckRequest { request: id })?,
            Response::Ok
        ) {
            return Ok(());
        }
        if service == presentation::SERVICE {
            return self.handle_presentation(id, caller, service, payload, now);
        }
        if service != PUBLIC_SERVICE {
            return self.complete(
                id,
                failure("invalid_service", "Remote session service is not supported"),
            );
        }
        if caller != MANAGEMENT {
            return self.complete(
                id,
                failure("forbidden", "Authenticated management is required"),
            );
        }
        if self.shutting_down {
            return self.complete(
                id,
                failure("module_stopping", "Remote sessions are stopping"),
            );
        }
        let decoded = operation_api::decode_delivery(
            payload,
            service,
            "amaterasu.remote-session/private",
            &public_methods(),
        );
        let mut operation = match decoded {
            Ok(value) => value,
            Err(_) => {
                return self.complete(
                    id,
                    failure(
                        "invalid_request",
                        "Request does not match the approved remote-session API",
                    ),
                );
            }
        };
        if matches!(
            operation["action"].as_str(),
            Some("profiles_get" | "profile_upsert" | "profile_delete" | "profile_targets")
        ) {
            return match self.profiles.handle(id, caller, operation) {
                Ok(()) => Ok(()),
                Err(error) => self.complete(id, error),
            };
        }
        // Old settings clients predate saved layouts. Preserve the current
        // layout when absent rather than interpreting omission as a reset.
        if operation["action"] == "settings_update"
            && let Some(settings) = operation["settings"].as_object_mut()
            && !settings.contains_key("remote_monitor_layout")
        {
            settings.insert(
                "remote_monitor_layout".into(),
                self.preferences.settings().0.remote_monitor_layout.clone(),
            );
        }
        let operation = match serde_json::from_value::<PublicOperation>(operation) {
            Ok(operation) => operation,
            Err(_) => {
                return self.complete(
                    id,
                    failure(
                        "invalid_request",
                        "Remote-session settings contain invalid or trusted fields",
                    ),
                );
            }
        };
        match operation {
            PublicOperation::SettingsGet {} => {
                if !self.preferences.is_ready() {
                    return self.complete(
                        id,
                        failure(
                            "configuration_unavailable",
                            "Remote-session preferences are loading",
                        ),
                    );
                }
                let (settings, revision) = self.preferences.settings();
                self.complete(
                    id,
                    bounded_public(json!({"ok":true,"settings":settings,"revision":revision})),
                )
            }
            PublicOperation::SettingsUpdate {
                settings,
                expected_revision,
            } => {
                self.update_preferences(id, settings, expected_revision, PreferencesReply::Settings)
            }
            PublicOperation::LayoutGet {} => self.complete(id, self.layout_response()),
            PublicOperation::LayoutUpdate {
                layout,
                expected_revision,
            } => {
                let layout = match self.validate_layout(layout) {
                    Ok(layout) => layout,
                    Err(error) => return self.complete(id, failure("invalid_layout", &error)),
                };
                let mut settings = self.preferences.settings().0.clone();
                settings.remote_monitor_layout = layout;
                self.update_preferences(id, settings, expected_revision, PreferencesReply::Layout)
            }
        }
    }

    fn handle_presentation(
        &mut self,
        id: u64,
        caller: &str,
        service: &str,
        payload: Value,
        now: Instant,
    ) -> ModuleResult<()> {
        let request = match presentation::decode(service, payload) {
            Ok(request) => request,
            Err(_) => {
                return self.complete_presentation(
                    id,
                    Reply::Error {
                        code: "invalid_request".into(),
                        message: "Request does not match the selected session contract".into(),
                    },
                );
            }
        };
        let cleanup = matches!(
            request,
            Request::Observe { .. }
                | Request::CancelDispatch { .. }
                | Request::ReleaseProject { .. }
                | Request::RevokeClient { .. }
                | Request::RecoveryWork { .. }
                | Request::ReconcileRecovery { .. }
                | Request::BindApplication { .. }
                | Request::BindTransport { .. }
        );
        let allowed = if matches!(request, Request::VerifySource { .. }) {
            self.startup.enabled
                && self
                    .startup
                    .source_verifiers
                    .iter()
                    .any(|principal| principal == caller)
        } else {
            self.startup.allows(caller)
                || cleanup
                    && self
                        .startup
                        .allowed_principals
                        .iter()
                        .any(|principal| principal == caller)
        };
        if !allowed {
            return self.complete_presentation(
                id,
                Reply::Error {
                    code: "forbidden".into(),
                    message: "The selected caller is not authorized for this operation".into(),
                },
            );
        }
        if matches!(request, Request::Describe {}) {
            return self.complete_presentation(
                id,
                Reply::Described {
                    interface_major: 1,
                    interface_minor: 0,
                },
            );
        }
        if (self.shutting_down && !cleanup)
            || !self.restored
            // Restoring the policy already required a successful settings and
            // journal read. Cleanup continues against that retained snapshot
            // after those readers drain, while other modules finish shutdown.
            || (!cleanup && !self.preferences.is_ready())
            || self.persistence_error.is_some()
            || (!cleanup && !self.membership.ready_for(&self.policy.authority_clients()))
        {
            return self.complete_presentation(
                id,
                Reply::Pending {
                    retry_after_ms: 250,
                },
            );
        }
        if let Request::VerifySource { client_id, .. } = &request {
            match self.membership.authorization(client_id) {
                Some(authority) if authority.paired && authority.may_view => {}
                Some(_) => {
                    return self.complete_presentation(
                        id,
                        Reply::Error {
                            code: "forbidden".into(),
                            message: "This source borrower is no longer authorized".into(),
                        },
                    );
                }
                None => {
                    return self.complete_presentation(
                        id,
                        Reply::Pending {
                            retry_after_ms: 250,
                        },
                    );
                }
            }
        }
        let reply = self.policy.handle(caller, request, now, &mut self.monitors);
        self.complete_presentation(id, reply)
    }

    fn update_preferences(
        &mut self,
        id: u64,
        settings: ProductSettings,
        expected_revision: u64,
        kind: PreferencesReply,
    ) -> ModuleResult<()> {
        if settings.remote_monitor_layout != self.preferences.settings().0.remote_monitor_layout
            && let Err(error) = self.validate_layout(settings.remote_monitor_layout.clone())
        {
            return self.complete(id, failure("invalid_layout", &error));
        }
        match self
            .preferences
            .begin_update(id, expected_revision, settings)
        {
            Ok(()) => {
                self.pending_preferences = Some((id, kind));
                Ok(())
            }
            Err(error) => self.complete(id, error),
        }
    }

    fn validate_layout(&self, value: Value) -> Result<Value, String> {
        let layout =
            Layout::decode(&serde_json::to_vec(&value).map_err(|error| error.to_string())?)?;
        let mut clients: BTreeSet<String> = self.journal.snapshot()["owners"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|owner| owner["client_id"].as_str().map(str::to_owned))
            .collect();
        if let Some(previous) =
            self.preferences.settings().0.remote_monitor_layout["placements"].as_object()
        {
            clients.extend(previous.keys().cloned());
        }
        let physical = self
            .monitors
            .manager
            .physical_anchors()
            .iter()
            .filter_map(|anchor| anchor["id"].as_str().map(str::to_owned))
            .collect();
        layout.validate(&clients, &physical)?;
        serde_json::to_value(layout).map_err(|error| error.to_string())
    }

    fn layout_response(&self) -> Value {
        if !self.preferences.is_ready() {
            return failure(
                "configuration_unavailable",
                "Remote-session preferences are loading",
            );
        }
        let (settings, revision) = self.preferences.settings();
        let clients:Vec<Value>=self.journal.snapshot()["owners"].as_array().into_iter().flatten().filter(|owner|owner["role"]=="monitor").filter_map(|owner| {
            let id=owner["client_id"].as_str()?;
            let status=self.monitors.snapshot(id);
            let label=self.membership.authorization(id).and_then(|authority|authority.name.clone()).unwrap_or_else(||format!("Client {}",id.chars().take(8).collect::<String>()));
            let error=status.as_ref().and_then(|status|status.error.as_ref()).map(|error|error.chars().take(256).collect::<String>());
            let ready=status.as_ref().is_some_and(|status|status.ready);
            let state=if error.is_some(){"failed"}else if owner["release_requested"]==true{"retiring"}else if ready{"ready"}else{"preparing"};
            Some(json!({"id":id,"label":label.chars().take(160).collect::<String>(),"ready":ready,"state":state,"error":error}))
        }).collect();
        let anchors=self.monitors.manager.physical_anchors().into_iter().filter_map(|anchor|Some(json!({"id":anchor["id"].as_str()?,"label":anchor["label"].as_str()?.chars().take(160).collect::<String>()}))).collect::<Vec<_>>();
        bounded_public(
            json!({"ok":true,"revision":revision,"layout":settings.remote_monitor_layout,"clients":clients,
            "physical_anchors":anchors,"application_status":layout_summary(&self.monitors.layout_status())}),
        )
    }

    fn complete_presentation(&self, request: u64, reply: Reply) -> ModuleResult<()> {
        reply.validate()?;
        self.complete(
            request,
            serde_json::to_value(reply).map_err(|error| error.to_string())?,
        )
    }

    fn complete(&self, request: u64, payload: Value) -> ModuleResult<()> {
        match self.runtime.send(Command::Complete { request, payload })? {
            Response::Ok => Ok(()),
            Response::Error { code, .. } if code == "request_ended" || code == "not_found" => {
                Ok(())
            }
            _ => Err("remote-session completion failed".into()),
        }
    }
}

fn public_methods() -> Vec<ApiMethod> {
    let methods: Vec<ApiMethod> = serde_json::from_str(include_str!("../metadata/api.json"))
        .expect("valid module-owned API metadata");
    assert!(
        methods
            .iter()
            .all(|method| method.service == PUBLIC_SERVICE)
    );
    methods
}

fn bounded_public(payload: Value) -> Value {
    if serde_json::to_vec(&payload).is_ok_and(|bytes| bytes.len() <= MAX_PUBLIC_BYTES) {
        payload
    } else {
        failure(
            "response_too_large",
            "Remote-session response exceeds the bounded API reply",
        )
    }
}

fn failure(code: &str, message: &str) -> Value {
    json!({"ok":false,"code":code,"message":message})
}

fn layout_summary(status: &Value) -> Value {
    let mut result = json!({"state":status["state"].as_str().unwrap_or("pending")});
    let error = status["monitors"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(
            status["capture_preflights"]
                .as_array()
                .into_iter()
                .flatten(),
        )
        .find_map(|entry| entry["error"].as_str());
    if let Some(error) = error {
        result["error"] = json!(error.chars().take(256).collect::<String>());
    }
    result
}

amaterasu_module_sdk::export_module!(RemoteSessionModule);

#[cfg(test)]
mod tests;
