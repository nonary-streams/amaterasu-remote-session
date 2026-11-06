use super::*;
use serde_json::json;
use std::collections::VecDeque;

struct Provider {
    calls: BTreeMap<u64, Value>,
    operations: BTreeMap<String, u64>,
    next: u64,
    specification: Option<Value>,
    reference: Option<String>,
    state: &'static str,
    capture_output: Option<String>,
    parked: bool,
    remove_requested: bool,
    released: usize,
    lose_admission: bool,
    lose_release: bool,
    pending: bool,
    observed: Vec<Value>,
    failures: VecDeque<String>,
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            calls: BTreeMap::new(),
            operations: BTreeMap::new(),
            next: 0,
            specification: None,
            reference: None,
            state: "creating",
            capture_output: Some("kwin:Virtual-7".into()),
            parked: false,
            remove_requested: false,
            released: 0,
            lose_admission: false,
            lose_release: false,
            pending: false,
            observed: Vec::new(),
            failures: VecDeque::new(),
        }
    }
}

impl Provider {
    fn send(&mut self, command: Command) -> ModuleResult<Response> {
        match command {
            Command::Request {
                service,
                payload,
                operation_id,
                ..
            } => {
                assert_eq!(service, SERVICE);
                if let Some(id) = self.operations.get(&operation_id) {
                    return Ok(Response::Accepted { id: *id });
                }
                self.next += 1;
                let id = self.next;
                self.operations.insert(operation_id, id);
                let result = self.operate(&payload);
                self.calls.insert(id, result);
                self.observed.push(payload);
                if std::mem::take(&mut self.lose_admission) {
                    return Err("lost admission after commit".into());
                }
                Ok(Response::Accepted { id })
            }
            Command::TakeResult { request } => {
                if self.pending {
                    return Ok(Response::Pending);
                }
                self.calls
                    .get(&request)
                    .cloned()
                    .map(|payload| Response::Result { payload })
                    .ok_or("unknown request".into())
            }
            Command::ReleaseResult { request } => {
                if std::mem::take(&mut self.lose_release) {
                    return Err("lost release reply".into());
                }
                self.calls.remove(&request);
                self.operations.retain(|_, id| *id != request);
                self.released += 1;
                Ok(Response::Ok)
            }
            Command::Cancel { request } => {
                self.calls
                    .insert(request, json!({"ok":false,"code":"cancelled"}));
                Ok(Response::Ok)
            }
            _ => Err("unexpected command".into()),
        }
    }

    fn operate(&mut self, payload: &Value) -> Value {
        if let Some(error) = self.failures.pop_front() {
            return json!({"ok":false,"code":error});
        }
        match payload["action"].as_str().unwrap() {
            "create" => {
                self.specification = Some(json!({
                    "identity":payload["identity"],"width":payload["width"],"height":payload["height"],
                    "refresh_millihz":payload["refresh_millihz"],"hdr_enabled":payload["hdr_enabled"],"name":payload["name"]
                }));
                self.reference = payload["reference_id"].as_str().map(str::to_owned);
                self.state = "creating";
                json!({"ok":true,"reference_owned":true,"monitor":{"state":"creating"}})
            }
            "remove" => {
                self.reference = None;
                self.remove_requested = true;
                if self.specification.is_some() {
                    self.state = "removing";
                }
                json!({"ok":true,"reference_owned":false,"monitor":{"state":"removing"}})
            }
            "status" => {
                let owned = self.reference.as_deref() == payload["reference_id"].as_str()
                    && self.reference.is_some();
                let monitors = self.specification.as_ref().map(|specification| json!({
                    "specification":specification,"state":self.state,"error":null,
                    "capture_output":self.capture_output,"ready":{"device_path":"Virtual-7","active":self.state=="ready"}
                })).into_iter().collect::<Vec<_>>();
                let parked_outputs = self
                    .parked
                    .then(|| json!({"identity":payload["identity"],"output":"Virtual-7"}))
                    .into_iter()
                    .collect::<Vec<_>>();
                json!({"ok":true,"driver":"libvirtualdisplay","reference_owned":owned,"monitors":monitors,"parked":self.parked,"parked_outputs":parked_outputs})
            }
            _ => panic!("unknown provider operation"),
        }
    }
}

