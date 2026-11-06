use super::*;
use std::collections::BTreeMap;

struct Call {
    operation_id: String,
    payload: Value,
    parent: Option<u64>,
    result: Option<Value>,
    cancelled: bool,
}

#[derive(Default)]
struct Configuration {
    document: Option<(ProductSettings, u64)>,
    calls: BTreeMap<u64, Call>,
    next: u64,
    management_active: bool,
    lose_admission: bool,
    lose_release: bool,
    submissions: Vec<(String, Value, Option<u64>)>,
}

impl Configuration {
    fn send(&mut self, command: Command) -> ModuleResult<Response> {
        match command {
            Command::CheckRequest { request: 88 } => Ok(if self.management_active {
                Response::Ok
            } else {
                Response::error("request_ended", "management request ended")
            }),
            Command::Request {
                service,
                payload,
                timeout_ms,
                operation_id,
                parent,
            } => {
                assert_eq!(service, CONFIGURATION);
                assert_eq!(timeout_ms, TIMEOUT_MS);
                assert!(operation_id.len() < 128);
                self.submissions
                    .push((operation_id.clone(), payload.clone(), parent));
                if let Some((id, previous)) = self
                    .calls
                    .iter()
                    .find(|(_, call)| call.operation_id == operation_id)
                {
                    assert_eq!(previous.payload, payload);
                    assert_eq!(previous.parent, parent);
                    return Ok(Response::Accepted { id: *id });
                }
                if parent.is_some() && !self.management_active {
                    return Ok(Response::error("request_ended", "parent ended"));
                }
                self.next += 1;
                self.calls.insert(
                    self.next,
                    Call {
                        operation_id,
                        payload,
                        parent,
                        result: None,
                        cancelled: false,
                    },
                );
                if self.lose_admission {
                    self.lose_admission = false;
                    Err("admission reply lost".into())
                } else {
                    Ok(Response::Accepted { id: self.next })
                }
            }
            Command::TakeResult { request } => {
                if !self.calls.contains_key(&request) {
                    return Ok(Response::error("not_found", "retired"));
                }
                if self.calls[&request].cancelled {
                    return Ok(Response::error("cancelled", "cancelled"));
                }
                if self.calls[&request].result.is_none() {
                    let payload = self.calls[&request].payload.clone();
                    let result = match payload["action"].as_str().unwrap() {
                        "product_get" => self.document(),
                        "product_update" => {
                            assert_eq!(payload["version"], DOCUMENT_VERSION);
                            let revision =
                                self.document.as_ref().map_or(0, |(_, revision)| *revision);
                            if payload["expected_revision"] != revision {
                                json!({"ok":false,"error":{"code":"revision_conflict","message":"changed","current_revision":revision}})
                            } else {
                                let desired: ProductSettings =
                                    serde_json::from_value(payload["values"].clone()).unwrap();
                                desired.validate().unwrap();
                                self.document = Some((desired, revision + 1));
                                self.document()
                            }
                        }
                        other => panic!("unexpected configuration action {other}"),
                    };
                    self.calls.get_mut(&request).unwrap().result = Some(result);
                }
                Ok(Response::Result {
                    payload: self.calls[&request].result.clone().unwrap(),
                })
            }
            Command::Cancel { request } => {
                if let Some(call) = self.calls.get_mut(&request) {
                    if call.result.is_none() {
                        call.cancelled = true;
                    }
                    Ok(Response::Ok)
                } else {
                    Ok(Response::error("not_found", "retired"))
                }
            }
            Command::ReleaseResult { request } => {
                let Some(call) = self.calls.get(&request) else {
                    return Ok(Response::error("not_found", "retired"));
                };
                assert!(
                    call.cancelled || call.result.is_some(),
                    "active request released"
                );
                self.calls.remove(&request);
                if self.lose_release {
                    self.lose_release = false;
                    Err("release reply lost".into())
                } else {
                    Ok(Response::Ok)
                }
            }
            other => panic!("unexpected command {other:?}"),
        }
    }

    fn document(&self) -> Value {
        match &self.document {
            Some((settings, revision)) => {
                json!({"ok":true,"found":true,"version":DOCUMENT_VERSION,"revision":revision,"values":settings})
            }
            None => json!({"ok":true,"found":false,"version":null,"revision":0,"values":{}}),
        }
    }

    fn load(&mut self, store: &mut ProductSettingsStore) {
        for _ in 0..8 {
            if store.is_ready() {
                break;
            }
            store.advance(&mut |command| self.send(command)).unwrap();
        }
        assert!(store.is_ready());
        assert!(self.calls.is_empty());
    }

