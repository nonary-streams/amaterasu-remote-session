use super::*;
use serde_json::json;
use std::collections::VecDeque;

struct Provider {
    calls: BTreeMap<u64, Value>,
    operations: BTreeMap<String, u64>,
    next: u64,
    specification: Option<Value>,
    references: BTreeSet<String>,
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
    lose_scope_result: bool,
    existing_references: BTreeSet<String>,
    physical_width: u32,
    mode_change: Option<Value>,
    mode_applied: bool,
    mode_revision: u64,
    external_borrow: bool,
    preflight_unavailable: bool,
    applied_display_ops: BTreeSet<String>,
    mode_prepare_pending: u8,
    mode_commit_pending: u8,
    reject_mode: bool,
    target_unavailable: bool,
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            calls: BTreeMap::new(),
            operations: BTreeMap::new(),
            next: 0,
            specification: None,
            references: BTreeSet::new(),
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
            lose_scope_result: false,
            existing_references: BTreeSet::new(),
            physical_width: 2560,
            mode_change: None,
            mode_applied: false,
            mode_revision: 0,
            external_borrow: false,
            preflight_unavailable: false,
            applied_display_ops: BTreeSet::new(),
            mode_prepare_pending: 0,
            mode_commit_pending: 0,
            reject_mode: false,
            target_unavailable: false,
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
                if let Some(id) = self.operations.get(&operation_id) {
                    return Ok(Response::Accepted { id: *id });
                }
                self.next += 1;
                let id = self.next;
                self.operations.insert(operation_id, id);
                let result = if service == "amaterasu.video/1" {
                    self.preflight(&payload)
                } else if service == amaterasu_module_sdk::display::SERVICE {
                    self.display(&payload)
                } else {
                    assert_eq!(service, SERVICE);
                    self.operate(&payload)
                };
                self.calls.insert(id, result);
                self.observed.push(payload);
                if std::mem::take(&mut self.lose_admission) {
                    return Err("lost admission after commit".into());
                }
                Ok(Response::Accepted { id })
            }
            Command::TakeResult { request } => {
                if self.lose_scope_result
                    && self
                        .calls
                        .get(&request)
                        .is_some_and(|payload| payload["applied"] == true)
                {
                    self.lose_scope_result = false;
                    return Err("lost scoped mutation completion".into());
                }
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
            "prepare_retained_mode" => {
                assert_eq!(payload["expected_mode_revision"], self.mode_revision);
                assert_eq!(
                    payload["expected_reference_ids"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<BTreeSet<_>>(),
                    self.references
                );
                let desired = json!({"identity":payload["identity"],"name":"Remote Monitor","width":payload["width"],"height":payload["height"],"refresh_millihz":payload["refresh_millihz"],"hdr_enabled":payload["hdr_enabled"]});
                self.mode_change = Some(
                    json!({"ok":true,"state":"prepared","operation_id":payload["operation_id"],"specification":desired,"reference_id":payload["reference_id"],"expected_reference_ids":payload["expected_reference_ids"],"expected_mode_revision":payload["expected_mode_revision"]}),
                );
                if self.mode_prepare_pending > 0 {
                    self.mode_prepare_pending -= 1;
                    return json!({"ok":true,"state":"pending","operation_id":payload["operation_id"]});
                }
                self.mode_change.clone().unwrap()
            }
            "commit_retained_mode" => {
                assert!(self.mode_applied);
                if self.mode_commit_pending > 0 {
                    self.mode_commit_pending -= 1;
                    return json!({"ok":true,"state":"pending","operation_id":payload["operation_id"]});
                }
                let mut completed = self.mode_change.clone().unwrap();
                completed["state"] = json!("succeeded");
                self.specification = Some(completed["specification"].clone());
                if self.mode_change.as_ref().unwrap()["state"] != "succeeded" {
                    self.mode_revision += 1;
                }
                self.mode_change = Some(completed.clone());
                completed
            }
            "abort_retained_mode" => {
                assert!(!self.mode_applied);
                let mut aborted = self.mode_change.clone().unwrap();
                aborted["state"] = json!("aborted");
                aborted["specification"] = self.specification.clone().unwrap();
                self.mode_revision += 1;
                aborted["mode_revision"] = json!(self.mode_revision);
                self.mode_change = Some(aborted.clone());
                aborted
            }
            "create" => {
                let first = self.specification.is_none();
                self.specification = Some(json!({
                    "identity":payload["identity"],"width":payload["width"],"height":payload["height"],
                    "refresh_millihz":payload["refresh_millihz"],"hdr_enabled":payload["hdr_enabled"],"name":payload["name"]
                }));
                self.references
                    .insert(payload["reference_id"].as_str().unwrap().into());
                if first {
                    self.state = "creating";
                }
                json!({"ok":true,"reference_owned":true,"monitor":{"state":"creating"}})
            }
            "remove" => {
                self.references
                    .remove(payload["reference_id"].as_str().unwrap());
                self.remove_requested = self.references.is_empty();
                if self.remove_requested && self.specification.is_some() {
                    self.state = "removing";
                }
                json!({"ok":true,"reference_owned":false,"monitor":{"state":"removing"}})
            }
            "status" => {
                let owned = self
                    .references
                    .contains(payload["reference_id"].as_str().unwrap());
                let monitors = self.specification.as_ref().map(|specification| json!({
                    "specification":specification,"state":self.state,"error":null,"mode_revision":self.mode_revision,"retained_mode":self.mode_change,
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

    fn display(&mut self, payload: &Value) -> Value {
        match payload["action"].as_str().unwrap() {
            "resolve_capture_target" => {
                if self.target_unavailable {
                    return json!({"ok":false,"error":{"code":"capture_target_unavailable","message":"selected source is absent"}});
                }
                json!({"ok":true,"target":{"selector":payload["selector"],"label":"Physical display","ready":true,"mode":{"width":self.physical_width,"height":1440,"refresh_millihz":60000,"hdr_enabled":false}},"capture_output":"exact-target:physical-bound","capture_route_key":"desktop_output"})
            }
            "borrow_target" => {
                self.existing_references
                    .insert(payload["reference_id"].as_str().unwrap().into());
                json!({"ok":true,"selector":payload["selector"],"reference_id":payload["reference_id"],"binding":{"output":"exact-target:physical-bound","route_key":"desktop_output","mode":{"width":self.physical_width,"height":1440,"refresh_millihz":60000,"hdr_enabled":false}}})
            }
            "release_target" => {
                self.existing_references
                    .remove(payload["reference_id"].as_str().unwrap());
                json!({"ok":true,"released":true,"native_removed":false,"selector":payload["selector"],"reference_id":payload["reference_id"]})
            }
            "snapshot" => {
                let mut displays = vec![
                    json!({"id":"physical-one","width":1920,"height":1080,"refresh":{"numerator":60000,"denominator":1000},"x":0,"y":0,"rotation":"identity","hdr_enabled":false}),
                ];
                if let Some(spec) = &self.specification {
                    displays.push(json!({"id":"Virtual-7","width":spec["width"],"height":spec["height"],"refresh":{"numerator":spec["refresh_millihz"],"denominator":1000},"x":1920,"y":0,"rotation":"identity","hdr_enabled":spec["hdr_enabled"]}));
                }
                json!({"ok":true,"topology_revision":"0123456789abcdef0123456789abcdef","configuration":{"displays":displays}})
            }
            "scoped_apply" => {
                let reconciled = !self
                    .applied_display_ops
                    .insert(payload["operation_id"].as_str().unwrap().into());
                json!({"ok":true,"applied":!reconciled,"reconciled":reconciled,"operation_id":payload["operation_id"],"target_id":payload["target"]["id"]})
            }
            "scoped_release" => {
                if self.external_borrow {
                    return json!({"ok":false,"error":{"code":"target_in_use","message":"existing source consumers retain this target"}});
                }
                json!({"ok":true,"released":true,"native_removed":false,"operation_id":payload["operation_id"],"target_id":"Virtual-7"})
            }
            "scoped_reconfigure" => {
                if self.reject_mode {
                    return json!({"ok":false,"error":{"code":"unsupported_mode","message":"requested mode is not available"}});
                }
                assert_eq!(
                    payload["mode_operation_id"],
                    self.mode_change.as_ref().unwrap()["operation_id"]
                );
                let reconciled = self.mode_applied;
                self.mode_applied = true;
                json!({"ok":true,"applied":!reconciled,"reconciled":reconciled,"operation_id":payload["operation_id"],"target_id":payload["target"]["id"]})
            }
            "scoped_reconcile" => {
                json!({"ok":true,"state":if self.mode_applied{"succeeded"}else{"not_applied"},"operation_id":payload["operation_id"],"mode_operation_id":self.mode_change.as_ref().unwrap()["operation_id"],"target_id":"Virtual-7"})
            }
            _ => panic!("unknown display operation"),
        }
    }

    fn preflight(&mut self, payload: &Value) -> Value {
        assert_eq!(payload["action"], "preflight_source");
        let request:amaterasu_module_sdk::capture::SourcePreflightRequest=serde_json::from_value(json!({"source_class":payload["source_class"],"route":payload["route"],"mode":payload["mode"]})).unwrap();
        let response = if self.preflight_unavailable {
            amaterasu_module_sdk::capture::SourcePreflightResponse::unavailable(
                &request,
                "capture_backend_unavailable",
            )
        } else {
            amaterasu_module_sdk::capture::SourcePreflightResponse::ready(
                &request,
                vec![amaterasu_module_sdk::capture::SourceModeCapabilities {
                    source_class: request.source_class,
                    min_width: 1,
                    max_width: 8192,
                    min_height: 1,
                    max_height: 8192,
                    min_refresh_millihz: 1000,
                    max_refresh_millihz: 1_000_000,
                    sdr: true,
                    hdr: true,
                    bit_depths: vec![8, 10],
                    format_open: true,
                }],
            )
        };
        json!({"ok":true,"preflight":response})
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
    for _ in 0..24 {
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
        2
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
    tick(&mut manager, &mut provider, now + Duration::from_secs(22));
    tick(&mut manager, &mut provider, now + Duration::from_secs(23));
    assert_eq!(
        provider
            .observed
            .iter()
            .filter(|operation| operation["action"] == "remove")
            .count(),
        attempts + 1
    );
}

#[test]
fn monitor_role_can_disconnect_while_an_independent_game_keeps_the_source() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    assert_eq!(provider.references.len(), 2);
    manager.capture_started("alice", 3, "game-source").unwrap();
    manager.release("alice", 3, now).unwrap();
    tick(&mut manager, &mut provider, now);
    let snapshot = manager.snapshot("alice").unwrap();
    assert!(snapshot.reference_released);
    assert!(!snapshot.cleanup_pending);
    assert!(!snapshot.ready);
    assert!(snapshot.source_ready);
    assert_eq!(snapshot.output.as_deref(), Some("kwin:Virtual-7"));
    assert!(!snapshot.native_retired);
    assert_eq!(provider.references.len(), 1);
    assert!(!provider.remove_requested);
    assert!(manager.begin("alice", 4, request(), now).is_err());
    manager.capture_stopped("alice", 3, "game-source", now);
    tick(&mut manager, &mut provider, now);
    assert!(provider.remove_requested);
    assert!(manager.snapshot("alice").unwrap().cleanup_pending);
    provider.specification = None;
    tick(&mut manager, &mut provider, now + Duration::from_secs(1));
    assert!(manager.snapshot("alice").unwrap().native_retired);
}

#[test]
fn persisted_layout_waits_for_capture_retirement_then_uses_scoped_composition() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    manager.capture_started("alice", 3, "game").unwrap();
    let anchors = manager.physical_anchors();
    let saved=layout::Layout::decode(serde_json::to_vec(&json!({"version":1,"placements":{"alice":{
        "anchor_kind":"physical","anchor_id":anchors[0]["id"],"edge":"left","alignment":"center","gap_px":20
    }}})).unwrap().as_slice()).unwrap();
    manager.set_layout(saved).unwrap();
    let before = provider
        .observed
        .iter()
        .filter(|operation| operation["action"] == "scoped_apply")
        .count();
    tick(&mut manager, &mut provider, now + Duration::from_secs(2));
    assert_eq!(manager.layout_status()["state"], "pending");
    assert_eq!(
        provider
            .observed
            .iter()
            .filter(|operation| operation["action"] == "scoped_apply")
            .count(),
        before
    );
    manager.capture_stopped("alice", 3, "game", now + Duration::from_secs(2));
    tick(&mut manager, &mut provider, now + Duration::from_secs(3));
    assert_eq!(manager.layout_status()["state"], "applied");
    let operation = provider
        .observed
        .iter()
        .rev()
        .find(|operation| operation["action"] == "scoped_apply")
        .unwrap();
    assert_eq!(operation["target"]["x"], -1940);
    assert_eq!(
        operation["reference_id"],
        manager.monitors["alice"].source_reference
    );
}

#[test]
fn uncertain_scoped_completion_reconciles_the_same_native_operation() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider {
        lose_scope_result: true,
        ..Provider::default()
    };
    manager.begin("alice", 3, request(), now).unwrap();
    tick(&mut manager, &mut provider, now);
    provider.state = "ready";
    tick(&mut manager, &mut provider, now + RETRY_INTERVAL);
    assert!(!manager.snapshot("alice").unwrap().ready);
    tick(&mut manager, &mut provider, now + RETRY_INTERVAL * 2);
    assert!(manager.snapshot("alice").unwrap().ready);
    let operations = provider
        .observed
        .iter()
        .filter(|operation| operation["action"] == "scoped_apply")
        .collect::<Vec<_>>();
    assert_eq!(operations.len(), 2);
    assert_eq!(operations[0], operations[1]);
    assert!(provider.calls.is_empty());
}

#[test]
fn existing_target_uses_actual_source_mode_and_never_creates_or_deletes_a_display() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    let mut profile = request();
    profile.profile_settings =
        json!({"monitor_target":{"kind":"existing","selector":"display-target:approved"}});
    manager.begin("alice", 3, profile, now).unwrap();
    tick(&mut manager, &mut provider, now);
    let snapshot = manager.snapshot("alice").unwrap();
    assert!(snapshot.ready);
    assert_eq!(
        snapshot.output.as_deref(),
        Some("exact-target:physical-bound")
    );
    assert_eq!(snapshot.source_route, "desktop_output");
    assert_eq!(snapshot.source_mode.unwrap().width, 2560);
    assert_eq!(provider.existing_references.len(), 2);
    manager.capture_started("alice", 3, "game-1").unwrap();
    manager.release("alice", 3, now).unwrap();
    tick(&mut manager, &mut provider, now);
    assert!(manager.snapshot("alice").unwrap().reference_released);
    assert!(!manager.snapshot("alice").unwrap().cleanup_pending);
    assert!(manager.snapshot("alice").unwrap().source_ready);
    manager.capture_started("alice", 3, "game-2").unwrap();
    manager.capture_stopped("alice", 3, "game-1", now);
    manager.capture_stopped("alice", 3, "game-2", now);
    tick(&mut manager, &mut provider, now);
    assert_eq!(provider.existing_references.len(), 0);
    assert!(!manager.snapshot("alice").unwrap().source_ready);
    assert!(!manager.snapshot("alice").unwrap().native_retired);
    assert!(provider.specification.is_none());
    assert!(!provider.observed.iter().any(|operation| matches!(
        operation["action"].as_str(),
        Some("create" | "remove" | "scoped_apply" | "scoped_release")
    )));
}

#[test]
fn changed_existing_target_never_retargets_a_frozen_source_reservation() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    let mut profile = request();
    profile.profile_settings =
        json!({"monitor_target":{"kind":"existing","selector":"display-target:approved"}});
    manager.begin("alice", 3, profile, now).unwrap();
    tick(&mut manager, &mut provider, now);
    assert!(manager.snapshot("alice").unwrap().ready);
    provider.physical_width = 3840;
    tick(&mut manager, &mut provider, now + HEALTH_INTERVAL);
    let snapshot = manager.snapshot("alice").unwrap();
    assert!(!snapshot.ready);
    assert_eq!(
        snapshot.error.as_deref(),
        Some("capture_target_binding_changed")
    );
    manager.release("alice", 3, now + HEALTH_INTERVAL).unwrap();
    tick(&mut manager, &mut provider, now + HEALTH_INTERVAL);
    assert!(provider.existing_references.is_empty());
    assert!(!provider.remove_requested);
}

#[test]
fn retained_monitor_changes_mode_and_hdr_only_after_every_capture_drains() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    let identity = provider.specification.as_ref().unwrap()["identity"].clone();
    let mut changed = request();
    changed.width = 2560;
    changed.height = 1440;
    changed.refresh_millihz = 120000;
    changed.hdr_enabled = true;
    manager
        .capture_started("alice", 3, "old-transport")
        .unwrap();
    assert_eq!(
        manager.begin("alice", 3, changed.clone(), now).unwrap_err(),
        "monitor_mode_busy"
    );
    manager.capture_stopped("alice", 3, "old-transport", now);
    assert!(!manager.begin("alice", 3, changed, now).unwrap().ready);
    assert!(
        manager
            .capture_started("alice", 3, "new-transport")
            .is_err()
    );
    tick(&mut manager, &mut provider, now);
    let snapshot = manager.snapshot("alice").unwrap();
    assert!(snapshot.ready);
    let mode = snapshot.source_mode.unwrap();
    assert_eq!(
        (mode.width, mode.height, mode.refresh_millihz, mode.hdr),
        (2560, 1440, 120000, true)
    );
    assert_eq!(
        provider.specification.as_ref().unwrap()["identity"],
        identity
    );
    assert_eq!(provider.references.len(), 2);
    assert_eq!(
        provider
            .observed
            .iter()
            .filter(|operation| operation["action"] == "create")
            .count(),
        2
    );
    assert!(
        !provider
            .observed
            .iter()
            .any(|operation| operation["action"] == "remove")
    );
    let phases = provider
        .observed
        .iter()
        .filter_map(|operation| operation["action"].as_str())
        .collect::<Vec<_>>();
    assert!(
        phases
            .iter()
            .position(|phase| *phase == "prepare_retained_mode")
            .unwrap()
            < phases
                .iter()
                .position(|phase| *phase == "scoped_reconfigure")
                .unwrap()
    );
    assert!(
        phases
            .iter()
            .position(|phase| *phase == "scoped_reconfigure")
            .unwrap()
            < phases
                .iter()
                .position(|phase| *phase == "commit_retained_mode")
                .unwrap()
    );
}

#[test]
fn restarted_owner_reconciles_both_exact_references_before_disconnect_completes() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    drop(manager);
    let mut recovered = MonitorManager::new(MonitorSettings::default());
    // The policy layer has durably established that the old transport retired.
    recovered.begin("alice", 3, request(), now).unwrap();
    recovered.release("alice", 3, now).unwrap();
    tick(&mut recovered, &mut provider, now);
    assert!(provider.references.is_empty());
    assert!(provider.remove_requested);
    assert!(
        !recovered
            .snapshot("alice")
            .unwrap()
            .source_reference_released
    );
    provider.specification = None;
    tick(&mut recovered, &mut provider, now + Duration::from_secs(1));
    assert!(
        recovered
            .snapshot("alice")
            .unwrap()
            .source_reference_released
    );
    assert!(recovered.snapshot("alice").unwrap().native_retired);
    assert_eq!(
        provider
            .observed
            .iter()
            .filter(|operation| operation["action"] == "create")
            .count(),
        2
    );
}