fn request() -> MonitorRequest {
    MonitorRequest {
        profile_id: "profile".into(),
        width: 1920,
        height: 1080,
        refresh_millihz: 60_000,
        hdr_enabled: false,
        profile_settings: json!({}),
    }
}

fn tick(manager: &mut MonitorManager, provider: &mut Provider, now: Instant) {
    for _ in 0..16 {
        manager
            .tick(now, &mut |command| provider.send(command))
            .unwrap();
    }
}

fn ready(manager: &mut MonitorManager, provider: &mut Provider, now: Instant) {
    manager.begin("alice", 3, request(), now).unwrap();
    tick(manager, provider, now);
    provider.state = "ready";
    tick(manager, provider, now + RETRY_INTERVAL);
    assert!(manager.snapshot("alice").unwrap().ready);
}

#[test]
fn admission_is_not_readiness_and_exact_target_is_required() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    manager.begin("alice", 3, request(), now).unwrap();
    tick(&mut manager, &mut provider, now);
    assert!(!manager.snapshot("alice").unwrap().ready);
    assert_eq!(provider.specification.as_ref().unwrap()["width"], 1920);
    assert!(
        provider
            .observed
            .iter()
            .any(|operation| operation["action"] == "create"
                && operation["virtual_display_layout"] == "extended")
    );
    provider.state = "ready";
    provider.capture_output = None;
    tick(&mut manager, &mut provider, now + RETRY_INTERVAL);
    assert!(!manager.snapshot("alice").unwrap().ready);
    assert_eq!(
        manager.snapshot("alice").unwrap().error.as_deref(),
        Some("exact_monitor_capture_unavailable")
    );
    provider.capture_output = Some("kwin:Virtual-7".into());
    tick(&mut manager, &mut provider, now + RETRY_INTERVAL * 2);
    assert_eq!(
        manager.snapshot("alice").unwrap().output.as_deref(),
        Some("kwin:Virtual-7")
    );
}

#[test]
fn a_lost_admission_reuses_the_exact_operation_and_releases_every_result() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider {
        lose_admission: true,
        lose_release: true,
        ..Provider::default()
    };
    ready(&mut manager, &mut provider, now);
    assert_eq!(
        provider
            .observed
            .iter()
            .filter(|operation| operation["action"] == "create")
            .count(),
        1
    );
    assert!(provider.calls.is_empty());
    assert_eq!(provider.released, provider.observed.len());
}

#[test]
fn disconnect_waits_for_every_idempotent_capture_hold_and_native_retirement() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    manager.capture_started("alice", 3, "transport-1").unwrap();
    manager.capture_started("alice", 3, "transport-1").unwrap();
    manager.capture_started("alice", 3, "transport-2").unwrap();
    manager.release("alice", 3, now).unwrap();
    tick(&mut manager, &mut provider, now + Duration::from_secs(2));
    assert!(!provider.remove_requested);
    manager.capture_stopped("alice", 2, "transport-1", now);
    manager.capture_stopped("alice", 3, "transport-1", now);
    tick(&mut manager, &mut provider, now + Duration::from_secs(2));
    assert!(!provider.remove_requested);
    manager.capture_stopped("alice", 3, "transport-2", now);
    tick(&mut manager, &mut provider, now + Duration::from_secs(2));
    assert!(provider.remove_requested);
    assert!(manager.snapshot("alice").unwrap().cleanup_pending);
    assert!(!manager.snapshot("alice").unwrap().native_retired);
    provider.specification = None;
    tick(&mut manager, &mut provider, now + Duration::from_secs(3));
    assert!(manager.snapshot("alice").unwrap().native_retired);
    assert!(!manager.snapshot("alice").unwrap().cleanup_pending);
}

#[test]
fn old_generations_and_profile_edits_never_retarget_retained_ownership() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    let mut changed = request();
    changed.profile_settings = json!({"dd_virtual_display_scale":150});
    assert_eq!(
        manager.begin("alice", 3, changed, now).unwrap_err(),
        "retained_monitor_profile_conflict"
    );
    assert!(manager.release("alice", 2, now).is_err());
    assert!(!manager.snapshot("alice").unwrap().cleanup_pending);
    manager.release("alice", 3, now).unwrap();
    tick(&mut manager, &mut provider, now);
    provider.specification = None;
    tick(&mut manager, &mut provider, now + Duration::from_secs(1));
    assert!(manager.begin("alice", 2, request(), now).is_err());
    manager.begin("alice", 4, request(), now).unwrap();
    assert!(manager.release("alice", 3, now).is_err());
    assert_eq!(manager.snapshot("alice").unwrap().generation, 4);
}

