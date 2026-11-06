use super::*;
use serde_json::json;
use std::collections::BTreeMap;

fn proves_retired(candidate: &RecoveryCandidate, payload: &Value) -> bool {
    let Ok((contract, _)) = operation_ref(candidate) else {
        return false;
    };
    proves_contract_retired(candidate, contract, payload)
}

fn candidate(kind: ReservationKind) -> RecoveryCandidate {
    let epoch = "ab".repeat(16);
    RecoveryCandidate {
        client_id: "cd".repeat(32),
        borrower_id: "cd".repeat(32),
        consumer: "wire".into(),
        session_id: "session".into(),
        generation: 7,
        reservation_id: "reservation".into(),
        transport_id: "transport-operation".into(),
        runtime_epoch: epoch.clone(),
        kind,
        application_operation_id: Some(format!("ale1:{epoch}:application")),
        application_epoch: Some(epoch),
        application_generation: Some(4),
    }
}

fn transport_status(candidate: &RecoveryCandidate) -> Value {
    json!({"ok":true,"operation_id":candidate.transport_id,"expected_runtime_epoch":candidate.runtime_epoch,"runtime_epoch":candidate.runtime_epoch,
        "startup_reconciled":true,"state":"retired","retired":true,"active_operations":0,"pending_admissions":0,"cleanup_pending":false})
}

#[test]
fn transport_proof_requires_the_exact_tuple_and_authoritative_native_reconciliation() {
    let candidate = candidate(ReservationKind::Auxiliary);
    let valid = transport_status(&candidate);
    assert!(proves_retired(&candidate, &valid));
    for (field, value) in [
        ("operation_id", json!("other")),
        ("expected_runtime_epoch", json!("ef".repeat(16))),
        ("startup_reconciled", json!(false)),
        ("retired", json!(false)),
        ("state", json!("unknown")),
        ("operation_id", Value::Null),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        assert!(
            !proves_retired(&candidate, &invalid),
            "accepted invalid {field}"
        );
    }
    let mut fenced = valid.clone();
    fenced["runtime_epoch"] = json!("ef".repeat(16));
    fenced["state"] = json!("epoch_fenced");
    assert!(proves_retired(&candidate, &fenced));
    fenced["cleanup_pending"] = json!(true);
    assert!(!proves_retired(&candidate, &fenced));
}

#[test]
fn application_source_uses_native_application_proof_independently_of_its_transport() {
    let candidate = candidate(ReservationKind::Application);
    assert!(!proves_retired(&candidate, &transport_status(&candidate)));
    let valid = json!({"ok":true,"operation_id":candidate.application_operation_id,"expected_runtime_epoch":candidate.application_epoch,
        "runtime_epoch":candidate.application_epoch,"startup_reconciled":false,"state":"retired","retired":true});
    assert!(proves_retired(&candidate, &valid));
    let mut wrong_generation = valid.clone();
    wrong_generation["application_generation"] = json!(5);
    assert!(!proves_retired(&candidate, &wrong_generation));
    wrong_generation["application_generation"] = json!(4);
    assert!(proves_retired(&candidate, &wrong_generation));
    let mut restarted = valid;
    restarted["runtime_epoch"] = json!("ef".repeat(16));
    restarted["state"] = json!("epoch_fenced");
    assert!(
        !proves_retired(&candidate, &restarted),
        "Launch has no cross-epoch native recovery proof"
    );
}

#[derive(Default)]
struct Transport {
    requests: BTreeMap<u64, (Command, bool)>,
    next: u64,
    submissions: Vec<Value>,
    payload: Value,
    application_payload: Option<Value>,
    lose_admission: bool,
    lose_release: bool,
}