#[test]
fn another_clients_read_only_reference_keeps_source_after_role_disconnect() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    provider.external_borrow = true;
    manager.release("alice", 3, now).unwrap();
    tick(&mut manager, &mut provider, now);
    let snapshot = manager.snapshot("alice").unwrap();
    assert!(snapshot.reference_released);
    assert!(!snapshot.cleanup_pending);
    assert!(!snapshot.source_reference_released);
    assert!(snapshot.source_ready);
    assert!(!provider.remove_requested);
    assert_eq!(provider.references.len(), 1);
    provider.external_borrow = false;
    tick(&mut manager, &mut provider, now + HEALTH_INTERVAL);
    assert!(provider.remove_requested);
    provider.specification = None;
    tick(&mut manager, &mut provider, now + HEALTH_INTERVAL * 2);
    assert!(manager.snapshot("alice").unwrap().source_reference_released);
}

#[test]
fn primary_origin_translation_waits_for_other_clients_live_capture() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    let mut borrowed = request();
    borrowed.profile_settings =
        json!({"monitor_target":{"kind":"existing","selector":"display-target:approved"}});
    manager.begin("bob", 4, borrowed, now).unwrap();
    tick(&mut manager, &mut provider, now);
    manager.capture_started("bob", 4, "normal-game").unwrap();
    let anchor = manager.physical_anchors()[0]["id"].clone();
    let desired=layout::Layout::decode(&serde_json::to_vec(&json!({"version":1,"placements":{"alice":{
        "anchor_kind":"physical","anchor_id":anchor,"edge":"right","alignment":"start","gap_px":0,"primary":true
    }}})).unwrap()).unwrap();
    manager.set_layout(desired).unwrap();
    let before = provider
        .observed
        .iter()
        .filter(|operation| operation["action"] == "scoped_apply")
        .count();
    tick(&mut manager, &mut provider, now + Duration::from_secs(2));
    assert_eq!(
        provider
            .observed
            .iter()
            .filter(|operation| operation["action"] == "scoped_apply")
            .count(),
        before
    );
    assert!(manager.monitors["alice"].composition_dirty);
    manager.capture_stopped("bob", 4, "normal-game", now + Duration::from_secs(2));
    tick(&mut manager, &mut provider, now + Duration::from_secs(3));
    assert!(
        provider
            .observed
            .iter()
            .filter(|operation| operation["action"] == "scoped_apply")
            .count()
            > before
    );
}