    fn finish_update(&mut self, store: &mut ProductSettingsStore) -> Value {
        for _ in 0..12 {
            store.advance(&mut |command| self.send(command)).unwrap();
            if let Some((request, response)) = store.take_completion() {
                assert_eq!(request, 88);
                return response;
            }
        }
        panic!("settings update did not settle")
    }
}

#[test]
fn absent_document_uses_bootstrap_preferences_and_existing_document_keeps_saved_values() {
    let defaults = ProductSettings {
        max_clients: 4,
        ..ProductSettings::default()
    };
    let mut store = ProductSettingsStore::new("fresh", defaults.clone());
    let mut configuration = Configuration::default();
    configuration.load(&mut store);
    assert_eq!(store.settings(), (&defaults, 0));
    assert_eq!(
        configuration.document, None,
        "loading preferences must not write startup data"
    );

    let saved = ProductSettings {
        max_clients: 7,
        remote_monitor_mute_audio: true,
        ..ProductSettings::default()
    };
    configuration.document = Some((saved.clone(), 9));
    let mut restarted = ProductSettingsStore::new("restart", defaults);
    configuration.load(&mut restarted);
    assert_eq!(restarted.settings(), (&saved, 9));
}

#[test]
fn update_requires_exact_revision_and_verified_persisted_values_before_completion() {
    let mut store = ProductSettingsStore::new("update", ProductSettings::default());
    let mut configuration = Configuration {
        management_active: true,
        ..Default::default()
    };
    configuration.load(&mut store);
    let desired = ProductSettings {
        remote_monitor_mute_audio: true,
        ..ProductSettings::default()
    };
    assert_eq!(
        store.begin_update(88, 1, desired.clone()).unwrap_err()["code"],
        "revision_conflict"
    );
    store.begin_update(88, 0, desired.clone()).unwrap();
    store
        .advance(&mut |command| configuration.send(command))
        .unwrap();
    store
        .advance(&mut |command| configuration.send(command))
        .unwrap();
    assert_eq!(store.settings().1, 0);
    assert!(store.take_completion().is_none());
    assert_eq!(configuration.document, Some((desired.clone(), 1)));
    assert_eq!(
        configuration.calls.len(),
        1,
        "persistence result still requires retirement"
    );
    let response = configuration.finish_update(&mut store);
    assert_eq!(response["ok"], true);
    assert_eq!(store.settings(), (&desired, 1));
    assert!(configuration.calls.is_empty());
}

#[test]
fn lost_admission_and_release_replies_reuse_identity_and_retire_once() {
    let mut store = ProductSettingsStore::new("retry", ProductSettings::default());
    let mut configuration = Configuration {
        management_active: true,
        ..Default::default()
    };
    configuration.load(&mut store);
    configuration.submissions.clear();
    configuration.lose_admission = true;
    configuration.lose_release = true;
    let desired = ProductSettings {
        show_remote_monitor: false,
        ..ProductSettings::default()
    };
    store.begin_update(88, 0, desired).unwrap();
    assert_eq!(configuration.finish_update(&mut store)["ok"], true);
    assert_eq!(configuration.submissions.len(), 2);
    assert_eq!(configuration.submissions[0], configuration.submissions[1]);
    assert_eq!(
        configuration.document.as_ref().unwrap().1,
        1,
        "retry must not apply twice"
    );
    assert!(configuration.calls.is_empty());
}

#[test]
fn ended_parent_does_not_submit_new_mutation_and_unknown_admission_is_recovered() {
    let mut store = ProductSettingsStore::new("cancel", ProductSettings::default());
    let mut configuration = Configuration {
        management_active: true,
        ..Default::default()
    };
    configuration.load(&mut store);
    configuration.submissions.clear();
    store
        .begin_update(88, 0, ProductSettings::default())
        .unwrap();
    configuration.management_active = false;
    assert_eq!(
        configuration.finish_update(&mut store)["code"],
        "request_ended"
    );
    assert!(configuration.submissions.is_empty());
    configuration.load(&mut store);

    configuration.management_active = true;
    configuration.lose_admission = true;
    store
        .begin_update(88, 0, ProductSettings::default())
        .unwrap();
    store
        .advance(&mut |command| configuration.send(command))
        .unwrap();
    assert_eq!(configuration.calls.len(), 1);
    configuration.management_active = false;
    assert_eq!(
        configuration.finish_update(&mut store)["code"],
        "request_ended"
    );
    assert!(configuration.calls.is_empty());
    assert!(configuration.document.is_none());
}