impl Transport {
    fn send(&mut self, command: Command) -> ModuleResult<Response> {
        match command {
            request @ Command::Request { .. } => {
                let value = serde_json::to_value(&request).unwrap();
                self.submissions.push(value.clone());
                if let Some((id, _)) = self
                    .requests
                    .iter()
                    .find(|(_, entry)| serde_json::to_value(&entry.0).unwrap() == value)
                {
                    return Ok(Response::Accepted { id: *id });
                }
                self.next += 1;
                self.requests.insert(self.next, (request, false));
                if self.lose_admission {
                    self.lose_admission = false;
                    Err("lost admission".into())
                } else {
                    Ok(Response::Accepted { id: self.next })
                }
            }
            Command::TakeResult { request } => match self.requests.get(&request) {
                None => Ok(Response::error("not_found", "retired")),
                Some((_, true)) => Ok(Response::error("cancelled", "cancelled")),
                Some((Command::Request { payload, .. }, _)) => Ok(Response::Result {
                    payload: if payload["action"] == "application_retirement_status" {
                        self.application_payload
                            .clone()
                            .unwrap_or_else(|| self.payload.clone())
                    } else {
                        self.payload.clone()
                    },
                }),
                Some(_) => panic!("request command"),
            },
            Command::Cancel { request } => {
                if let Some(entry) = self.requests.get_mut(&request) {
                    entry.1 = true;
                    Ok(Response::Ok)
                } else {
                    Ok(Response::error("not_found", "retired"))
                }
            }
            Command::ReleaseResult { request } => {
                if self.requests.remove(&request).is_none() {
                    return Ok(Response::error("not_found", "retired"));
                }
                if self.lose_release {
                    self.lose_release = false;
                    Err("lost release".into())
                } else {
                    Ok(Response::Ok)
                }
            }
            unexpected => panic!("observer attempted unsupported command {unexpected:?}"),
        }
    }
}

#[test]
fn observer_releases_results_before_reporting_retirement_and_replays_unknown_admissions() {
    let now = Instant::now();
    let candidate = candidate(ReservationKind::Auxiliary);
    let mut transport = Transport {
        payload: transport_status(&candidate),
        lose_admission: true,
        lose_release: true,
        ..Default::default()
    };
    let mut recovery = Recovery::new("retry", now);
    for _ in 0..8 {
        recovery
            .advance(now, std::slice::from_ref(&candidate), &mut |command| {
                transport.send(command)
            })
            .unwrap();
    }
    assert_eq!(recovery.take_retired(), Some(candidate));
    assert!(transport.requests.is_empty());
    assert_eq!(transport.submissions.len(), 2);
    assert_eq!(transport.submissions[0], transport.submissions[1]);
    assert_eq!(
        transport.submissions[0]["payload"]["action"],
        "retirement_status"
    );
}

#[test]
fn shutdown_recovers_and_cancels_a_lost_observation_without_calling_it_retired() {
    let now = Instant::now();
    let candidate = candidate(ReservationKind::Auxiliary);
    let mut transport = Transport {
        payload: transport_status(&candidate),
        lose_admission: true,
        ..Default::default()
    };
    let mut recovery = Recovery::new("shutdown", now);
    recovery
        .advance(now, &[candidate], &mut |command| transport.send(command))
        .unwrap();
    let mut retired = false;
    for _ in 0..8 {
        if recovery
            .quiesce(now, &mut |command| transport.send(command))
            .unwrap()
        {
            retired = true;
            break;
        }
    }
    assert!(retired);
    assert!(transport.requests.is_empty());
    assert!(recovery.take_retired().is_none());
    assert_eq!(transport.submissions[0], transport.submissions[1]);
}

#[test]
fn application_recovery_cannot_substitute_a_transport_query_or_missing_epoch() {
    let mut candidate = candidate(ReservationKind::Application);
    let (contract, reference) = operation_ref(&candidate).unwrap();
    assert_eq!(contract, Contract::Application);
    assert_eq!(
        reference.epoch,
        candidate.application_epoch.clone().unwrap()
    );
    candidate.application_epoch = None;
    assert!(operation_ref(&candidate).is_err());
}