#[test]
fn exact_existing_source_preflight_precedes_every_borrow_effect() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider {
        preflight_unavailable: true,
        ..Provider::default()
    };
    let mut profile = request();
    profile.profile_settings =
        json!({"monitor_target":{"kind":"existing","selector":"display-target:approved"}});
    manager.begin("alice", 3, profile, now).unwrap();
    tick(&mut manager, &mut provider, now);
    assert!(!manager.snapshot("alice").unwrap().ready);
    assert!(provider.existing_references.is_empty());
    assert!(
        provider
            .observed
            .iter()
            .any(|operation| operation["action"] == "preflight_source"
                && operation["route"] == "exact-target:physical-bound"
                && operation["route_key"] == "desktop_output")
    );
    provider.preflight_unavailable = false;
    tick(&mut manager, &mut provider, now + RETRY_INTERVAL);
    assert!(manager.snapshot("alice").unwrap().ready);
    let phases = provider
        .observed
        .iter()
        .filter_map(|operation| operation["action"].as_str())
        .collect::<Vec<_>>();
    assert!(
        phases
            .iter()
            .position(|phase| *phase == "preflight_source")
            .unwrap()
            < phases
                .iter()
                .position(|phase| *phase == "borrow_target")
                .unwrap()
    );
}

#[test]
fn asynchronous_mode_prepare_and_commit_never_publish_pending_as_ready() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    provider.mode_prepare_pending = 1;
    provider.mode_commit_pending = 1;
    let mut changed = request();
    changed.refresh_millihz = 120000;
    manager.begin("alice", 3, changed, now).unwrap();
    tick(&mut manager, &mut provider, now);
    assert!(!manager.snapshot("alice").unwrap().ready);
    tick(&mut manager, &mut provider, now + RETRY_INTERVAL);
    assert!(!manager.snapshot("alice").unwrap().ready);
    tick(&mut manager, &mut provider, now + RETRY_INTERVAL * 2);
    assert!(manager.snapshot("alice").unwrap().ready);
    assert_eq!(
        manager
            .snapshot("alice")
            .unwrap()
            .source_mode
            .unwrap()
            .refresh_millihz,
        120000
    );
}

