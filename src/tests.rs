use super::*;
use amaterasu_module_sdk::abi::{
    AbiHeader, BorrowedBytes, HostApi, ReplySink, STATUS_INVALID_ARGUMENT, STATUS_OK, Status,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::c_void,
};

struct Outbound {
    service: String,
    payload: Value,
    result: Option<Value>,
}

struct Host {
    settings: Value,
    methods: Vec<ApiMethod>,
    incoming: VecDeque<Response>,
    active: BTreeSet<u64>,
    completed: BTreeMap<u64, Value>,
    requests: BTreeMap<u64, Outbound>,
    next: u64,
    preferences: Option<(Value, u64)>,
    journal: Option<Value>,
    state_revision: u64,
    services: Vec<(String, Value)>,
    allow_state_commit: bool,
    paired: bool,
}

impl Default for Host {
    fn default() -> Self {
        Self {
            settings: json!({"allowed_principals":["wire"],"source_verifiers":["verifier"]}),
            methods: Vec::new(),
            incoming: VecDeque::new(),
            active: BTreeSet::new(),
            completed: BTreeMap::new(),
            requests: BTreeMap::new(),
            next: 0,
            preferences: None,
            journal: None,
            state_revision: 0,
            services: Vec::new(),
            allow_state_commit: true,
            paired: true,
        }
    }
}

impl Host {
    fn submit(&mut self, id: u64, caller: &str, service: &str, payload: Value) {
        self.active.insert(id);
        self.incoming.push_back(Response::Request {
            id,
            caller: caller.into(),
            initiator: caller.into(),
            service: service.into(),
            payload,
            remaining_ms: 30_000,
        });
    }

    fn dispatch(&mut self, command: Command) -> Response {
        match command {
            Command::GetSettings {} => Response::Result {
                payload: self.settings.clone(),
            },
            Command::RegisterApi { method } => {
                self.methods.push(method);
                Response::Ok
            }
            Command::Poll {} => self.incoming.pop_front().unwrap_or(Response::Pending),
            Command::CheckRequest { request } => {
                if self.active.contains(&request) {
                    Response::Ok
                } else {
                    Response::error("request_ended", "ended")
                }
            }
            Command::Complete { request, payload } => {
                self.active.remove(&request);
                self.completed.insert(request, payload);
                Response::Ok
            }
            Command::Request {
                service,
                payload,
                parent,
                ..
            } => {
                if parent.is_some_and(|parent| !self.active.contains(&parent)) {
                    return Response::error("request_ended", "parent ended");
                }
                self.next += 1;
                self.services.push((service.clone(), payload.clone()));
                self.requests.insert(
                    self.next,
                    Outbound {
                        service,
                        payload,
                        result: None,
                    },
                );
                Response::Accepted { id: self.next }
            }
            Command::TakeResult { request } => {
                let Some(call) = self.requests.get(&request) else {
                    return Response::error("not_found", "retired");
                };
                if call.result.is_none() {
                    if call.service == "amaterasu.state/1"
                        && call.payload["action"] == "set"
                        && !self.allow_state_commit
                    {
                        return Response::Pending;
                    }
                    let result = self.dependency(call.service.clone(), call.payload.clone());
                    self.requests.get_mut(&request).unwrap().result = Some(result);
                }
                Response::Result {
                    payload: self.requests[&request].result.clone().unwrap(),
                }
            }
            Command::ReleaseResult { request } => {
                if self.requests.remove(&request).is_some() {
                    Response::Ok
                } else {
                    Response::error("not_found", "retired")
                }
            }
            Command::Cancel { request } => {
                self.requests.remove(&request);
                Response::Ok
            }
            unexpected => panic!("runtime wrapper attempted unexpected command {unexpected:?}"),
        }
    }