#[test]
fn cancellation_after_commit_reloads_durable_values_without_claiming_success() {
    let mut store = ProductSettingsStore::new("late-cancel", ProductSettings::default());
    let mut configuration = Configuration {
        management_active: true,
        ..Default::default()
    };
    configuration.load(&mut store);
    let desired = ProductSettings {
        max_clients: 2,
        ..ProductSettings::default()
    };
    store.begin_update(88, 0, desired.clone()).unwrap();
    store
        .advance(&mut |command| configuration.send(command))
        .unwrap();
    store
        .advance(&mut |command| configuration.send(command))
        .unwrap();
    assert_eq!(configuration.document, Some((desired.clone(), 1)));
    configuration.management_active = false;
    assert_eq!(
        configuration.finish_update(&mut store)["code"],
        "request_ended"
    );
    assert!(!store.is_ready());
    configuration.load(&mut store);
    assert_eq!(store.settings(), (&desired, 1));
}

#[test]
fn concurrent_configuration_change_returns_conflict_then_refreshes_the_revision() {
    let mut store = ProductSettingsStore::new("conflict", ProductSettings::default());
    let mut configuration = Configuration {
        management_active: true,
        ..Default::default()
    };
    configuration.load(&mut store);
    store
        .begin_update(88, 0, ProductSettings::default())
        .unwrap();
    let persisted = ProductSettings {
        max_clients: 3,
        ..ProductSettings::default()
    };
    configuration.document = Some((persisted.clone(), 1));
    let response = configuration.finish_update(&mut store);
    assert_eq!(response["code"], "revision_conflict");
    assert_eq!(response["current_revision"], 1);
    assert!(!store.is_ready());
    configuration.load(&mut store);
    assert_eq!(store.settings(), (&persisted, 1));
}

#[test]
fn malformed_documents_never_turn_into_ready_or_successful_settings() {
    let defaults = ProductSettings::default();
    let valid = json!({"ok":true,"found":true,"version":1,"revision":7,"values":defaults});
    assert_eq!(
        parse_document(&valid, &defaults),
        Some((defaults.clone(), 7))
    );
    for (field, value) in [
        ("version", json!(2)),
        ("revision", json!(0)),
        ("found", json!(false)),
        ("ok", json!(false)),
        ("values", json!({})),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        assert!(
            parse_document(&invalid, &defaults).is_none(),
            "accepted invalid {field}"
        );
    }
    let mut invalid = valid.clone();
    invalid["values"]["allowed_principals"] = json!(["attacker"]);
    assert!(parse_document(&invalid, &defaults).is_none());
    let mut invalid = valid;
    invalid["values"]["max_clients"] = json!(33);
    assert!(parse_document(&invalid, &defaults).is_none());
}

#[test]
fn repeated_updates_bound_broker_results_and_unconsumed_management_completions() {
    let mut store = ProductSettingsStore::new("bounded", ProductSettings::default());
    let mut configuration = Configuration {
        management_active: true,
        ..Default::default()
    };
    configuration.load(&mut store);
    for revision in 0..600 {
        let desired = ProductSettings {
            max_clients: 1 + (revision as usize % 32),
            ..ProductSettings::default()
        };
        store.begin_update(88, revision, desired.clone()).unwrap();
        for _ in 0..3 {
            store
                .advance(&mut |command| configuration.send(command))
                .unwrap();
            assert!(configuration.calls.len() <= 1);
        }
        assert_eq!(
            store.begin_update(88, revision + 1, desired).unwrap_err()["code"],
            "settings_busy"
        );
        assert_eq!(store.take_completion().unwrap().1["ok"], true);
        assert!(configuration.calls.is_empty());
    }
    assert_eq!(store.settings().1, 600);
}

#[test]
fn shutdown_recovers_unknown_admission_and_retires_it_before_reporting_quiescence() {
    let mut store = ProductSettingsStore::new("shutdown", ProductSettings::default());
    let mut configuration = Configuration {
        lose_admission: true,
        ..Default::default()
    };
    store
        .advance(&mut |command| configuration.send(command))
        .unwrap();
    assert_eq!(configuration.calls.len(), 1);
    let mut settled = false;
    for _ in 0..8 {
        if store
            .quiesce(&mut |command| configuration.send(command))
            .unwrap()
        {
            settled = true;
            break;
        }
    }
    assert!(settled);
    assert!(!store.is_ready());
    assert!(store.take_completion().is_none());
    assert!(configuration.calls.is_empty());
    assert_eq!(configuration.submissions.len(), 2);
    assert_eq!(configuration.submissions[0], configuration.submissions[1]);
}