#[test]
fn restart_settles_prepared_mode_from_provider_journal_without_replaying_display_effect() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    let mut changed = request();
    changed.width = 2560;
    changed.height = 1440;
    changed.hdr_enabled = true;
    manager.begin("alice", 3, changed.clone(), now).unwrap();
    let prepare = manager.monitors["alice"].mode_operation().unwrap();
    assert_eq!(provider.operate(&prepare)["state"], "prepared");
    provider.mode_applied = true;
    drop(manager);
    let mut recovered = MonitorManager::new(MonitorSettings::default());
    recovered.begin("alice", 3, changed, now).unwrap();
    tick(&mut recovered, &mut provider, now);
    assert!(recovered.snapshot("alice").unwrap().ready);
    assert!(
        provider
            .observed
            .iter()
            .any(|operation| operation["action"] == "scoped_reconcile")
    );
    assert!(
        !provider
            .observed
            .iter()
            .any(|operation| operation["action"] == "scoped_reconfigure")
    );
    assert_eq!(
        provider
            .observed
            .iter()
            .filter(|operation| operation["action"] == "create")
            .count(),
        2
    );
}

#[test]
fn rejected_mode_retires_intent_only_after_old_native_mode_is_verified() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    provider.reject_mode = true;
    let mut unsupported = request();
    unsupported.refresh_millihz = 240000;
    manager.begin("alice", 3, unsupported.clone(), now).unwrap();
    tick(&mut manager, &mut provider, now);
    let snapshot = manager.snapshot("alice").unwrap();
    assert!(!snapshot.ready);
    assert!(!snapshot.retryable);
    assert!(snapshot.source_ready);
    assert_eq!(snapshot.source_mode.unwrap().refresh_millihz, 60000);
    assert_eq!(snapshot.error.as_deref(), Some("unsupported_mode"));
    assert_eq!(provider.references.len(), 2);
    assert!(
        provider
            .observed
            .iter()
            .any(|operation| operation["action"] == "abort_retained_mode")
    );
    assert!(manager.begin("alice", 3, unsupported, now).is_err());
    assert!(manager.begin("alice", 3, request(), now).unwrap().ready);
}