    fn dependency(&mut self, service: String, payload: Value) -> Value {
        match (service.as_str(), payload["action"].as_str()) {
            ("amaterasu.configuration/1", Some("product_get")) => match &self.preferences {
                Some((values, revision)) => {
                    json!({"ok":true,"found":true,"version":1,"revision":revision,"values":values})
                }
                None => json!({"ok":true,"found":false,"version":null,"revision":0,"values":{}}),
            },
            ("amaterasu.configuration/1", Some("product_update")) => {
                let revision = self
                    .preferences
                    .as_ref()
                    .map_or(0, |(_, revision)| *revision);
                if payload["expected_revision"] != revision {
                    return json!({"ok":false,"error":{"code":"revision_conflict","message":"changed","current_revision":revision}});
                }
                self.preferences = Some((payload["values"].clone(), revision + 1));
                json!({"ok":true,"found":true,"version":1,"revision":revision+1,"values":payload["values"]})
            }
            ("amaterasu.state/1", Some("get")) => {
                json!({"ok":true,"revision":self.state_revision,"found":self.journal.is_some(),"value":self.journal})
            }
            ("amaterasu.state/1", Some("set")) => {
                if payload["expected_revision"] != self.state_revision {
                    return json!({"ok":false,"error":{"code":"revision_conflict","message":"changed","current_revision":self.state_revision}});
                }
                self.state_revision += 1;
                self.journal = Some(payload["value"].clone());
                json!({"ok":true,"revision":self.state_revision})
            }
            ("amaterasu.pairing/1", Some("client_authorization")) => {
                json!({"ok":true,"certificate_id":payload["certificate_id"],"paired":self.paired,"may_view":self.paired,"may_launch":self.paired,"may_terminate":self.paired,"revision":1,"name":if self.paired {Some("Player")}else{None}})
            }
            ("amaterasu.rtsp/1", Some("retirement_status")) => {
                json!({"ok":true,"operation_id":payload["operation_id"],"expected_runtime_epoch":payload["expected_runtime_epoch"],"runtime_epoch":payload["expected_runtime_epoch"],"startup_reconciled":true,"state":"active","retired":false,"active_operations":1,"pending_admissions":0,"cleanup_pending":false})
            }
            unexpected => {
                panic!("runtime attempted unexpected dependency operation {unexpected:?}")
            }
        }
    }
}

