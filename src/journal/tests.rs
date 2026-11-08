use super::*;
use amaterasu_state::test_support::TestStateStore;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
const PRINCIPAL: &str = "amaterasu.remote-session";

struct Call {
    operation_id: String,
    payload: Value,
    result: Option<Value>,
    cancelled: bool,
}

struct Backend {
    directory: PathBuf,
    store: Option<TestStateStore>,
    calls: BTreeMap<u64, Call>,
    next: u64,
    lose_admission: bool,
    lose_result: bool,
    lose_release: bool,
    submissions: Vec<(String, Value)>,
}

impl Backend {
    fn new() -> Self {
        let root = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let directory = root.join(format!(
            "owner-journal-test-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let store = Some(TestStateStore::open(directory.clone()).unwrap());
        Self {
            directory,
            store,
            calls: BTreeMap::new(),
            next: 0,
            lose_admission: false,
            lose_result: false,
            lose_release: false,
            submissions: Vec::new(),
        }
    }

    fn state(&self, payload: Value) -> Value {
        self.store.as_ref().unwrap().handle(PRINCIPAL, payload)
    }

    fn reopen(&mut self) {
        self.store.take();
        self.store = Some(TestStateStore::open(self.directory.clone()).unwrap());
    }

    fn send(&mut self, command: Command) -> ModuleResult<Response> {
        match command {
            Command::Request {
                service,
                payload,
                timeout_ms,
                operation_id,
                parent,
            } => {
                assert_eq!(service, STATE);
                assert_eq!(timeout_ms, TIMEOUT_MS);
                assert_eq!(parent, None);
                assert!(serde_json::to_vec(&payload).unwrap().len() <= MAX_PAYLOAD_BYTES);
                self.submissions
                    .push((operation_id.clone(), payload.clone()));
                if let Some((request, call)) = self
                    .calls
                    .iter()
                    .find(|(_, call)| call.operation_id == operation_id)
                {
                    assert_eq!(call.payload, payload);
                    return Ok(Response::Accepted { id: *request });
                }
                self.next += 1;
                self.calls.insert(
                    self.next,
                    Call {
                        operation_id,
                        payload,
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
                let Some(call) = self.calls.get(&request) else {
                    return Ok(Response::error("not_found", "retired"));
                };
                if call.cancelled {
                    return Ok(Response::error("cancelled", "cancelled"));
                }
                if call.result.is_none() {
                    let payload = call.payload.clone();
                    let response = self.state(payload);
                    self.calls.get_mut(&request).unwrap().result = Some(response);
                    if self.lose_result {
                        self.lose_result = false;
                        return Err("result reply lost".into());
                    }
                }
                Ok(Response::Result {
                    payload: self.calls[&request].result.clone().unwrap(),
                })
            }
            Command::LookupRequest {
                service,
                payload,
                operation_id,
                parent,
            } => {
                assert_eq!(service, STATE);
                assert_eq!(parent, None);
                Ok(self
                    .calls
                    .iter()
                    .find_map(|(id, call)| {
                        (call.operation_id == operation_id && call.payload == payload)
                            .then_some(Response::Accepted { id: *id })
                    })
                    .unwrap_or_else(|| Response::error("not_found", "not admitted")))
            }
            Command::ReleaseResult { request } => {
                let Some(call) = self.calls.get(&request) else {
                    return Ok(Response::error("not_found", "retired"));
                };
                assert!(call.cancelled || call.result.is_some());
                self.calls.remove(&request);
                if self.lose_release {
                    self.lose_release = false;
                    Err("release reply lost".into())
                } else {
                    Ok(Response::Ok)
                }
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
            unexpected => panic!("unexpected journal command {unexpected:?}"),
        }
    }

    fn load(&mut self, journal: &mut OwnerJournal) {
        for _ in 0..12 {
            if journal.is_ready() {
                break;
            }
            journal.advance(&mut |command| self.send(command)).unwrap();
        }
        assert!(journal.is_ready(), "journal did not load");
        assert!(self.calls.is_empty());
    }

    fn finish_store(&mut self, journal: &mut OwnerJournal) -> Result<(), String> {
        for _ in 0..20 {
            journal.advance(&mut |command| self.send(command)).unwrap();
            assert!(self.calls.len() <= 1);
            if let Some(result) = journal.take_completion() {
                return result;
            }
        }
        panic!("journal store did not settle")
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.store.take();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn owner(client: usize) -> Value {
    json!({"client_id":format!("client-{client}"),"session_id":format!("session-{client}"),"generation":client as u64+1,
        "role":"monitor","mode":{"width":1920,"height":1080,"refresh_millihz":60000,"hdr":false},
        "profile_id":"builtin-monitor","profile_settings":{},"release_requested":false,
        "reservations":[{"id":format!("reservation-{client}"),"transport_id":null,"runtime_epoch":null}]})
}

fn document(count: usize) -> Value {
    json!({"version":1,"next_generation":count as u64+1,"owners":(0..count).map(owner).collect::<Vec<_>>()})
}

#[test]
fn state_backed_journal_requires_commit_readback_and_result_retirement() {
    let mut backend = Backend::new();
    let mut journal = OwnerJournal::new("persist");
    backend.load(&mut journal);
    assert_eq!(journal.snapshot(), &empty_document());
    let desired = document(1);
    journal.begin_store(desired.clone()).unwrap();
    assert!(!journal.is_ready());
    for _ in 0..3 {
        journal
            .advance(&mut |command| backend.send(command))
            .unwrap();
    }
    assert_eq!(
        backend.state(json!({"action":"get","key":KEY}))["value"],
        desired
    );
    assert_eq!(
        journal.snapshot(),
        &empty_document(),
        "set acknowledgement alone cannot authorize effects"
    );
    assert!(journal.take_completion().is_none());
    assert!(!journal.is_ready());
    backend.finish_store(&mut journal).unwrap();
    assert_eq!(journal.snapshot(), &desired);
    assert!(journal.is_ready());
    assert!(!journal.is_busy());
    assert!(backend.calls.is_empty());
}

#[test]
fn restart_retains_every_owner_and_unfinished_transport_reservation() {
    let mut backend = Backend::new();
    let mut journal = OwnerJournal::new("before-restart");
    backend.load(&mut journal);
    let mut desired = document(2);
    desired["owners"][0]["reservations"][0]["transport_id"] = json!("rtsp-operation-1");
    desired["owners"][0]["reservations"][0]["runtime_epoch"] = json!("boot-epoch");
    desired["owners"][1]["release_requested"] = json!(true);
    journal.begin_store(desired.clone()).unwrap();
    backend.finish_store(&mut journal).unwrap();
    drop(journal);
    backend.reopen();
    let mut restarted = OwnerJournal::new("after-restart");
    backend.load(&mut restarted);
    assert_eq!(restarted.snapshot(), &desired);
    assert!(
        restarted.take_completion().is_none(),
        "startup must not invent cleanup completion"
    );
}

#[test]
fn committed_intent_survives_crash_before_its_completion_can_authorize_effects() {
    let mut backend = Backend::new();
    let mut journal = OwnerJournal::new("crash");
    backend.load(&mut journal);
    let desired = document(1);
    journal.begin_store(desired.clone()).unwrap();
    journal
        .advance(&mut |command| backend.send(command))
        .unwrap();
    journal
        .advance(&mut |command| backend.send(command))
        .unwrap();
    assert!(journal.take_completion().is_none());
    assert!(!journal.is_ready());
    backend.calls.clear(); // A new runtime has no old process-local broker ledger.
    backend.reopen();
    let mut restarted = OwnerJournal::new("recover-crash");
    backend.load(&mut restarted);
    assert_eq!(restarted.snapshot(), &desired);
}

#[test]
fn lost_admission_result_and_release_replies_never_duplicate_a_state_mutation() {
    let mut backend = Backend::new();
    let mut journal = OwnerJournal::new("retry");
    backend.load(&mut journal);
    backend.submissions.clear();
    backend.lose_admission = true;
    backend.lose_result = true;
    backend.lose_release = true;
    let desired = document(1);
    journal.begin_store(desired.clone()).unwrap();
    backend.finish_store(&mut journal).unwrap();
    let stored = backend.state(json!({"action":"get","key":KEY}));
    assert_eq!(stored["revision"], 1);
    assert_eq!(stored["value"], desired);
    assert_eq!(
        backend
            .submissions
            .iter()
            .filter(|(_, payload)| payload["action"] == "set")
            .count(),
        1
    );
    assert!(backend.calls.is_empty());
}

#[test]
fn stale_cas_cannot_overwrite_retained_owners_and_refreshes_the_committed_snapshot() {
    let mut backend = Backend::new();
    let mut journal = OwnerJournal::new("cas");
    backend.load(&mut journal);
    journal.begin_store(document(1)).unwrap();
    backend.finish_store(&mut journal).unwrap();
    journal.begin_store(document(2)).unwrap();
    let concurrent = document(3);
    assert_eq!(
        backend.state(json!({"action":"set","key":KEY,"value":concurrent,"expected_revision":1}))["ok"],
        true
    );
    assert!(
        backend
            .finish_store(&mut journal)
            .unwrap_err()
            .starts_with("revision_conflict")
    );
    assert!(!journal.is_ready());
    backend.load(&mut journal);
    assert_eq!(journal.snapshot(), &concurrent);
}

#[test]
fn changed_document_during_readback_cannot_report_success_for_the_requested_intent() {
    let mut backend = Backend::new();
    let mut journal = OwnerJournal::new("verify");
    backend.load(&mut journal);
    journal.begin_store(document(1)).unwrap();
    for _ in 0..3 {
        journal
            .advance(&mut |command| backend.send(command))
            .unwrap();
    }
    let concurrent = document(2);
    assert_eq!(
        backend.state(json!({"action":"set","key":KEY,"value":concurrent,"expected_revision":1}))["ok"],
        true
    );
    assert_eq!(
        backend.finish_store(&mut journal).unwrap_err(),
        "owner_journal_readback_mismatch"
    );
    backend.load(&mut journal);
    assert_eq!(journal.snapshot(), &concurrent);
}

#[test]
fn capacity_and_structural_rejections_happen_before_admission_or_state_changes() {
    let mut backend = Backend::new();
    let mut journal = OwnerJournal::new("capacity");
    backend.load(&mut journal);
    backend.submissions.clear();
    validate_document(&document(32)).unwrap();
    let mut oversized = document(32);
    for owner in oversized["owners"].as_array_mut().unwrap() {
        owner["profile_settings"] = json!({"long":"x".repeat(1000)});
    }
    assert_eq!(
        journal.begin_store(oversized).unwrap_err(),
        "owner_journal_capacity_exhausted"
    );
    assert_eq!(
        journal.begin_store(document(33)).unwrap_err(),
        "invalid_owner_journal"
    );
    assert!(journal.is_ready());
    assert!(!journal.is_busy());
    assert!(backend.submissions.is_empty());
    assert_eq!(
        backend.state(json!({"action":"get","key":KEY}))["found"],
        false
    );
    let mut duplicate = document(2);
    duplicate["owners"][1] = duplicate["owners"][0].clone();
    assert_eq!(
        validate_document(&duplicate).unwrap_err(),
        "duplicate_owner_journal_owner"
    );
    let mut invalid = document(1);
    invalid["next_generation"] = json!(1);
    assert_eq!(
        validate_document(&invalid).unwrap_err(),
        "invalid_owner_journal_generation"
    );
}

#[test]
fn generations_cannot_regress_and_missing_previously_written_journal_cannot_reset_them() {
    let mut backend = Backend::new();
    let mut journal = OwnerJournal::new("generation");
    backend.load(&mut journal);
    journal.begin_store(document(1)).unwrap();
    backend.finish_store(&mut journal).unwrap();
    assert_eq!(
        journal.begin_store(empty_document()).unwrap_err(),
        "owner_journal_generation_regressed"
    );
    assert_eq!(
        backend.state(json!({"action":"delete","key":KEY,"expected_revision":1}))["ok"],
        true
    );
    let mut restarted = OwnerJournal::new("missing");
    for _ in 0..6 {
        restarted
            .advance(&mut |command| backend.send(command))
            .unwrap();
    }
    assert!(!restarted.is_ready());
    assert_eq!(
        restarted.last_error(),
        Some("owner_journal_missing_after_prior_state")
    );
}

#[test]
fn quiesce_recovers_unknown_call_ids_and_does_not_report_retirement_early() {
    let mut backend = Backend::new();
    backend.lose_admission = true;
    let mut journal = OwnerJournal::new("shutdown");
    journal
        .advance(&mut |command| backend.send(command))
        .unwrap();
    assert_eq!(backend.calls.len(), 1);
    assert!(
        !journal
            .quiesce(&mut |command| backend.send(command))
            .unwrap()
    );
    let mut settled = false;
    for _ in 0..8 {
        if journal
            .quiesce(&mut |command| backend.send(command))
            .unwrap()
        {
            settled = true;
            break;
        }
    }
    assert!(settled);
    assert!(backend.calls.is_empty());
    assert!(!journal.is_ready());
    assert!(journal.take_completion().is_none());
    assert_eq!(backend.submissions.len(), 1);
}

#[test]
fn shutdown_does_not_replay_or_cancel_an_uncertain_journal_write() {
    let mut backend = Backend::new();
    let mut journal = OwnerJournal::new("shutdown-write");
    backend.load(&mut journal);
    backend.submissions.clear();
    backend.lose_admission = true;
    let desired = document(1);
    journal.begin_store(desired.clone()).unwrap();
    journal
        .advance(&mut |command| backend.send(command))
        .unwrap();
    let mut commands = Vec::new();
    for _ in 0..10 {
        if journal
            .quiesce(&mut |command| {
                commands.push(command.clone());
                backend.send(command)
            })
            .unwrap()
        {
            break;
        }
    }
    assert!(journal.pending.is_none());
    assert!(journal.take_completion().is_none());
    assert!(!journal.is_ready());
    assert_eq!(
        backend.state(json!({"action":"get","key":KEY}))["value"],
        desired
    );
    assert_eq!(backend.submissions.len(), 1);
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, Command::LookupRequest { .. }))
    );
    assert!(
        commands
            .iter()
            .all(|command| !matches!(command, Command::Request { .. } | Command::Cancel { .. }))
    );
}

#[test]
fn shutdown_fences_a_write_when_broker_timeout_does_not_prove_completion() {
    let mut backend = Backend::new();
    let mut journal = OwnerJournal::new("timeout-write");
    backend.load(&mut journal);
    journal.begin_store(document(1)).unwrap();
    journal
        .advance(&mut |command| backend.send(command))
        .unwrap();
    for _ in 0..3 {
        assert!(
            !journal
                .quiesce(&mut |command| {
                    assert!(matches!(command, Command::TakeResult { .. }));
                    Ok(Response::error(
                        "deadline_exceeded",
                        "provider outcome unconfirmed",
                    ))
                })
                .unwrap()
        );
    }
    assert!(journal.pending.is_some());
    assert!(journal.take_completion().is_none());
}

#[test]
fn normal_broker_timeout_does_not_reload_over_an_unconfirmed_write() {
    let mut backend = Backend::new();
    let mut journal = OwnerJournal::new("normal-timeout-write");
    backend.load(&mut journal);
    journal.begin_store(document(1)).unwrap();
    journal
        .advance(&mut |command| backend.send(command))
        .unwrap();
    for _ in 0..3 {
        journal
            .advance(&mut |command| {
                assert!(matches!(command, Command::TakeResult { .. }));
                Ok(Response::error(
                    "deadline_exceeded",
                    "provider outcome unconfirmed",
                ))
            })
            .unwrap();
    }
    assert!(!journal.is_ready());
    assert_eq!(journal.snapshot(), &empty_document());
    assert!(journal.pending.is_some());
    assert!(journal.take_completion().is_none());
}

#[test]
fn lookup_absence_only_retires_within_the_broker_retention_window() {
    for (age, expected) in [(Duration::from_secs(1), true), (LOOKUP_RETENTION, false)] {
        let mut journal = OwnerJournal::new("lookup-window");
        let pending = journal.pending.as_mut().unwrap();
        pending.state = CallState::Submit { attempted: true };
        pending.first_sent = Some(Instant::now() - age);
        assert_eq!(
            journal
                .quiesce(&mut |command| {
                    assert!(matches!(command, Command::LookupRequest { .. }));
                    Ok(Response::error("not_found", "not admitted"))
                })
                .unwrap(),
            expected
        );
    }
}