#[test]
fn a_mode_update_before_initial_admission_waits_for_owned_reference_reconciliation() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    manager.begin("alice", 3, request(), now).unwrap();
    let mut changed = request();
    changed.width = 2560;
    changed.height = 1440;
    assert!(!manager.begin("alice", 3, changed, now).unwrap().ready);
    tick(&mut manager, &mut provider, now);
    assert!(provider.mode_change.is_none());
    assert_eq!(provider.specification.as_ref().unwrap()["width"], 1920);
    provider.state = "ready";
    tick(&mut manager, &mut provider, now + RETRY_INTERVAL);
    tick(&mut manager, &mut provider, now + RETRY_INTERVAL * 2);
    assert!(manager.snapshot("alice").unwrap().ready);
    assert_eq!(
        manager
            .snapshot("alice")
            .unwrap()
            .source_mode
            .unwrap()
            .width,
        2560
    );
    assert_eq!(provider.references.len(), 2);
}

#[test]
fn unplugged_borrowed_target_releases_after_its_last_capture_without_resolution() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    let mut profile = request();
    profile.profile_settings =
        json!({"monitor_target":{"kind":"existing","selector":"display-target:approved"}});
    manager.begin("alice", 3, profile, now).unwrap();
    tick(&mut manager, &mut provider, now);
    manager.capture_started("alice", 3, "game").unwrap();
    manager.release("alice", 3, now).unwrap();
    tick(&mut manager, &mut provider, now);
    assert!(manager.snapshot("alice").unwrap().reference_released);
    provider.target_unavailable = true;
    tick(&mut manager, &mut provider, now + HEALTH_INTERVAL);
    manager.capture_stopped("alice", 3, "game", now + HEALTH_INTERVAL);
    tick(&mut manager, &mut provider, now + HEALTH_INTERVAL);
    assert!(manager.snapshot("alice").unwrap().source_reference_released);
    assert!(provider.existing_references.is_empty());
    assert!(!provider.remove_requested);
}

