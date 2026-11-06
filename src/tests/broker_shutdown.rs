//! Exercise the reverse module-drain ordering through the production broker.
//! The consumer must obtain an authoritative empty recovery page after the
//! provider has finished draining its own settings and journal requests.
use super::*;
use amaterasu_module_sdk::requests::RequestOptions;
use amaterasu_runtime::Broker;

const PROVIDER: &str = "amaterasu.remote-session";
const CONSUMER: &str = "wire";
const CONFIGURATION: &str = "amaterasu.configuration";
const STATE: &str = "amaterasu.state";

struct BrokerHost {
    broker: Broker,
}

impl BrokerHost {
    fn new() -> Box<Self> {
        let approvals: Vec<Value> =
            presentation::methods()
                .into_iter()
                .map(|method| json!({"provider":PROVIDER,"callers":[CONSUMER],"method":method}))
                .chain(public_methods().into_iter().map(
                    |method| json!({"provider":PROVIDER,"callers":[MANAGEMENT],"method":method}),
                ))
                .collect();
        let configuration = serde_json::from_value(json!({
            "principals":{
                PROVIDER:{"provide":[presentation::SERVICE,PUBLIC_SERVICE],"call":["amaterasu.configuration/1","amaterasu.state/1"]},
                CONSUMER:{"call":[presentation::SERVICE]},
                MANAGEMENT:{"call":[PUBLIC_SERVICE]},
                CONFIGURATION:{"provide":["amaterasu.configuration/1"],"call":["amaterasu.state/1"]},
                STATE:{"provide":["amaterasu.state/1"]}
            },
            "api_methods":approvals,
            "settings":{PROVIDER:{"allowed_principals":[CONSUMER]}}
        }))
        .unwrap();
        let mut host = Box::new(Self {
            broker: Broker::new(configuration).unwrap(),
        });
        for (principal, service) in [
            (CONFIGURATION, "amaterasu.configuration/1"),
            (STATE, "amaterasu.state/1"),
        ] {
            assert!(matches!(
                host.broker.dispatch(
                    principal,
                    Command::Register {
                        service: service.into()
                    }
                ),
                Response::Ok
            ));
        }
        host
    }

    fn complete_startup_reads(&mut self) {
        for principal in [CONFIGURATION, STATE] {
            while let Response::Request {
                id,
                caller,
                payload,
                ..
            } = self.broker.dispatch(principal, Command::Poll {})
            {
                assert_eq!(caller, PROVIDER);
                let result = match (principal, payload["action"].as_str()) {
                    (CONFIGURATION, Some("product_get")) => {
                        json!({"ok":true,"found":false,"version":null,"revision":0,"values":{}})
                    }
                    (STATE, Some("get")) => {
                        json!({"ok":true,"revision":0,"found":false,"value":null})
                    }
                    unexpected => panic!("unexpected dependency operation: {unexpected:?}"),
                };
                assert!(matches!(
                    self.broker.dispatch(
                        principal,
                        Command::Complete {
                            request: id,
                            payload: result
                        }
                    ),
                    Response::Ok
                ));
            }
        }
    }

    fn submit(&mut self, operation_id: &str, request: &Request) -> u64 {
        let command = presentation::command(
            RequestOptions {
                service: presentation::SERVICE,
                timeout_ms: 5_000,
                operation_id,
                parent: None,
            },
            request,
        )
        .unwrap();
        match self.broker.dispatch(CONSUMER, command) {
            Response::Accepted { id } => id,
            other => panic!("request admission failed: {other:?}"),
        }
    }

    fn take_reply(&mut self, request: u64) -> Reply {
        let Response::Result { payload } = self
            .broker
            .dispatch(CONSUMER, Command::TakeResult { request })
        else {
            panic!("provider did not complete recovery request");
        };
        let reply = presentation::decode_reply(payload).unwrap();
        assert!(matches!(
            self.broker
                .dispatch(CONSUMER, Command::ReleaseResult { request }),
            Response::Ok
        ));
        reply
    }
}

unsafe extern "C" fn invoke_broker(
    context: *mut c_void,
    input: BorrowedBytes,
    reply: ReplySink,
) -> Status {
    // SAFETY: the test keeps the boxed endpoint and input bytes alive during
    // every synchronous call, and makes no concurrent calls to this broker.
    let Ok(bytes) = (unsafe { input.read() }) else {
        return STATUS_INVALID_ARGUMENT;
    };
    let Ok(command) = serde_json::from_slice::<Command>(bytes) else {
        return STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: context points to the stable boxed BrokerHost owned by this test.
    let host = unsafe { &mut *context.cast::<BrokerHost>() };
    let bytes = serde_json::to_vec(&host.broker.dispatch(PROVIDER, command)).unwrap();
    // SAFETY: bytes remains live until the ABI copies this synchronous reply.
    unsafe { reply.send(&bytes) };
    STATUS_OK
}

#[test]
fn queued_recovery_scan_completes_after_provider_dependencies_quiesce() {
    let mut host = BrokerHost::new();
    let endpoint = HostApi {
        header: AbiHeader::of::<HostApi>(),
        context: std::ptr::from_mut(host.as_mut()).cast(),
        invoke: invoke_broker,
    };
    // SAFETY: host's stable boxed allocation outlives the module and its calls.
    let mut module =
        RemoteSessionModule::start(unsafe { RuntimeClient::from_endpoint(endpoint) }).unwrap();

    // A native readiness probe performs exactly one tick, then starts reverse
    // module shutdown. The consumer queues its final recovery scan before the
    // provider has restored its durable snapshot.
    module.tick().unwrap();
    assert!(!module.restored);
    let scan = host.submit(
        "shutdown-empty-scan",
        &Request::RecoveryWork {
            cursor: None,
            limit: presentation::MAX_RECOVERY_ITEMS,
        },
    );
    let mut drained = false;
    for _ in 0..12 {
        host.complete_startup_reads();
        if module.quiesce().unwrap() {
            drained = true;
            break;
        }
    }
    assert!(
        drained,
        "empty provider did not retire its dependency requests"
    );
    assert!(module.restored);
    assert!(!module.preferences.is_ready());
    assert!(!module.journal.is_ready());

    // The host continues ticking all modules while another consumer drains.
    module.tick().unwrap();
    assert!(
        matches!(host.take_reply(scan), Reply::RecoveryWork { items, next_cursor }
        if items.is_empty() && next_cursor.is_none())
    );

    // Reconciliation is also a cleanup operation and must reach the retained
    // policy snapshot instead of waiting for a settings reader that is closed.
    let reconcile = host.submit(
        "shutdown-reconcile",
        &Request::ReconcileRecovery {
            work_id: "unknown-work".into(),
        },
    );
    module.tick().unwrap();
    assert!(
        matches!(host.take_reply(reconcile), Reply::Error { code, .. }
        if code == "recovery_work_not_found")
    );
    assert!(module.quiesce().unwrap());
}