unsafe extern "C" fn invoke(
    context: *mut c_void,
    input: BorrowedBytes,
    reply: ReplySink,
) -> Status {
    // SAFETY: each test retains the boxed Host and synchronous borrowed buffers
    // until its module is dropped; no worker thread uses this fixture endpoint.
    let Ok(bytes) = (unsafe { input.read() }) else {
        return STATUS_INVALID_ARGUMENT;
    };
    let Ok(command) = serde_json::from_slice::<Command>(bytes) else {
        return STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: the fixture supplies this live exclusive Host allocation.
    let host = unsafe { &mut *context.cast::<Host>() };
    let bytes = serde_json::to_vec(&host.dispatch(command)).unwrap();
    // SAFETY: the reply byte allocation remains live for the synchronous copy.
    unsafe { reply.send(&bytes) };
    STATUS_OK
}

fn start(host: &mut Box<Host>) -> RemoteSessionModule {
    let endpoint = HostApi {
        header: AbiHeader::of::<HostApi>(),
        context: std::ptr::from_mut(host.as_mut()).cast(),
        invoke,
    };
    // SAFETY: tests keep Host's stable boxed allocation alive beyond this module.
    RemoteSessionModule::start(unsafe { RuntimeClient::from_endpoint(endpoint) }).unwrap()
}

fn drive(module: &mut RemoteSessionModule, ticks: usize) {
    for _ in 0..ticks {
        module.tick().unwrap();
    }
}
fn envelope(method: &str, arguments: Value) -> Value {
    json!({"method":method,"interface_major":1,"interface_minor":0,"arguments":arguments})
}
fn input_dispatch() -> Value {
    envelope(
        "dispatch",
        json!({"context":{"client":{"id":"ab".repeat(32),"paired":true,"may_view":true,"may_launch":true,"may_terminate":true},"game":null},"operation_id":"input-request","intent":"launch","selection":{"id":"2147483506","uuid":null},"mode":{"width":1920,"height":1080,"refresh_millihz":60000,"hdr":false}}),
    )
}

#[test]
fn startup_registers_only_exact_approved_apis_and_rejects_raw_alias_operations() {
    let mut host = Box::<Host>::default();
    let mut module = start(&mut host);
    drive(&mut module, 4);
    assert_eq!(
        host.methods.len(),
        presentation::methods().len() + public_methods().len()
    );
    assert!(
        host.methods
            .iter()
            .any(|method| method.service == presentation::SERVICE && method.method == "describe")
    );
    host.submit(
        100,
        "wire",
        presentation::SERVICE,
        json!({"action":"describe"}),
    );
    host.submit(
        101,
        "wire",
        PUBLIC_SERVICE,
        envelope("settings_get", json!({})),
    );
    host.submit(
        102,
        MANAGEMENT,
        PUBLIC_SERVICE,
        json!({"action":"settings_get"}),
    );
    drive(&mut module, 1);
    assert_eq!(host.completed[&100]["code"], "invalid_request");
    assert_eq!(host.completed[&101]["code"], "forbidden");
    assert_eq!(host.completed[&102]["code"], "invalid_request");
}

#[test]
fn source_verifier_does_not_gain_other_generic_or_public_operations() {
    let mut host = Box::<Host>::default();
    let mut module = start(&mut host);
    drive(&mut module, 4);
    host.submit(
        100,
        "verifier",
        presentation::SERVICE,
        envelope("describe", json!({})),
    );
    host.submit(
        101,
        "wire",
        presentation::SERVICE,
        envelope("describe", json!({})),
    );
    host.submit(
        102,
        "verifier",
        PUBLIC_SERVICE,
        envelope("settings_get", json!({})),
    );
    drive(&mut module, 1);
    assert_eq!(host.completed[&100]["code"], "forbidden");
    assert_eq!(host.completed[&101]["result"], "described");
    assert_eq!(host.completed[&102]["code"], "forbidden");
}

#[test]
fn pending_owner_persistence_never_delivers_an_auxiliary_plan_or_creates_a_display() {
    let mut host = Box::<Host>::default();
    let mut module = start(&mut host);
    drive(&mut module, 4);
    host.allow_state_commit = false;
    host.submit(100, "wire", presentation::SERVICE, input_dispatch());
    drive(&mut module, 8);
    host.submit(101, "wire", presentation::SERVICE, input_dispatch());
    drive(&mut module, 1);
    assert_eq!(host.completed[&100]["result"], "pending");
    assert_eq!(host.completed[&101]["result"], "pending");
    assert!(host.journal.is_none());
    assert!(
        !host
            .services
            .iter()
            .any(|(service, _)| service == "amaterasu.virtual-display/1")
    );
    host.allow_state_commit = true;
    drive(&mut module, 12);
    host.submit(102, "wire", presentation::SERVICE, input_dispatch());
    drive(&mut module, 1);
    assert_eq!(host.completed[&102]["result"], "dispatched");
    assert_eq!(host.completed[&102]["decision"]["kind"], "auxiliary");
    assert!(
        host.journal.as_ref().unwrap()["owners"]
            .as_array()
            .is_some_and(|owners| owners.len() == 1)
    );
    assert!(
        !host
            .services
            .iter()
            .any(|(service, _)| service == "amaterasu.virtual-display/1")
    );
}

#[test]
fn old_settings_forms_preserve_saved_layout_and_cannot_change_startup_grants() {
    let mut host = Box::<Host>::default();
    let mut saved = serde_json::to_value(ProductSettings::default()).unwrap();
    let layout = json!({"version":1,"placements":{"offline":{"anchor_kind":"physical","anchor_id":"unavailable","edge":"right","alignment":"center","gap_px":0}}});
    saved["remote_monitor_layout"] = layout.clone();
    host.preferences = Some((saved.clone(), 7));
    let mut module = start(&mut host);
    drive(&mut module, 4);
    let mut legacy = saved;
    legacy
        .as_object_mut()
        .unwrap()
        .remove("remote_monitor_layout");
    legacy["remote_monitor_mute_audio"] = json!(true);
    host.submit(
        100,
        MANAGEMENT,
        PUBLIC_SERVICE,
        envelope(
            "settings_update",
            json!({"settings":legacy,"expected_revision":7}),
        ),
    );
    drive(&mut module, 5);
    assert_eq!(host.completed[&100]["ok"], true);
    assert_eq!(
        host.preferences.as_ref().unwrap().0["remote_monitor_layout"],
        layout
    );
    let mut invalid = host.preferences.as_ref().unwrap().0.clone();
    invalid["source_verifiers"] = json!([MANAGEMENT]);
    host.submit(
        101,
        MANAGEMENT,
        PUBLIC_SERVICE,
        envelope(
            "settings_update",
            json!({"settings":invalid,"expected_revision":8}),
        ),
    );
    drive(&mut module, 1);
    assert_eq!(host.completed[&101]["code"], "invalid_request");
    assert_eq!(host.preferences.as_ref().unwrap().1, 8);
}

#[test]
fn empty_module_quiescence_retires_all_async_dependency_results() {
    let mut host = Box::<Host>::default();
    let mut module = start(&mut host);
    drive(&mut module, 4);
    let mut settled = false;
    for _ in 0..8 {
        if module.quiesce().unwrap() {
            settled = true;
            break;
        }
    }
    assert!(settled);
    assert!(host.requests.is_empty());
}

#[test]
fn restored_unpaired_input_owner_is_released_only_after_fresh_authority_observation() {
    let mut host = Box::<Host>::default();
    host.paired = false;
    host.state_revision = 1;
    host.journal = Some(json!({"version":1,"next_generation":2,"owners":[{
        "client_id":"ab".repeat(32),"consumer":"wire","session_id":"retained-input","generation":1,"role":"input","role_active":true,"role_committed":false,
        "mode":{"width":1920,"height":1080,"refresh_millihz":60000,"hdr":false},"profile_id":"input","profile_settings":{},"release_requested":false,"reservations":[],
    }]}));
    let mut module = start(&mut host);
    drive(&mut module, 4);
    assert!(
        !host.journal.as_ref().unwrap()["owners"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    drive(&mut module, 20);
    assert!(
        host.services
            .iter()
            .any(|(service, payload)| service == "amaterasu.pairing/1"
                && payload["action"] == "client_authorization")
    );
    assert!(
        host.journal.as_ref().unwrap()["owners"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(host.journal.as_ref().unwrap()["next_generation"], 2);
    assert!(
        !host
            .services
            .iter()
            .any(|(service, _)| service == "amaterasu.virtual-display/1")
    );
}