#[test]
fn native_recovery_does_not_leave_this_owners_composition_reference_behind() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    provider.specification = None;
    provider.references.clear();
    manager.release("alice", 3, now).unwrap();
    let before = provider
        .observed
        .iter()
        .filter(|operation| operation["action"] == "scoped_release")
        .count();
    tick(&mut manager, &mut provider, now);
    assert!(manager.snapshot("alice").unwrap().source_reference_released);
    assert!(
        provider
            .observed
            .iter()
            .filter(|operation| operation["action"] == "scoped_release")
            .count()
            > before
    );
}

#[test]
fn lost_native_output_does_not_recreate_or_retire_a_source_with_live_capture_holds() {
    let now = Instant::now();
    let mut manager = MonitorManager::new(MonitorSettings::default());
    let mut provider = Provider::default();
    ready(&mut manager, &mut provider, now);
    manager.capture_started("alice", 3, "old-capture").unwrap();
    provider.specification = None;
    provider.references.clear();
    tick(&mut manager, &mut provider, now + HEALTH_INTERVAL * 2);
    assert!(!manager.snapshot("alice").unwrap().source_ready);
    assert_eq!(
        provider
            .observed
            .iter()
            .filter(|operation| operation["action"] == "create")
            .count(),
        2
    );
    manager
        .release("alice", 3, now + HEALTH_INTERVAL * 2)
        .unwrap();
    tick(&mut manager, &mut provider, now + HEALTH_INTERVAL * 2);
    assert!(!manager.snapshot("alice").unwrap().source_reference_released);
    manager.capture_stopped("alice", 3, "old-capture", now + HEALTH_INTERVAL * 2);
    tick(&mut manager, &mut provider, now + HEALTH_INTERVAL * 3);
    assert!(manager.snapshot("alice").unwrap().source_reference_released);
}