#[test]
fn membership_keeps_unknown_authority_fenced_and_records_authoritative_revocation() {
    let now = Instant::now();
    let id = "ab".repeat(32);
    let mut membership = Membership::new("authority", now);
    let mut transport = Transport {
        payload: json!({"ok":true,"certificate_id":"cd".repeat(32),"paired":true,"may_view":true,"may_launch":true,"may_terminate":true,"revision":1,"name":"Wrong client"}),
        ..Default::default()
    };
    for _ in 0..3 {
        membership
            .advance(now, std::slice::from_ref(&id), &mut |command| {
                transport.send(command)
            })
            .unwrap();
    }
    assert!(!membership.ready_for(std::slice::from_ref(&id)));
    assert!(membership.source_authorizations().is_empty());
    transport.payload = json!({"ok":true,"certificate_id":id,"paired":false,"may_view":false,"may_launch":false,"may_terminate":false,"revision":2,"name":null});
    let next = now + Duration::from_secs(1);
    for _ in 0..3 {
        membership
            .advance(next, std::slice::from_ref(&id), &mut |command| {
                transport.send(command)
            })
            .unwrap();
    }
    assert!(membership.ready_for(std::slice::from_ref(&id)));
    assert_eq!(membership.source_authorizations().get(&id), Some(&false));
    assert!(!membership.take_observation().unwrap().paired);
    assert!(transport.requests.is_empty());
}

#[test]
fn application_owner_requires_both_application_and_original_transport_retirement() {
    let now = Instant::now();
    let candidate = candidate(ReservationKind::Application);
    let app = json!({"ok":true,"operation_id":candidate.application_operation_id,"expected_runtime_epoch":candidate.application_epoch,
        "runtime_epoch":candidate.application_epoch,"startup_reconciled":false,"state":"retired","retired":true});
    let mut active = transport_status(&candidate);
    active["state"] = json!("active");
    active["retired"] = json!(false);
    active["active_operations"] = json!(1);
    let mut transport = Transport {
        payload: active,
        application_payload: Some(app),
        ..Default::default()
    };
    let mut recovery = Recovery::new("conjunction", now);
    for _ in 0..7 {
        recovery
            .advance(now, std::slice::from_ref(&candidate), &mut |command| {
                transport.send(command)
            })
            .unwrap();
    }
    assert!(recovery.take_retired().is_none());
    assert!(transport.requests.is_empty());
    assert_eq!(
        transport.submissions[0]["service"],
        Contract::Application.service()
    );
    assert_eq!(
        transport.submissions[1]["service"],
        Contract::Transport.service()
    );
    transport.payload = transport_status(&candidate);
    let next = now + Duration::from_secs(1);
    for _ in 0..7 {
        recovery
            .advance(next, std::slice::from_ref(&candidate), &mut |command| {
                transport.send(command)
            })
            .unwrap();
    }
    assert_eq!(recovery.take_retired(), Some(candidate));
    assert!(transport.requests.is_empty());
}

#[test]
fn failed_live_authority_refresh_blocks_new_effects_without_inventing_revocation() {
    let now = Instant::now();
    let id = "ab".repeat(32);
    let mut membership = Membership::new("refresh", now);
    let mut transport = Transport {
        payload: json!({"ok":true,"certificate_id":id,"paired":true,"may_view":true,"may_launch":true,"may_terminate":true,"revision":1,"name":"Player"}),
        ..Default::default()
    };
    for _ in 0..3 {
        membership
            .advance(now, std::slice::from_ref(&id), &mut |command| {
                transport.send(command)
            })
            .unwrap();
    }
    assert!(membership.ready_for(std::slice::from_ref(&id)));
    assert!(membership.take_observation().unwrap().paired);
    transport.payload = json!({"ok":false,"error":{"code":"unavailable"}});
    let later = now + Duration::from_secs(1);
    for _ in 0..3 {
        membership
            .advance(later, std::slice::from_ref(&id), &mut |command| {
                transport.send(command)
            })
            .unwrap();
    }
    assert!(!membership.ready_for(std::slice::from_ref(&id)));
    assert!(membership.source_authorizations().is_empty());
    assert!(
        membership.take_observation().is_none(),
        "an unavailable authority is not evidence of unpairing"
    );
}
