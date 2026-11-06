use super::*;
use std::collections::BTreeMap;

const UUID: &str = "f920e4f5-e140-45e4-a737-947c8ce523c0";
fn profile() -> Profile {
    Profile {
        id: String::new(),
        uuid: String::new(),
        name: "Second screen".into(),
        width: 1920,
        height: 1080,
        refresh_millihz: 60000,
        hdr_enabled: false,
        scale: 125,
        target: ProfileTarget::Owned,
    }
}
fn row(profile: &Profile, id: &str) -> Value {
    json!({"id":id,"uuid":profile.uuid,"title":profile.name,"aliases":[],"visible":true,"monitor_profile":true,"properties":profile.entry()["properties"]})
}
fn page(revision: u64, entries: Vec<Value>, cursor: Value) -> Value {
    json!({"ok":true,"revision":revision,"entries":entries,"next_cursor":cursor})
}

#[derive(Default)]
struct Host {
    next: u64,
    results: BTreeMap<u64, Value>,
    commands: Vec<Command>,
    lose_admission_once: bool,
    fail_release: bool,
    parent_live: bool,
}
impl Host {
    fn tick(
        &mut self,
        manager: &mut ProfileManager,
        provider: &mut impl FnMut(&str, &Value) -> Value,
    ) {
        manager
            .tick(&mut |command| {
                self.commands.push(command.clone());
                Ok(match command {
                    Command::CheckRequest { .. } => {
                        if self.parent_live {
                            Response::Ok
                        } else {
                            Response::error("request_ended", "expired")
                        }
                    }
                    Command::Request {
                        service,
                        payload,
                        operation_id,
                        ..
                    } => {
                        let existing =
                            self.commands
                                .iter()
                                .rev()
                                .skip(1)
                                .find_map(|command| match command {
                                    Command::Request {
                                        operation_id: previous,
                                        ..
                                    } if previous == &operation_id => Some(self.next),
                                    _ => None,
                                });
                        let id = if let Some(id) = existing {
                            id
                        } else {
                            self.next += 1;
                            self.results.insert(self.next, provider(&service, &payload));
                            self.next
                        };
                        if std::mem::take(&mut self.lose_admission_once) {
                            return Err("lost admission reply".into());
                        }
                        Response::Accepted { id }
                    }
                    Command::TakeResult { request } => Response::Result {
                        payload: self.results[&request].clone(),
                    },
                    Command::ReleaseResult { .. } if self.fail_release => {
                        return Err("release unavailable".into());
                    }
                    Command::ReleaseResult { request } => {
                        self.results.remove(&request);
                        Response::Ok
                    }
                    Command::Cancel { .. } => Response::Ok,
                    other => panic!("unexpected command {other:?}"),
                })
            })
            .unwrap();
    }
}

#[test]
fn create_returns_catalog_id_uuid_and_revision_only_after_result_retirement() {
    let mut manager = ProfileManager::new("startup");
    manager
        .handle(
            50,
            "amaterasu.management",
            json!({"action":"profile_upsert","profile":profile(),"expected_revision":7}),
        )
        .unwrap();
    let mut host = Host {
        parent_live: true,
        ..Default::default()
    };
    let mut provider = |service: &str, payload: &Value| {
        assert_eq!(service, CATALOG);
        match payload["action"].as_str().unwrap() {
            "profile_list" => page(7, Vec::new(), Value::Null),
            "profile_upsert" => {
                assert_eq!(payload["id"], "");
                assert_eq!(payload["expected_revision"], 7);
                let uuid = payload["entry"]["uuid"].as_str().unwrap();
                assert!(valid_uuid(uuid));
                assert_ne!(uuid, UUID);
                let mut expected = profile();
                expected.uuid = uuid.into();
                json!({"ok":true,"revision":8,"profile":row(&expected,"app-0000000000000004")})
            }
            other => panic!("unexpected action {other}"),
        }
    };
    for _ in 0..5 {
        host.tick(&mut manager, &mut provider);
    }
    host.fail_release = true;
    for _ in 0..3 {
        host.tick(&mut manager, &mut provider);
        assert!(manager.take_completion().is_none());
    }
    host.fail_release = false;
    host.tick(&mut manager, &mut provider);
    let (request, response) = manager.take_completion().expect("profile never completed");
    assert_eq!(request, 50);
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(response["profile"]["id"], "app-0000000000000004");
    assert_eq!(response["revision"], 8);
    assert!(
        host.commands
            .iter()
            .filter_map(|command| match command {
                Command::Request { parent, .. } => Some(parent),
                _ => None,
            })
            .all(|parent| *parent == Some(50))
    );
    assert!(host.results.is_empty());
}

