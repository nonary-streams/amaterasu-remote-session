use super::*;
use amaterasu_module_sdk::capture::{SourceModeCapabilities, SourcePreflightResponse};

fn owner() -> RetainedOwner {
    serde_json::from_value(json!({"client_id":"client","consumer":"wire","session_id":"session","generation":1,"role":"monitor","role_active":true,"role_committed":false,
        "mode":{"width":1920,"height":1080,"refresh_millihz":60000,"hdr":false},"profile_id":"builtin-monitor","profile_settings":{},"release_requested":false,"reservations":[]})).unwrap()
}

struct Video {
    request: Option<SourcePreflightRequest>,
    command: Option<Command>,
    ready: bool,
    actions: Vec<String>,
    released: bool,
}

impl Video {
    fn new(ready: bool) -> Self {
        Self {
            request: None,
            command: None,
            ready,
            actions: Vec::new(),
            released: false,
        }
    }
    fn send(&mut self, command: Command) -> ModuleResult<Response> {
        match command {
            Command::Request {
                service, payload, ..
            } => {
                assert_eq!(service, "amaterasu.video/1");
                assert_eq!(payload["action"], "preflight_source");
                self.actions
                    .push(payload["route_key"].as_str().unwrap().to_owned());
                self.request=Some(serde_json::from_value(json!({"source_class":payload["source_class"],"route":payload["route"],"mode":payload["mode"]})).unwrap());
                self.command = Some(Command::Request {
                    service,
                    payload,
                    timeout_ms: 5000,
                    operation_id: "observed".into(),
                    parent: None,
                });
                Ok(Response::Accepted { id: 1 })
            }
            Command::TakeResult { request: 1 } => {
                let request = self.request.as_ref().unwrap();
                let response = if self.ready {
                    SourcePreflightResponse::ready(
                        request,
                        vec![SourceModeCapabilities {
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
                            format_open: false,
                        }],
                    )
                } else {
                    SourcePreflightResponse::unsupported(request, "source_unsupported")
                };
                Ok(Response::Result {
                    payload: json!({"ok":true,"preflight":response}),
                })
            }
            Command::ReleaseResult { request: 1 } => {
                self.released = true;
                Ok(Response::Ok)
            }
            unexpected => panic!(
                "preflight allocated native resources or used an unexpected command {unexpected:?}"
            ),
        }
    }
}

#[test]
fn missing_authority_and_pending_preflight_cannot_begin_a_native_monitor() {
    let now = Instant::now();
    let mut port = MonitorPort::new(&ProductSettings::default(), "gate");
    assert_eq!(
        port.begin(&owner(), now).unwrap_err(),
        "monitor_client_authority_pending"
    );
    assert_eq!(port.manager.active_count(), 0);
    port.authorize_sources(BTreeMap::from([("client".into(), true)]), true);
    assert_eq!(
        port.begin(&owner(), now).unwrap_err(),
        "capture_source_preflight_pending"
    );
    let mut video = Video::new(true);
    port.tick(now, &mut |command| video.send(command)).unwrap();
    port.tick(now, &mut |command| video.send(command)).unwrap();
    assert_eq!(port.manager.active_count(), 0);
    assert!(!video.released);
    assert!(port.begin(&owner(), now).is_err());
    port.tick(now, &mut |command| video.send(command)).unwrap();
    assert!(video.released);
    port.begin(&owner(), now).unwrap();
    let status = port.snapshot("client").unwrap();
    assert!(!status.ready);
    assert!(!status.source_ready);
    assert!(status.source_mode.is_none());
    assert!(status.output.is_none());
    assert_eq!(video.actions, vec!["managed_output"]);
}

#[test]
fn unsupported_capture_never_creates_a_display_and_reports_pending_application() {
    let now = Instant::now();
    let mut port = MonitorPort::new(&ProductSettings::default(), "unsupported");
    port.authorize_sources(BTreeMap::from([("client".into(), true)]), true);
    assert!(port.begin(&owner(), now).is_err());
    let mut video = Video::new(false);
    for _ in 0..3 {
        port.tick(now, &mut |command| video.send(command)).unwrap();
    }
    assert!(port.begin(&owner(), now).is_err());
    assert_eq!(port.manager.active_count(), 0);
    assert_eq!(port.layout_status()["state"], "failed");
    assert!(video.released);
    let status = port.snapshot("client").unwrap();
    assert!(!status.retryable);
    assert_eq!(status.error.as_deref(), Some("source_unsupported"));
    assert!(!status.reference_released);
    assert!(!status.source_reference_released);
    port.release("client", 1, now).unwrap();
    assert!(
        !port
            .manager
            .snapshot("client")
            .unwrap()
            .source_reference_released,
        "a cleanup actor must still obtain native retirement proof"
    );
}

#[test]
fn existing_targets_preflight_the_desktop_route_without_inventing_a_selector() {
    let now = Instant::now();
    let mut port = MonitorPort::new(&ProductSettings::default(), "existing");
    port.authorize_sources(BTreeMap::from([("client".into(), true)]), true);
    let mut existing = owner();
    existing.mode.width = 64;
    existing.mode.height = 48;
    existing.profile_settings =
        json!({"monitor_target":{"kind":"existing","selector":"opaque-target"}});
    assert!(port.begin(&existing, now).is_err());
    let mut video = Video::new(true);
    for _ in 0..3 {
        port.tick(now, &mut |command| video.send(command)).unwrap();
    }
    assert_eq!(video.actions, vec!["desktop_output"]);
    let request = video.request.unwrap();
    assert_eq!(request.source_class, SourceClass::DesktopOutput);
    assert_eq!(request.route, None);
    assert_eq!(port.manager.active_count(), 0);
    port.begin(&existing, now).unwrap();
    assert_eq!(port.manager.active_count(), 1);
}