#[test]
fn independent_disconnect_policies_preserve_retained_monitors_by_default() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    manager.stream_end("alice", 3, now);
    manager.client_disconnect("alice", 3, now);
    assert!(manager.snapshot("alice").unwrap().ready);
    manager.unpair("alice", now);
    assert!(manager.snapshot("alice").unwrap().cleanup_pending);
    assert!(manager.has_pending_cleanup());
}

#[test]
fn safe_park_is_reference_retirement_and_is_not_physical_removal() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    manager.release("alice", 3, now).unwrap();
    tick(&mut manager, &mut provider, now);
    provider.specification = None;
    provider.parked = true;
    tick(&mut manager, &mut provider, now + Duration::from_secs(1));
    let snapshot = manager.snapshot("alice").unwrap();
    assert!(snapshot.reference_released);
    assert!(snapshot.parked);
    assert!(!snapshot.native_retired);
    assert!(!snapshot.cleanup_pending);
}

#[test]
fn bad_status_cannot_publish_a_different_mode_or_output() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    provider.specification.as_mut().unwrap()["width"] = json!(1280);
    tick(&mut manager, &mut provider, now + Duration::from_secs(2));
    assert!(!manager.snapshot("alice").unwrap().ready);
    assert_eq!(
        manager.snapshot("alice").unwrap().error.as_deref(),
        Some("monitor_specification_mismatch")
    );
    manager.release("alice", 3, now).unwrap();
    tick(&mut manager, &mut provider, now + Duration::from_secs(3));
    assert!(provider.remove_requested);
}

#[test]
fn bounded_capacity_includes_pending_monitors_and_shutdown_waits_for_capture() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings {
        max_monitors: 1,
        ..MonitorSettings::default()
    });
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    assert!(manager.begin("bob", 1, request(), now).is_err());
    manager.capture_started("alice", 3, "transport").unwrap();
    manager.shutdown(now);
    tick(&mut manager, &mut provider, now);
    assert!(!provider.remove_requested);
    assert!(manager.begin("bob", 1, request(), now).is_err());
    manager.capture_stopped("alice", 3, "transport", now);
    tick(&mut manager, &mut provider, now);
    assert!(provider.remove_requested);
}

#[test]
fn request_timeout_retires_broker_request_before_provider_reconciliation() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider {
        pending: true,
        ..Provider::default()
    };
    manager.begin("alice", 3, request(), now).unwrap();
    tick(&mut manager, &mut provider, now);
    assert_eq!(provider.calls.len(), 1);
    tick(&mut manager, &mut provider, now + REQUEST_TIMEOUT);
    assert!(provider.calls.is_empty());
    assert_eq!(provider.released, 1);
    assert_eq!(
        manager.snapshot("alice").unwrap().error.as_deref(),
        Some("monitor_request_cancelled")
    );
}

#[test]
fn routine_status_polling_releases_more_than_the_broker_ledger_capacity() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    for second in 1..=600 {
        tick(
            &mut manager,
            &mut provider,
            now + Duration::from_secs(second),
        );
    }
    assert!(manager.snapshot("alice").unwrap().ready);
    assert!(provider.calls.is_empty());
    assert!(provider.released > 512);
}

#[test]
fn failed_native_cleanup_uses_a_bounded_window_until_an_explicit_retry() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    manager.release("alice", 3, now).unwrap();
    for step in 0..20 {
        provider.state = "cleanup_failed";
        tick(&mut manager, &mut provider, now + Duration::from_secs(step));
    }
    let attempts = provider
        .observed
        .iter()
        .filter(|operation| operation["action"] == "remove")
        .count();
    assert_eq!(attempts, usize::from(MAX_EFFECT_ATTEMPTS));
    assert_eq!(
        manager.snapshot("alice").unwrap().error.as_deref(),
        Some("monitor_cleanup_requires_retry")
    );
    manager
        .release("alice", 3, now + Duration::from_secs(21))
        .unwrap();
    tick(&mut manager, &mut provider, now + Duration::from_secs(21));
    assert_eq!(
        provider
            .observed
            .iter()
            .filter(|operation| operation["action"] == "remove")
            .count(),
        attempts + 1
    );
}
