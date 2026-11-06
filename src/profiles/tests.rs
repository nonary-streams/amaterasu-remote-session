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
    for _ in 0..2 {
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
        .handle(
            10,
            "amaterasu.management",
            json!({"action":"profiles_get","cursor":{"revision":3,"after":"app-0000000000000001"}}),
        )
        .unwrap();
    let mut host = Host {
        parent_live: true,
        ..Default::default()
    };
    let mut existing = profile();
    existing.uuid = UUID.into();
    let mut provider = |_: &str, payload: &Value| {
        assert_eq!(payload["cursor"]["revision"], 3);
        assert_eq!(payload["cursor"]["query"], PAGE_QUERY);
        page(4, vec![row(&existing, "app-0000000000000002")], Value::Null)
    };
    for _ in 0..3 {
        host.tick(&mut manager, &mut provider);
    }
    let (_, response) = manager.take_completion().unwrap();
    assert_eq!(response["code"], "revision_conflict");
    assert!(response.get("profiles").is_none());
}

#[test]
fn mutation_delegation_preserves_catalog_cas_and_immutable_identity_failures() {
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
        let mut host = Host {
            parent_live: true,
            ..Default::default()
        };
        let mut provider = |_: &str, payload: &Value| {
            assert_eq!(payload["action"], "profile_upsert");
            assert_eq!(payload["expected_revision"], expected_revision);
            assert_eq!(payload["entry"]["uuid"], uuid);
            json!({"ok":false,"error":{"code":expected_code,"message":"catalog rejected stale mutation","current_revision":4}})
        };
        for _ in 0..3 {
            host.tick(&mut manager, &mut provider);
        }
        let (_, response) = manager.take_completion().unwrap();
        assert_eq!(response["code"], expected_code);
        assert_eq!(response["current_revision"], 4);
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
fn cancelled_parent_cannot_start_a_catalog_mutation() {
    let mut manager = ProfileManager::new("startup");
    manager
        .handle(
            10,
            "amaterasu.management",
            json!({"action":"profile_upsert","profile":profile(),"expected_revision":0}),
        )
        .unwrap();
    let mut host = Host {
        parent_live: false,
        ..Default::default()
    };
    let mut provider = |_: &str, _: &Value| panic!("cancelled parent reached provider");
    for _ in 0..3 {
        host.tick(&mut manager, &mut provider);
    }
    assert!(manager.active.is_none());
    assert!(
        !host
            .commands
            .iter()
            .any(|command| matches!(command, Command::Request { .. }))
    );
    assert!(manager.take_completion().is_none());
}

#[test]
fn more_than_sixteen_profiles_are_read_across_consistent_bounded_pages() {
    let mut manager = ProfileManager::new("startup");
    let mut host = Host {
        parent_live: true,
        ..Default::default()
    };
    let records: Vec<_> = (1..=45)
        .map(|number| {
            let mut item = profile();
            item.uuid = format!("f920e4f5-e140-45e4-a737-{number:012x}");
            row(&item, &format!("app-{number:016x}"))
        })
        .collect();
    let mut provider = |_: &str, payload: &Value| {
        assert_eq!(payload["action"], "profile_list");
        assert_eq!(payload["limit"], 16);
        let after = payload["cursor"]["after"].as_str().unwrap_or("");
        let available: Vec<_> = records
            .iter()
            .filter(|row| row["id"].as_str().unwrap() > after)
            .collect();
        let entries: Vec<_> = available
            .iter()
            .take(16)
            .map(|row| (*row).clone())
            .collect();
        let cursor = if available.len() > entries.len() {
            json!({"revision":9,"after":entries.last().unwrap()["id"],"query":PAGE_QUERY})
        } else {
            Value::Null
        };
        page(9, entries, cursor)
    };
    let mut cursor = Value::Null;
    let mut collected = Vec::new();
    let mut request = 0;
    loop {
        request += 1;
        let mut payload = json!({"action":"profiles_get"});
        if !cursor.is_null() {
            payload["cursor"] = cursor;
        }
        manager
            .handle(request, "amaterasu.management", payload)
            .unwrap();
        for _ in 0..3 {
            host.tick(&mut manager, &mut provider);
        }
        let (_, response) = manager.take_completion().unwrap();
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["revision"], 9);
        assert!(serde_json::to_vec(&response).unwrap().len() <= 12000);
        collected.extend(
            response["profiles"]
                .as_array()
                .unwrap()
                .iter()
                .map(|profile| profile["id"].as_str().unwrap().to_owned()),
        );
        cursor = response["next_cursor"].clone();
        if cursor.is_null() {
            break;
        }
        assert!(request < 10);
    }
    assert_eq!(collected.len(), 45);
    assert_eq!(request, 3);
    assert!(collected.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn page_bounds_retain_continuation_instead_of_truncating_expanded_profiles() {
    let entries:Vec<_>=(1..=28).map(|number|json!({"id":format!("app-{number:016x}"),"uuid":format!("f920e4f5-e140-45e4-a737-{number:012x}"),"title":"x".repeat(160),"aliases":[],"visible":true,"monitor_profile":true,"properties":{"monitor_target":{"kind":"existing","selector":"s".repeat(64)}}})).collect();
    let response = project_profile_page(page(3, entries, Value::Null), None, 32).unwrap();
    assert!(serde_json::to_vec(&response).unwrap().len() <= 12000);
    let profiles = response["profiles"].as_array().unwrap();
    assert!(!profiles.is_empty());
    assert!(profiles.len() < 28);
    assert_eq!(
        response["next_cursor"]["after"],
        profiles.last().unwrap()["id"]
    );
    assert_eq!(response["next_cursor"]["revision"], 3);
}

#[test]
fn empty_sparse_pages_advance_and_invalid_limits_or_cursors_are_rejected() {
    let cursor = ProfileCursor {
        revision: 3,
        after: "app-0000000000000001".into(),
    };
    let response = project_profile_page(
        page(
            3,
            Vec::new(),
            json!({"revision":3,"after":"app-0000000000000100","query":PAGE_QUERY}),
        ),
        Some(&cursor),
        16,
    )
    .unwrap();
    assert_eq!(response["profiles"], json!([]));
    assert_eq!(response["next_cursor"]["after"], "app-0000000000000100");
    let mut manager = ProfileManager::new("startup");
    for extra in [
        json!({"limit":0}),
        json!({"limit":33}),
        json!({"cursor":{"revision":3,"after":"invalid"}}),
        json!({"cursor":{"revision":3,"after":"app-0000000000000001","owner":"foreign"}}),
    ] {
        let mut payload = json!({"action":"profiles_get"});
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(manager.handle(1, "amaterasu.management", payload).is_err());
    }
    assert!(
        project_profile_page(
            page(
                3,
                Vec::new(),
                json!({"revision":3,"after":cursor.after,"query":PAGE_QUERY})
            ),
            Some(&cursor),
            16
        )
        .is_err()
    );
}