#[test]
fn listing_rejects_revision_change_without_publishing_partial_profiles() {
    let mut manager = ProfileManager::new("startup");
    manager
        .handle(10, "amaterasu.management", json!({"action":"profiles_get"}))
        .unwrap();
    let mut host = Host {
        parent_live: true,
        ..Default::default()
    };
    let mut calls = 0;
    let mut existing = profile();
    existing.uuid = UUID.into();
    let mut provider = |_: &str, _: &Value| {
        calls += 1;
        if calls == 1 {
            page(
                3,
                vec![row(&existing, "app-0000000000000001")],
                json!({"revision":3,"after":"app-0000000000000001","query":PAGE_QUERY}),
            )
        } else {
            page(4, vec![row(&existing, "app-0000000000000002")], Value::Null)
        }
    };
    for _ in 0..8 {
        host.tick(&mut manager, &mut provider);
    }
    let (_, response) = manager.take_completion().unwrap();
    assert_eq!(response["code"], "revision_conflict");
    assert!(response.get("profiles").is_none());
}

#[test]
fn mutation_preflight_fences_stale_revision_and_immutable_uuid() {
    for (expected_revision, uuid, expected_code) in [
        (3, UUID, "revision_conflict"),
        (
            4,
            "4d7994f2-9f1c-4b67-b175-41ce5a7c8979",
            "profile_identity_mismatch",
        ),
    ] {
        let mut manager = ProfileManager::new("startup");
        let mut desired = profile();
        desired.id = "app-0000000000000001".into();
        desired.uuid = uuid.into();
        manager.handle(11,"amaterasu.management",json!({"action":"profile_upsert","profile":desired,"expected_revision":expected_revision})).unwrap();
        let mut stored = profile();
        stored.uuid = UUID.into();
        let mut host = Host {
            parent_live: true,
            ..Default::default()
        };
        let mut provider = |_: &str, payload: &Value| {
            assert_eq!(payload["action"], "profile_list");
            page(4, vec![row(&stored, "app-0000000000000001")], Value::Null)
        };
        for _ in 0..4 {
            host.tick(&mut manager, &mut provider);
        }
        let (_, response) = manager.take_completion().unwrap();
        assert_eq!(response["code"], expected_code);
        assert_eq!(
            host.commands
                .iter()
                .filter(|command| matches!(command, Command::Request { .. }))
                .count(),
            1
        );
    }
}

#[test]
fn management_authorization_and_strict_profile_schema_precede_provider_calls() {
    let mut manager = ProfileManager::new("startup");
    assert!(
        manager
            .handle(1, "untrusted", json!({"action":"profiles_get"}))
            .is_err()
    );
    for extra in [
        json!({"launch":{"program":"/private/tool"}}),
        json!({"allowed_principals":["untrusted"]}),
        json!({"target":{"kind":"existing","selector":"\ninvalid"}}),
        json!({"target":{"kind":"existing","selector":"opaque","device_path":"/private/native"}}),
    ] {
        let mut desired = serde_json::to_value(profile()).unwrap();
        desired
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(
            manager
                .handle(
                    1,
                    "amaterasu.management",
                    json!({"action":"profile_upsert","profile":desired,"expected_revision":0})
                )
                .is_err()
        );
    }
    assert!(manager.active.is_none());
    assert!(manager.queue.is_empty());
}