#[derive(Default)]
struct AbsentMonitorProviders {
    next_request: u64,
    results: BTreeMap<u64, Value>,
    actions: Vec<(String, Value)>,
}

impl AbsentMonitorProviders {
    fn send(&mut self, command: Command) -> ModuleResult<Response> {
        match command {
            Command::Request {
                service, payload, ..
            } => {
                self.actions.push((service.clone(), payload.clone()));
                let response = match (service.as_str(), payload["action"].as_str()) {
                    ("amaterasu.virtual-display/1", Some("remove")) => {
                        json!({"ok":true,"removed":true,"identity":payload["identity"]})
                    }
                    ("amaterasu.virtual-display/1", Some("status")) => {
                        json!({"ok":true,"reference_owned":false,"monitors":[]})
                    }
                    ("amaterasu.display/1", Some("scoped_status")) => {
                        json!({"ok":true,"identity":payload["identity"],"reference_id":payload["reference_id"],"reference_owned":false,"operation_id":null,"pending":false})
                    }
                    unexpected => panic!("cleanup attempted a new source effect: {unexpected:?}"),
                };
                self.next_request += 1;
                self.results.insert(self.next_request, response);
                Ok(Response::Accepted {
                    id: self.next_request,
                })
            }
            Command::TakeResult { request } => Ok(Response::Result {
                payload: self.results.get(&request).unwrap().clone(),
            }),
            Command::ReleaseResult { request } => {
                assert!(self.results.remove(&request).is_some());
                Ok(Response::Ok)
            }
            unexpected => panic!("unexpected cleanup command: {unexpected:?}"),
        }
    }
}

fn drain_absent_monitor(port: &mut MonitorPort, owner: &RetainedOwner, now: Instant) {
    let mut providers = AbsentMonitorProviders::default();
    for step in 0..40 {
        let current = now + Duration::from_millis(step * 250);
        // Policy synchronization repeats while the independently observed
        // provider result is pending. Repeated cleanup cannot reset progress.
        port.begin(owner, current).unwrap();
        port.release(&owner.client_id, owner.generation, current)
            .unwrap();
        port.tick(current, &mut |command| providers.send(command))
            .unwrap();
        if port
            .snapshot(&owner.client_id)
            .unwrap()
            .source_reference_released
        {
            break;
        }
    }
    let status = port.snapshot(&owner.client_id).unwrap();
    assert!(status.reference_released);
    assert!(status.source_reference_released);
    assert!(!status.cleanup_pending);
    assert!(!status.ready);
    assert!(!status.source_ready);
    assert!(providers.results.is_empty());
    let references = providers
        .actions
        .iter()
        .filter(|(service, payload)| {
            service == "amaterasu.virtual-display/1" && payload["action"] == "status"
        })
        .map(|(_, payload)| payload["reference_id"].as_str().unwrap().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        references.len(),
        2,
        "role and source require separate absence proofs"
    );
    assert!(
        providers
            .actions
            .iter()
            .any(|(service, payload)| service == "amaterasu.display/1"
                && payload["action"] == "scoped_status")
    );
}

#[test]
fn persisted_invalid_monitor_mode_retires_without_authority_or_capture_preflight() {
    let now = Instant::now();
    let mut port = MonitorPort::new(&ProductSettings::default(), "persisted-cleanup");
    let mut retiring = owner();
    retiring.mode.width = 64;
    retiring.mode.height = 48;
    retiring.release_requested = true;
    port.begin(&retiring, now).unwrap();
    assert!(!port.snapshot("client").unwrap().reference_released);
    assert!(!port.snapshot("client").unwrap().source_reference_released);
    assert!(port.proofs.is_empty());
    drain_absent_monitor(&mut port, &retiring, now);
}

#[test]
fn rejected_small_monitor_can_start_exact_cleanup_from_its_frozen_proof() {
    let now = Instant::now();
    let mut port = MonitorPort::new(&ProductSettings::default(), "rejected-cleanup");
    port.authorize_sources(BTreeMap::from([("client".into(), true)]), true);
    let mut rejected = owner();
    rejected.mode.width = 64;
    rejected.mode.height = 48;
    assert!(port.begin(&rejected, now).is_err());
    let mut video = Video::new(false);
    for _ in 0..3 {
        port.tick(now, &mut |command| video.send(command)).unwrap();
    }
    assert!(video.released);
    assert!(port.manager.snapshot("client").is_none());
    port.release("client", 1, now).unwrap();
    assert!(!port.snapshot("client").unwrap().source_reference_released);
    rejected.release_requested = true;
    drain_absent_monitor(&mut port, &rejected, now);
}

#[test]
fn temporary_authority_loss_keeps_existing_capture_capability_and_native_ownership() {
    let now = Instant::now();
    let mut port = MonitorPort::new(&ProductSettings::default(), "authority");
    port.authorize_sources(BTreeMap::from([("client".into(), true)]), true);
    assert!(port.begin(&owner(), now).is_err());
    let mut video = Video::new(true);
    for _ in 0..3 {
        port.tick(now, &mut |command| video.send(command)).unwrap();
    }
    port.begin(&owner(), now).unwrap();
    port.authorize_sources(BTreeMap::new(), false);
    assert!(port.proofs[&("client".into(), 1)].ready);
    assert_eq!(port.manager.active_count(), 1);
    assert!(!port.snapshot("client").unwrap().ready);
    port.authorize_sources(BTreeMap::from([("client".into(), true)]), true);
    assert!(port.begin(&owner(), now).is_ok());
    assert_eq!(
        video.actions.len(),
        1,
        "unchanged capabilities survive an authority outage while their admission remains blocked"
    );
}