#[test]
fn target_picker_keeps_opaque_selectors_and_strips_native_identity() {
    let result=project_targets(json!({"ok":true,"targets":[{"selector":"target:opaque","label":"Display 1","ready":true,"mode":{"width":1920,"height":1080,"refresh_millihz":60000,"hdr_enabled":true,"device_path":"private"},"device_path":"private","native_handle":123}]})).unwrap();
    let target = &result["targets"][0];
    assert_eq!(target["selector"], "target:opaque");
    assert!(target.get("device_path").is_none());
    assert!(target["mode"].get("device_path").is_none());
    assert!(project_targets(json!({"ok":true,"targets":[target,target]})).is_err());
}

#[test]
fn target_inventory_uses_the_selected_display_contract_and_parent_authority() {
    let mut manager = ProfileManager::new("startup");
    manager
        .handle(
            22,
            "amaterasu.management",
            json!({"action":"profile_targets"}),
        )
        .unwrap();
    let mut host = Host {
        parent_live: true,
        ..Default::default()
    };
    let mut provider = |service: &str, payload: &Value| {
        assert_eq!(service, DISPLAY);
        assert_eq!(*payload, json!({"action":"capture_targets"}));
        json!({"ok":true,"targets":[{"selector":"opaque-target","label":"Unavailable display","ready":false,"error":"not_ready","device_path":"private"}]})
    };
    for _ in 0..3 {
        host.tick(&mut manager, &mut provider);
    }
    let (request, response) = manager.take_completion().unwrap();
    assert_eq!(request, 22);
    assert_eq!(
        response,
        json!({"ok":true,"targets":[{"selector":"opaque-target","label":"Unavailable display","ready":false,"error":"not_ready"}]})
    );
    assert!(host.commands.iter().any(|command|matches!(command,Command::Request {service,parent:Some(22),..} if service==DISPLAY)));
}

#[test]
fn lost_admission_retries_exact_payload_and_quiesce_waits_for_release() {
    let mut manager = ProfileManager::new("startup");
    manager
        .handle(10, "amaterasu.management", json!({"action":"profiles_get"}))
        .unwrap();
    let mut host = Host {
        parent_live: true,
        lose_admission_once: true,
        ..Default::default()
    };
    let mut provider = |_: &str, _: &Value| page(0, Vec::new(), Value::Null);
    host.tick(&mut manager, &mut provider);
    host.tick(&mut manager, &mut provider);
    let requests: Vec<_> = host
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::Request {
                payload,
                operation_id,
                ..
            } => Some((payload.clone(), operation_id.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    manager.shutting_down = true;
    host.fail_release = true;
    for _ in 0..4 {
        host.tick(&mut manager, &mut provider);
    }
    assert!(manager.active.is_some());
    assert!(manager.take_completion().is_none());
    host.fail_release = false;
    host.tick(&mut manager, &mut provider);
    assert!(manager.active.is_none());
    assert!(host.results.is_empty());
}

#[test]
fn cancelled_parent_cannot_start_a_following_catalog_mutation() {
    let mut manager = ProfileManager::new("startup");
    manager
        .handle(
            10,
            "amaterasu.management",
            json!({"action":"profile_upsert","profile":profile(),"expected_revision":0}),
        )
        .unwrap();
    let mut host = Host {
        parent_live: true,
        ..Default::default()
    };
    let mut provider = |_: &str, payload: &Value| {
        assert_eq!(payload["action"], "profile_list");
        page(0, Vec::new(), Value::Null)
    };
    for _ in 0..3 {
        host.tick(&mut manager, &mut provider);
    }
    host.parent_live = false;
    for _ in 0..4 {
        host.tick(&mut manager, &mut provider);
    }
    assert!(manager.active.is_none());
    assert_eq!(
        host.commands
            .iter()
            .filter(|command| matches!(command, Command::Request { .. }))
            .count(),
        1
    );
    assert!(manager.take_completion().is_none());
}
