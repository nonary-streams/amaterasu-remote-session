use super::*;
use amaterasu_module_sdk::session_presentation::{Client, Game};

mod admission;

#[derive(Default)]
struct TestMonitors {
    states: BTreeMap<String, MonitorStatus>,
    holds: BTreeSet<(String, u64, String)>,
    begins: usize,
    releases: usize,
    fail_next_begin: bool,
}

impl Monitors for TestMonitors {
    fn begin(&mut self, owner: &RetainedOwner, _now: Instant) -> Result<(), String> {
        if self.fail_next_begin {
            self.fail_next_begin = false;
            return Err("temporary_preflight_error".into());
        }
        if let Some(previous) = self.states.get(&owner.client_id) {
            if previous.generation == owner.generation {
                return Ok(());
            }
            if !previous.source_reference_released {
                return Err("previous_generation_active".into());
            }
        }
        self.begins += 1;
        self.states.insert(
            owner.client_id.clone(),
            MonitorStatus {
                generation: owner.generation,
                ready: true,
                source_ready: true,
                source_mode: Some(owner.mode),
                source_route: "managed_output".into(),
                output: Some(format!("output-{}", owner.generation)),
                cleanup_pending: false,
                reference_released: false,
                source_reference_released: false,
                error: None,
                retryable: true,
            },
        );
        Ok(())
    }
    fn snapshot(&self, client: &str) -> Option<MonitorStatus> {
        self.states.get(client).cloned()
    }
    fn release(&mut self, client: &str, generation: u64, _now: Instant) -> Result<(), String> {
        let state = self.states.get_mut(client).ok_or("missing")?;
        if state.generation != generation {
            return Err("generation_changed".into());
        }
        self.releases += 1;
        state.ready = false;
        state.reference_released = true;
        state.cleanup_pending = false;
        if !self
            .holds
            .iter()
            .any(|(id, owner_generation, _)| id == client && *owner_generation == generation)
        {
            state.source_ready = false;
            state.source_reference_released = true;
        }
        Ok(())
    }
    fn capture_started(
        &mut self,
        client: &str,
        generation: u64,
        reservation: &str,
    ) -> Result<(), String> {
        self.holds
            .insert((client.into(), generation, reservation.into()));
        Ok(())
    }
    fn capture_stopped(&mut self, client: &str, generation: u64, reservation: &str, _now: Instant) {
        self.holds
            .remove(&(client.into(), generation, reservation.into()));
        if let Some(state) = self.states.get_mut(client)
            && state.reference_released
            && !self
                .holds
                .iter()
                .any(|(id, owner_generation, _)| id == client && *owner_generation == generation)
        {
            state.source_ready = false;
            state.source_reference_released = true;
        }
    }
}

fn app(id: &str, name: &str) -> Application {
    Application {
        id: id.into(),
        uuid: format!("game-{id}"),
        title: name.into(),
        aliases: vec![],
        visible: true,
        monitor_profile: false,
        properties: json!({}),
    }
}
fn context(client: &str, game: bool) -> Context {
    Context {
        client: Client {
            id: client.into(),
            paired: true,
            may_list: true,
            may_view: true,
            may_launch: true,
            may_terminate: true,
        },
        game: game.then(|| Game {
            application: app("42", "Running game"),
            owner_client_id: "owner".into(),
            generation: 7,
            source: None,
        }),
    }
}
fn mode() -> Mode {
    Mode {
        width: 1920,
        height: 1080,
        refresh_millihz: 60_000,
        hdr: false,
    }
}
fn dispatch(context: Context, operation: &str, selection: Selection) -> Request {
    Request::Dispatch {
        context,
        operation_id: operation.into(),
        intent: DispatchIntent::Launch,
        selection: Some(selection),
        mode: Some(mode()),
    }
}
fn control(control: Control) -> Selection {
    Selection {
        id: control.id(false),
        uuid: None,
    }
}
fn call(
    policy: &mut SessionPolicy,
    monitors: &mut TestMonitors,
    request: Request,
    now: Instant,
) -> Reply {
    policy.handle("adapter", request, now, monitors)
}
fn persist(policy: &mut SessionPolicy, monitors: &mut TestMonitors, now: Instant) {
    for _ in 0..8 {
        if policy.needs_persistence() {
            let (revision, value) = policy.desired_document();
            policy.accept_durable(revision, value).unwrap();
        }
        policy.synchronize(now, monitors);
        if !policy.needs_persistence() {
            return;
        }
    }
    panic!("policy did not settle durable state");
}
fn inventory(
    policy: &mut SessionPolicy,
    monitors: &mut TestMonitors,
    context: Context,
    entries: Vec<Application>,
    now: Instant,
) -> String {
    let client = context.client.id.clone();
    let Reply::Started { projection_id } = call(
        policy,
        monitors,
        Request::BeginProject {
            context,
            catalog_revision: "7".into(),
        },
        now,
    ) else {
        panic!("begin projection")
    };
    assert!(matches!(
        call(
            policy,
            monitors,
            Request::AppendProject {
                client_id: client,
                projection_id: projection_id.clone(),
                entries,
                final_page: true
            },
            now
        ),
        Reply::Appended { complete: true }
    ));
    projection_id
}
fn auxiliary(
    policy: &mut SessionPolicy,
    monitors: &mut TestMonitors,
    client: &str,
    operation: &str,
    kind: Control,
    now: Instant,
) -> AuxiliaryStreamPlan {
    let request = dispatch(context(client, false), operation, control(kind));
    let first = call(policy, monitors, request.clone(), now);
    assert!(matches!(first, Reply::Pending { .. }));
    persist(policy, monitors, now);
    let Reply::Dispatched {
        decision: Decision::Auxiliary { plan },
    } = call(policy, monitors, request, now)
    else {
        panic!("auxiliary plan")
    };
    plan
}
fn bind(
    policy: &mut SessionPolicy,
    monitors: &mut TestMonitors,
    client: &str,
    plan: &AuxiliaryStreamPlan,
    transport: &str,
    now: Instant,
) {
    let request = Request::BindTransport {
        client_id: client.into(),
        session_id: plan.session_id.clone(),
        generation: plan.generation,
        reservation_id: plan.reservation_id.clone(),
        transport_id: transport.into(),
        transport_epoch: "a".repeat(32),
    };
    assert!(matches!(
        call(policy, monitors, request.clone(), now),
        Reply::Pending { .. }
    ));
    persist(policy, monitors, now);
    assert!(matches!(
        call(policy, monitors, request, now),
        Reply::Acknowledged {}
    ));
}
fn observe(
    client: &str,
    plan: &AuxiliaryStreamPlan,
    transport: &str,
    event: SessionEvent,
) -> Request {
    Request::Observe {
        client_id: client.into(),
        session_id: plan.session_id.clone(),
        generation: plan.generation,
        reservation_id: plan.reservation_id.clone(),
        transport_id: transport.into(),
        transport_epoch: Some("a".repeat(32)),
        event,
    }
}

#[test]
fn native_effects_and_transport_binding_wait_for_durable_readback() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let request = dispatch(
        context("monitor", false),
        "start",
        control(Control::Monitor),
    );
    assert!(matches!(
        call(&mut policy, &mut monitors, request.clone(), now),
        Reply::Pending { .. }
    ));
    assert_eq!(monitors.begins, 0);
    policy.synchronize(now, &mut monitors);
    assert_eq!(monitors.begins, 0);
    persist(&mut policy, &mut monitors, now);
    assert_eq!(monitors.begins, 1);
    let Reply::Dispatched {
        decision: Decision::Auxiliary { plan },
    } = call(&mut policy, &mut monitors, request, now)
    else {
        panic!("ready")
    };
    assert!(matches!(plan.capture, CaptureSource::ExactOutput { .. }));
    assert!(monitors.holds.contains(&(
        "monitor".into(),
        plan.generation,
        plan.reservation_id.clone()
    )));
    bind(
        &mut policy,
        &mut monitors,
        "monitor",
        &plan,
        "rtsp-one",
        now,
    );
    assert!(
        matches!(call(&mut policy,&mut monitors,Request::BindTransport{client_id:"monitor".into(),session_id:plan.session_id,generation:plan.generation,reservation_id:plan.reservation_id,transport_id:"foreign".into(),transport_epoch:"a".repeat(32)},now),Reply::Error{code,..} if code=="transport_binding_conflict")
    );
}

#[test]
fn input_never_acquires_display_or_audio_and_old_transport_end_cannot_drop_reconnect() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let first = auxiliary(
        &mut policy,
        &mut monitors,
        "input",
        "one",
        Control::Input,
        now,
    );
    assert_eq!(first.capture, CaptureSource::SyntheticBlack);
    assert!(!first.stream_audio);
    assert!(!first.host_audio);
    assert_eq!(monitors.begins, 0);
    bind(
        &mut policy,
        &mut monitors,
        "input",
        &first,
        "transport-one",
        now,
    );
    call(
        &mut policy,
        &mut monitors,
        observe(
            "input",
            &first,
            "transport-one",
            SessionEvent::StreamStarted,
        ),
        now,
    );
    persist(&mut policy, &mut monitors, now);
    let second = auxiliary(
        &mut policy,
        &mut monitors,
        "input",
        "two",
        Control::Input,
        now,
    );
    assert_eq!(second.session_id, first.session_id);
    assert_eq!(second.generation, first.generation);
    assert_ne!(second.reservation_id, first.reservation_id);
    call(
        &mut policy,
        &mut monitors,
        observe("input", &first, "transport-one", SessionEvent::StreamEnded),
        now,
    );
    persist(&mut policy, &mut monitors, now);
    assert_eq!(policy.role("input"), Some(Role::Input));
    let request = dispatch(context("input", false), "two", control(Control::Input));
    assert!(matches!(
        call(&mut policy, &mut monitors, request, now),
        Reply::Dispatched {
            decision: Decision::Auxiliary { .. }
        }
    ));
}

#[test]
fn control_permissions_are_checked_at_dispatch_independently_of_catalog_view() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let mut view = context("view", true);
    view.client.may_launch = false;
    view.client.may_terminate = false;
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            dispatch(view.clone(), "input", control(Control::Input)),
            now
        ),
        Reply::Dispatched {
            decision: Decision::Reject {
                status_code: 403,
                ..
            }
        }
    ));
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            dispatch(view.clone(), "resume", control(Control::Resume)),
            now
        ),
        Reply::Dispatched {
            decision: Decision::ResumeApplication { .. }
        }
    ));
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            dispatch(view, "terminate", control(Control::Terminate)),
            now
        ),
        Reply::Dispatched {
            decision: Decision::Reject {
                status_code: 403,
                ..
            }
        }
    ));
}

#[test]
fn termination_confirmation_requires_fresh_launch_same_game_and_unexpired_window() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let first = dispatch(
        context("secondary", true),
        "one",
        control(Control::Terminate),
    );
    assert!(matches!(
        call(&mut policy, &mut monitors, first.clone(), now),
        Reply::Dispatched {
            decision: Decision::Reject {
                status_code: 410,
                ..
            }
        }
    ));
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            first,
            now + Duration::from_secs(10)
        ),
        Reply::Dispatched {
            decision: Decision::Reject {
                status_code: 410,
                ..
            }
        }
    ));
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            dispatch(
                context("secondary", true),
                "two",
                control(Control::Terminate)
            ),
            now + Duration::from_secs(59)
        ),
        Reply::Dispatched {
            decision: Decision::StopApplication { generation: 7, .. }
        }
    ));
    let mut changed = context("secondary", true);
    changed.game.as_mut().unwrap().generation = 8;
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            dispatch(changed.clone(), "three", control(Control::Terminate)),
            now + Duration::from_secs(59)
        ),
        Reply::Dispatched {
            decision: Decision::Reject {
                status_code: 410,
                ..
            }
        }
    ));
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            dispatch(changed, "four", control(Control::Terminate)),
            now + Duration::from_secs(119)
        ),
        Reply::Dispatched {
            decision: Decision::Reject {
                status_code: 410,
                ..
            }
        }
    ));
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            dispatch(context("owner", true), "owner", control(Control::Terminate)),
            now
        ),
        Reply::Dispatched {
            decision: Decision::StopApplication { .. }
        }
    ));
}

#[test]
fn configured_profile_freezes_settings_and_survives_edit_and_deletion() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let mut profile = app("8", "Work screen");
    profile.uuid = "a1234567-1234-4234-8234-123456789abc".into();
    profile.monitor_profile = true;
    profile.properties = json!({"remote_monitor":{"width":2560,"height":1440,"refresh_millihz":90_000,"hdr_enabled":true},"dd_virtual_display_scale":125});
    inventory(
        &mut policy,
        &mut monitors,
        context("client", false),
        vec![profile.clone()],
        now,
    );
    let request = dispatch(
        context("client", false),
        "profile",
        Selection {
            id: "8".into(),
            uuid: Some(profile.uuid.clone()),
        },
    );
    assert!(matches!(
        call(&mut policy, &mut monitors, request.clone(), now),
        Reply::Pending { .. }
    ));
    persist(&mut policy, &mut monitors, now);
    assert!(matches!(
        call(&mut policy, &mut monitors, request, now),
        Reply::Dispatched {
            decision: Decision::Auxiliary { .. }
        }
    ));
    let owner = policy.owner("client").unwrap().clone();
    assert_eq!(owner.mode.width, 2560);
    assert!(owner.mode.hdr);
    profile.properties = json!({"dd_virtual_display_scale":200});
    inventory(
        &mut policy,
        &mut monitors,
        context("client", false),
        vec![profile],
        now,
    );
    assert_eq!(
        policy.owner("client").unwrap().profile_settings,
        owner.profile_settings
    );
    inventory(
        &mut policy,
        &mut monitors,
        context("client", false),
        vec![],
        now,
    );
    let resume = dispatch(context("client", false), "resume", control(Control::Resume));
    assert!(matches!(
        call(&mut policy, &mut monitors, resume.clone(), now),
        Reply::Pending { .. }
    ));
    persist(&mut policy, &mut monitors, now);
    assert!(matches!(
        call(&mut policy, &mut monitors, resume, now),
        Reply::Dispatched {
            decision: Decision::ResumeAuxiliary { .. }
        }
    ));
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            dispatch(
                context("client", false),
                "builtin",
                control(Control::Monitor)
            ),
            now
        ),
        Reply::Dispatched {
            decision: Decision::Reject {
                status_code: 409,
                ..
            }
        }
    ));
}

#[test]
fn cached_running_tile_retargets_original_application_after_game_changes() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let projection = inventory(
        &mut policy,
        &mut monitors,
        context("secondary", true),
        vec![app("42", "Running game"), app("9", "Another game")],
        now,
    );
    let Reply::Projected { entries, .. } = call(
        &mut policy,
        &mut monitors,
        Request::ReadProject {
            client_id: "secondary".into(),
            projection_id: projection,
            offset: 0,
            limit: 32,
        },
        now,
    ) else {
        panic!("projected")
    };
    let tile = entries[0].clone();
    inventory(
        &mut policy,
        &mut monitors,
        context("secondary", false),
        vec![app("42", "Renamed game")],
        now,
    );
    let selected = Selection {
        id: tile.id.clone(),
        uuid: Some(tile.uuid.clone()),
    };
    assert!(
        matches!(call(&mut policy,&mut monitors,dispatch(context("secondary",false),"cached",selected.clone()),now),Reply::Dispatched{decision:Decision::LaunchApplication{application_id,..}} if application_id=="42")
    );
    assert!(
        matches!(call(&mut policy,&mut monitors,Request::Artwork{context:context("secondary",false),selection:selected.clone()},now),Reply::Artwork{artwork:Artwork::Application{application_id}} if application_id=="42")
    );
    inventory(
        &mut policy,
        &mut monitors,
        context("secondary", false),
        vec![],
        now,
    );
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            dispatch(context("secondary", false), "deleted", selected),
            now
        ),
        Reply::Dispatched {
            decision: Decision::Reject {
                status_code: 404,
                ..
            }
        }
    ));
}

#[test]
fn cancel_is_role_scoped_and_special_roles_guard_secondary_game_cancel() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    auxiliary(
        &mut policy,
        &mut monitors,
        "monitor",
        "monitor",
        Control::Monitor,
        now,
    );
    let cancel = |client: &str| Request::Dispatch {
        context: context(client, true),
        operation_id: format!("cancel-{client}"),
        intent: DispatchIntent::Cancel,
        selection: None,
        mode: None,
    };
    assert!(matches!(
        call(&mut policy, &mut monitors, cancel("monitor"), now),
        Reply::Dispatched {
            decision: Decision::StopAuxiliary { .. }
        }
    ));
    assert!(matches!(
        call(&mut policy, &mut monitors, cancel("secondary"), now),
        Reply::Dispatched {
            decision: Decision::Reject {
                status_code: 403,
                ..
            }
        }
    ));
    assert!(matches!(
        call(&mut policy, &mut monitors, cancel("owner"), now),
        Reply::Dispatched {
            decision: Decision::StopApplication { .. }
        }
    ));
}

#[test]
fn restart_preserves_bound_capture_until_authoritative_retirement_proof() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let plan = auxiliary(
        &mut policy,
        &mut monitors,
        "monitor",
        "start",
        Control::Monitor,
        now,
    );
    bind(
        &mut policy,
        &mut monitors,
        "monitor",
        &plan,
        "rtsp-one",
        now,
    );
    let saved = policy.desired_document().1;
    let mut restarted = SessionPolicy::new(ProductSettings::default());
    restarted.restore(saved).unwrap();
    let mut new_monitors = TestMonitors::default();
    restarted.synchronize(now, &mut new_monitors);
    assert_eq!(new_monitors.begins, 0);
    let candidates = restarted.recovery_candidates();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].transport_id, "rtsp-one");
    let mut wrong = candidates[0].clone();
    wrong.runtime_epoch = "b".repeat(32);
    restarted.accept_retired(&wrong);
    assert_eq!(restarted.recovery_candidates().len(), 1);
    restarted.accept_retired(&candidates[0]);
    persist(&mut restarted, &mut new_monitors, now);
    assert!(restarted.recovery_candidates().is_empty());
    assert_eq!(new_monitors.begins, 1);
}

#[test]
fn canceling_unstarted_dispatch_retires_only_new_owner_after_journal_cleanup() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let request = dispatch(
        context("monitor", false),
        "pending",
        control(Control::Monitor),
    );
    call(&mut policy, &mut monitors, request, now);
    persist(&mut policy, &mut monitors, now);
    let cancel = Request::CancelDispatch {
        client_id: "monitor".into(),
        operation_id: "pending".into(),
    };
    assert!(matches!(
        call(&mut policy, &mut monitors, cancel.clone(), now),
        Reply::Pending { .. }
    ));
    persist(&mut policy, &mut monitors, now);
    assert!(matches!(
        call(&mut policy, &mut monitors, cancel, now),
        Reply::Acknowledged {}
    ));
    assert!(policy.owner("monitor").is_none());
}

#[test]
fn oversized_private_intent_is_rejected_before_native_or_persistence_effects() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    for index in 0..3 {
        let mut profile = app(&(index + 1).to_string(), "Large profile");
        profile.uuid = format!("a1234567-1234-4234-8234-{:012x}", index + 1);
        profile.monitor_profile = true;
        profile.properties = json!({"approved":"x".repeat(4000)});
        let client = format!("client-{index}");
        inventory(
            &mut policy,
            &mut monitors,
            context(&client, false),
            vec![profile.clone()],
            now,
        );
        let request = dispatch(
            context(&client, false),
            &format!("profile-{index}"),
            Selection {
                id: profile.id,
                uuid: Some(profile.uuid),
            },
        );
        assert!(matches!(
            call(&mut policy, &mut monitors, request, now),
            Reply::Pending { .. }
        ));
        persist(&mut policy, &mut monitors, now);
    }
    let before = policy.desired.owners.len();
    let mut profile = app("99", "Too much");
    profile.uuid = "a1234567-1234-4234-8234-000000000099".into();
    profile.monitor_profile = true;
    profile.properties = json!({"approved":"x".repeat(4000)});
    inventory(
        &mut policy,
        &mut monitors,
        context("overflow", false),
        vec![profile.clone()],
        now,
    );
    assert!(
        matches!(call(&mut policy,&mut monitors,dispatch(context("overflow",false),"overflow",Selection{id:profile.id,uuid:Some(profile.uuid)}),now),Reply::Error{code,..} if code=="owner_journal_capacity_exhausted")
    );
    assert_eq!(policy.desired.owners.len(), before);
    assert_eq!(monitors.begins, before);
}

#[test]
fn game_source_borrowers_have_independent_holds_after_monitor_role_disconnects() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let screen = auxiliary(
        &mut policy,
        &mut monitors,
        "owner",
        "screen",
        Control::Monitor,
        now,
    );
    bind(
        &mut policy,
        &mut monitors,
        "owner",
        &screen,
        "screen-transport",
        now,
    );
    call(
        &mut policy,
        &mut monitors,
        observe(
            "owner",
            &screen,
            "screen-transport",
            SessionEvent::StreamEnded,
        ),
        now,
    );
    persist(&mut policy, &mut monitors, now);
    inventory(
        &mut policy,
        &mut monitors,
        context("owner", false),
        vec![app("42", "Game")],
        now,
    );
    let launch = dispatch(
        context("owner", false),
        "game",
        Selection {
            id: "42".into(),
            uuid: None,
        },
    );
    assert!(matches!(
        call(&mut policy, &mut monitors, launch.clone(), now),
        Reply::Pending { .. }
    ));
    persist(&mut policy, &mut monitors, now);
    let Reply::Dispatched {
        decision:
            Decision::LaunchApplication {
                source: Some(original),
                ..
            },
    } = call(&mut policy, &mut monitors, launch, now)
    else {
        panic!("application source")
    };
    let original_plan = AuxiliaryStreamPlan {
        session_id: original.session_id.clone(),
        generation: original.generation,
        reservation_id: original.reservation_id.clone(),
        capture: original.capture.clone(),
        mode: original.mode,
        source_mode: Some(original.mode),
        stream_audio: true,
        host_audio: true,
    };
    bind(
        &mut policy,
        &mut monitors,
        "owner",
        &original_plan,
        "game-transport",
        now,
    );
    let application = Request::BindApplication {
        client_id: "owner".into(),
        session_id: original.session_id.clone(),
        generation: original.generation,
        reservation_id: original.reservation_id.clone(),
        application_operation_id: format!("ale1:{}:game", "b".repeat(32)),
        application_epoch: "b".repeat(32),
        application_generation: 77,
    };
    assert!(matches!(
        call(&mut policy, &mut monitors, application.clone(), now),
        Reply::Pending { .. }
    ));
    persist(&mut policy, &mut monitors, now);
    assert!(matches!(
        call(&mut policy, &mut monitors, application, now),
        Reply::Acknowledged {}
    ));
    call(
        &mut policy,
        &mut monitors,
        observe(
            "owner",
            &original_plan,
            "game-transport",
            SessionEvent::StreamStarted,
        ),
        now,
    );
    persist(&mut policy, &mut monitors, now);
    let mut secondary = context("secondary", true);
    let game = secondary.game.as_mut().unwrap();
    game.generation = 77;
    game.source = Some(original.clone());
    let resume = dispatch(secondary.clone(), "join", control(Control::Resume));
    assert!(matches!(
        call(&mut policy, &mut monitors, resume.clone(), now),
        Reply::Pending { .. }
    ));
    persist(&mut policy, &mut monitors, now);
    let Reply::Dispatched {
        decision:
            Decision::ResumeApplication {
                source: Some(joined),
                ..
            },
    } = call(&mut policy, &mut monitors, resume, now)
    else {
        panic!("fresh secondary reservation")
    };
    assert_ne!(joined.reservation_id, original.reservation_id);
    assert_eq!(joined.session_id, original.session_id);
    let joined_plan = AuxiliaryStreamPlan {
        session_id: joined.session_id.clone(),
        generation: joined.generation,
        reservation_id: joined.reservation_id.clone(),
        capture: joined.capture.clone(),
        mode: joined.mode,
        source_mode: Some(joined.mode),
        stream_audio: true,
        host_audio: false,
    };
    bind(
        &mut policy,
        &mut monitors,
        "secondary",
        &joined_plan,
        "joined-transport",
        now,
    );
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            Request::VerifySource {
                client_id: "secondary".into(),
                source: joined.clone()
            },
            now
        ),
        Reply::Verified { .. }
    ));
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            Request::VerifySource {
                client_id: "owner".into(),
                source: joined.clone()
            },
            now
        ),
        Reply::Error { .. }
    ));
    let disconnect = observe(
        "owner",
        &screen,
        "screen-transport",
        SessionEvent::ExplicitDisconnect,
    );
    call(&mut policy, &mut monitors, disconnect.clone(), now);
    persist(&mut policy, &mut monitors, now);
    assert!(matches!(
        call(&mut policy, &mut monitors, disconnect, now),
        Reply::Acknowledged {}
    ));
    assert_eq!(policy.role("owner"), None);
    assert!(policy.owner("owner").is_some());
    assert!(monitors.snapshot("owner").unwrap().source_ready);
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            Request::VerifySource {
                client_id: "secondary".into(),
                source: joined
            },
            now
        ),
        Reply::Verified { .. }
    ));
    inventory(
        &mut policy,
        &mut monitors,
        context("owner", false),
        vec![app("42", "Game"), app("9", "Replacement")],
        now,
    );
    let mut replacement_context = context("owner", true);
    replacement_context.game.as_mut().unwrap().generation = 77;
    replacement_context.game.as_mut().unwrap().source = Some(original);
    let replacement_selection = Selection {
        id: "9".into(),
        uuid: None,
    };
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            dispatch(
                replacement_context.clone(),
                "replace-prompt",
                replacement_selection.clone()
            ),
            now
        ),
        Reply::Dispatched {
            decision: Decision::Reject {
                status_code: 410,
                ..
            }
        }
    ));
    let confirmed = dispatch(
        replacement_context,
        "replace-confirmed",
        replacement_selection,
    );
    assert!(matches!(
        call(&mut policy, &mut monitors, confirmed.clone(), now),
        Reply::Pending { .. }
    ));
    persist(&mut policy, &mut monitors, now);
    assert!(
        matches!(call(&mut policy,&mut monitors,confirmed,now),Reply::Dispatched{decision:Decision::ReplaceApplication{application_id,expected_current_generation:77,..}} if application_id=="9")
    );
    let cancel = Request::CancelDispatch {
        client_id: "owner".into(),
        operation_id: "replace-confirmed".into(),
    };
    call(&mut policy, &mut monitors, cancel.clone(), now);
    persist(&mut policy, &mut monitors, now);
    assert!(matches!(
        call(&mut policy, &mut monitors, cancel, now),
        Reply::Acknowledged {}
    ));
    call(
        &mut policy,
        &mut monitors,
        observe(
            "secondary",
            &joined_plan,
            "joined-transport",
            SessionEvent::StreamEnded,
        ),
        now,
    );
    persist(&mut policy, &mut monitors, now);
    assert!(policy.owner("owner").is_some());
    call(
        &mut policy,
        &mut monitors,
        observe(
            "owner",
            &original_plan,
            "game-transport",
            SessionEvent::StreamEnded,
        ),
        now,
    );
    persist(&mut policy, &mut monitors, now);
    assert!(policy.owner("owner").is_none());
    assert!(policy.is_retired());
}

#[test]
fn recovery_work_is_scoped_durable_and_not_completed_by_consumer_claim() {
    let now = Instant::now();
    let mut initial = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let plan = auxiliary(
        &mut initial,
        &mut monitors,
        "client",
        "input",
        Control::Input,
        now,
    );
    bind(
        &mut initial,
        &mut monitors,
        "client",
        &plan,
        "rtsp-recover",
        now,
    );
    let mut policy = SessionPolicy::new(ProductSettings::default());
    policy.restore(initial.desired_document().1).unwrap();
    let mut monitors = TestMonitors::default();
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            Request::RecoveryWork {
                cursor: None,
                limit: 8
            },
            now
        ),
        Reply::Pending { .. }
    ));
    persist(&mut policy, &mut monitors, now);
    let Reply::RecoveryWork { items, next_cursor } = call(
        &mut policy,
        &mut monitors,
        Request::RecoveryWork {
            cursor: None,
            limit: 8,
        },
        now,
    ) else {
        panic!("work")
    };
    assert_eq!(items.len(), 1);
    assert!(next_cursor.is_none());
    let work = items[0].id.clone();
    assert!(
        matches!(policy.handle("other-adapter",Request::RecoveryWork{cursor:None,limit:8},now,&mut monitors),Reply::RecoveryWork{items,..} if items.is_empty())
    );
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            Request::ReconcileRecovery {
                work_id: work.clone()
            },
            now
        ),
        Reply::Pending { .. }
    ));
    let candidate = policy.recovery_candidates()[0].clone();
    policy.accept_retired(&candidate);
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            Request::ReconcileRecovery {
                work_id: work.clone()
            },
            now
        ),
        Reply::Pending { .. }
    ));
    persist(&mut policy, &mut monitors, now);
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            Request::ReconcileRecovery { work_id: work },
            now
        ),
        Reply::Acknowledged {}
    ));
    assert!(policy.owner("client").is_none());
}

#[test]
fn delayed_cancel_of_old_start_cannot_release_new_generation() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let first = auxiliary(
        &mut policy,
        &mut monitors,
        "client",
        "first",
        Control::Monitor,
        now,
    );
    bind(
        &mut policy,
        &mut monitors,
        "client",
        &first,
        "transport-first",
        now,
    );
    call(
        &mut policy,
        &mut monitors,
        observe(
            "client",
            &first,
            "transport-first",
            SessionEvent::StreamEnded,
        ),
        now,
    );
    persist(&mut policy, &mut monitors, now);
    call(
        &mut policy,
        &mut monitors,
        observe(
            "client",
            &first,
            "transport-first",
            SessionEvent::ExplicitDisconnect,
        ),
        now,
    );
    persist(&mut policy, &mut monitors, now);
    assert!(policy.owner("client").is_none());
    let second = auxiliary(
        &mut policy,
        &mut monitors,
        "client",
        "second",
        Control::Monitor,
        now,
    );
    assert_ne!(second.generation, first.generation);
    bind(
        &mut policy,
        &mut monitors,
        "client",
        &second,
        "transport-second",
        now,
    );
    call(
        &mut policy,
        &mut monitors,
        observe(
            "client",
            &second,
            "transport-second",
            SessionEvent::StreamEnded,
        ),
        now,
    );
    persist(&mut policy, &mut monitors, now);
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            Request::CancelDispatch {
                client_id: "client".into(),
                operation_id: "first".into()
            },
            now
        ),
        Reply::Acknowledged {}
    ));
    assert_eq!(
        policy.owner("client").unwrap().generation,
        second.generation
    );
    assert_eq!(policy.role("client"), Some(Role::Monitor));
    assert!(!monitors.snapshot("client").unwrap().reference_released);
}

#[test]
fn transient_begin_error_cannot_retire_a_durably_held_source() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let plan = auxiliary(
        &mut policy,
        &mut monitors,
        "client",
        "source",
        Control::Monitor,
        now,
    );
    bind(
        &mut policy,
        &mut monitors,
        "client",
        &plan,
        "transport-live",
        now,
    );
    let before = monitors.holds.clone();
    monitors.fail_next_begin = true;
    policy.synchronize(now, &mut monitors);
    assert_eq!(monitors.holds, before);
    assert!(policy.applied_holds.contains(&(
        "client".into(),
        plan.generation,
        plan.reservation_id
    )));
}

#[test]
fn committed_dispatch_cannot_be_canceled_by_a_late_http_cancellation() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let plan = auxiliary(
        &mut policy,
        &mut monitors,
        "client",
        "start",
        Control::Monitor,
        now,
    );
    bind(
        &mut policy,
        &mut monitors,
        "client",
        &plan,
        "native-live",
        now,
    );
    call(
        &mut policy,
        &mut monitors,
        observe("client", &plan, "native-live", SessionEvent::StreamStarted),
        now,
    );
    persist(&mut policy, &mut monitors, now);
    let before = policy.desired_document().1;
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            Request::CancelDispatch {
                client_id: "client".into(),
                operation_id: "start".into()
            },
            now
        ),
        Reply::Acknowledged {}
    ));
    assert_eq!(policy.desired_document().1, before);
    assert!(policy.recovery_candidates().is_empty());
    assert_eq!(policy.role("client"), Some(Role::Monitor));
    assert!(!monitors.snapshot("client").unwrap().reference_released);
}

#[test]
fn terminal_source_failure_cleans_only_pending_owner_before_rejecting() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let request = dispatch(
        context("client", false),
        "unsupported",
        control(Control::Monitor),
    );
    assert!(matches!(
        call(&mut policy, &mut monitors, request.clone(), now),
        Reply::Pending { .. }
    ));
    persist(&mut policy, &mut monitors, now);
    let status = monitors.states.get_mut("client").unwrap();
    status.ready = false;
    status.source_ready = false;
    status.retryable = false;
    status.error = Some("selected capture cannot serve this mode".into());
    assert!(matches!(
        call(&mut policy, &mut monitors, request.clone(), now),
        Reply::Pending { .. }
    ));
    assert!(policy.owner("client").unwrap().release_requested);
    persist(&mut policy, &mut monitors, now);
    assert!(matches!(
        call(&mut policy, &mut monitors, request.clone(), now),
        Reply::Dispatched {
            decision: Decision::Reject {
                status_code: 503,
                ..
            }
        }
    ));
    assert!(policy.owner("client").is_none());
    assert!(matches!(
        call(&mut policy, &mut monitors, request, now),
        Reply::Dispatched {
            decision: Decision::Reject {
                status_code: 503,
                ..
            }
        }
    ));
    assert_eq!(monitors.begins, 1);
}

#[test]
fn recovery_cursor_cannot_skip_work_while_a_retirement_is_being_persisted() {
    let now = Instant::now();
    let mut initial = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    for client in ["one", "two"] {
        let plan = auxiliary(
            &mut initial,
            &mut monitors,
            client,
            "input",
            Control::Input,
            now,
        );
        bind(
            &mut initial,
            &mut monitors,
            client,
            &plan,
            &format!("transport-{client}"),
            now,
        );
    }
    let mut policy = SessionPolicy::new(ProductSettings::default());
    policy.restore(initial.desired_document().1).unwrap();
    let mut monitors = TestMonitors::default();
    persist(&mut policy, &mut monitors, now);
    let Reply::RecoveryWork {
        items,
        next_cursor: Some(cursor),
    } = call(
        &mut policy,
        &mut monitors,
        Request::RecoveryWork {
            cursor: None,
            limit: 1,
        },
        now,
    )
    else {
        panic!("first page")
    };
    assert_eq!(items[0].client_id, "one");
    let first = policy
        .recovery_candidates()
        .into_iter()
        .find(|candidate| candidate.borrower_id == "one")
        .unwrap();
    policy.accept_retired(&first);
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            Request::RecoveryWork {
                cursor: Some(cursor.clone()),
                limit: 1
            },
            now
        ),
        Reply::Pending { .. }
    ));
    persist(&mut policy, &mut monitors, now);
    assert!(
        matches!(call(&mut policy,&mut monitors,Request::RecoveryWork{cursor:Some(cursor),limit:1},now),Reply::Error{code,..} if code=="recovery_cursor_changed")
    );
    assert!(
        matches!(call(&mut policy,&mut monitors,Request::RecoveryWork{cursor:None,limit:1},now),Reply::RecoveryWork{items,next_cursor:None} if items[0].client_id=="two")
    );
}

#[test]
fn list_and_view_permissions_control_different_public_results() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let mut list_only = context("list-only", true);
    list_only.client.may_view = false;
    list_only.client.may_launch = false;
    let projection = inventory(
        &mut policy,
        &mut monitors,
        list_only.clone(),
        vec![app("42", "Game")],
        now,
    );
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            Request::ReadProject {
                client_id: "list-only".into(),
                projection_id: projection,
                offset: 0,
                limit: 32
            },
            now
        ),
        Reply::Projected { .. }
    ));
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            Request::Artwork {
                context: list_only.clone(),
                selection: control(Control::Input)
            },
            now
        ),
        Reply::Artwork {
            artwork: Artwork::InlinePng { .. }
        }
    ));
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            dispatch(list_only, "resume-denied", control(Control::Resume)),
            now
        ),
        Reply::Dispatched {
            decision: Decision::Reject {
                status_code: 403,
                ..
            }
        }
    ));
    let mut view_only = context("view-only", true);
    view_only.client.may_list = false;
    view_only.client.may_launch = false;
    let projection = inventory(
        &mut policy,
        &mut monitors,
        view_only.clone(),
        vec![app("42", "Game")],
        now,
    );
    assert!(
        matches!(call(&mut policy,&mut monitors,Request::ReadProject{client_id:"view-only".into(),projection_id:projection,offset:0,limit:32},now),Reply::Error{code,..} if code=="forbidden")
    );
    assert!(matches!(
        call(
            &mut policy,
            &mut monitors,
            dispatch(view_only.clone(), "resume-view", control(Control::Resume)),
            now
        ),
        Reply::Dispatched {
            decision: Decision::ResumeApplication { .. }
        }
    ));
    assert!(
        matches!(call(&mut policy,&mut monitors,Request::Artwork{context:view_only,selection:control(Control::Input)},now),Reply::Error{code,..} if code=="forbidden")
    );
}

#[test]
fn accepted_monitor_profile_remains_resolved_while_catalog_changes_during_startup() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let mut profile = app("8", "Accepted profile");
    profile.uuid = "a1234567-1234-4234-8234-123456789abc".into();
    profile.monitor_profile = true;
    profile.properties = json!({"dd_virtual_display_scale":125});
    inventory(
        &mut policy,
        &mut monitors,
        context("client", false),
        vec![profile.clone()],
        now,
    );
    let request = dispatch(
        context("client", false),
        "pending-profile",
        Selection {
            id: profile.id,
            uuid: Some(profile.uuid),
        },
    );
    assert!(matches!(
        call(&mut policy, &mut monitors, request.clone(), now),
        Reply::Pending { .. }
    ));
    inventory(
        &mut policy,
        &mut monitors,
        context("client", false),
        vec![],
        now,
    );
    persist(&mut policy, &mut monitors, now);
    assert!(matches!(
        call(&mut policy, &mut monitors, request, now),
        Reply::Dispatched {
            decision: Decision::Auxiliary { .. }
        }
    ));
    assert_eq!(
        policy.owner("client").unwrap().profile_settings,
        json!({"dd_virtual_display_scale":125})
    );
}

#[test]
fn cancel_after_provider_restart_retires_an_unstarted_creator_before_acknowledging() {
    let now = Instant::now();
    let mut initial = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    auxiliary(
        &mut initial,
        &mut monitors,
        "client",
        "creator",
        Control::Monitor,
        now,
    );
    let saved = initial.desired_document().1;
    monitors.holds.clear();
    let mut restarted = SessionPolicy::new(ProductSettings::default());
    restarted.restore(saved).unwrap();
    persist(&mut restarted, &mut monitors, now);
    assert!(restarted.operations.is_empty());
    assert_eq!(restarted.role("client"), Some(Role::Monitor));
    let cancel = Request::CancelDispatch {
        client_id: "client".into(),
        operation_id: "creator".into(),
    };
    assert!(matches!(
        call(&mut restarted, &mut monitors, cancel.clone(), now),
        Reply::Pending { .. }
    ));
    assert!(!monitors.snapshot("client").unwrap().reference_released);
    persist(&mut restarted, &mut monitors, now);
    assert!(matches!(
        call(&mut restarted, &mut monitors, cancel, now),
        Reply::Acknowledged {}
    ));
    assert!(restarted.owner("client").is_none());
    assert!(
        monitors
            .snapshot("client")
            .unwrap()
            .source_reference_released
    );
}

#[test]
fn restart_does_not_make_a_committed_monitor_cancellable_as_a_pending_start() {
    let now = Instant::now();
    let mut initial = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let plan = auxiliary(
        &mut initial,
        &mut monitors,
        "client",
        "creator",
        Control::Monitor,
        now,
    );
    bind(
        &mut initial,
        &mut monitors,
        "client",
        &plan,
        "transport",
        now,
    );
    call(
        &mut initial,
        &mut monitors,
        observe("client", &plan, "transport", SessionEvent::StreamStarted),
        now,
    );
    persist(&mut initial, &mut monitors, now);
    call(
        &mut initial,
        &mut monitors,
        observe("client", &plan, "transport", SessionEvent::StreamEnded),
        now,
    );
    persist(&mut initial, &mut monitors, now);
    let mut restarted = SessionPolicy::new(ProductSettings::default());
    restarted.restore(initial.desired_document().1).unwrap();
    persist(&mut restarted, &mut monitors, now);
    assert!(restarted.owner("client").unwrap().role_committed);
    assert!(matches!(
        call(
            &mut restarted,
            &mut monitors,
            Request::CancelDispatch {
                client_id: "client".into(),
                operation_id: "creator".into()
            },
            now
        ),
        Reply::Acknowledged {}
    ));
    assert_eq!(restarted.role("client"), Some(Role::Monitor));
    assert!(!monitors.snapshot("client").unwrap().reference_released);
}

#[test]
fn released_existing_target_reference_does_not_block_a_new_input_role() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let mut profile = app("8", "Physical target");
    profile.uuid = "a1234567-1234-4234-8234-123456789abc".into();
    profile.monitor_profile = true;
    profile.properties = json!({"monitor_target":{"kind":"existing","selector":"approved-target"}});
    inventory(
        &mut policy,
        &mut monitors,
        context("client", false),
        vec![profile.clone()],
        now,
    );
    let request = dispatch(
        context("client", false),
        "physical",
        Selection {
            id: profile.id,
            uuid: Some(profile.uuid),
        },
    );
    call(&mut policy, &mut monitors, request.clone(), now);
    persist(&mut policy, &mut monitors, now);
    let Reply::Dispatched {
        decision: Decision::Auxiliary { plan },
    } = call(&mut policy, &mut monitors, request, now)
    else {
        panic!("physical source")
    };
    bind(
        &mut policy,
        &mut monitors,
        "client",
        &plan,
        "physical-transport",
        now,
    );
    call(
        &mut policy,
        &mut monitors,
        observe(
            "client",
            &plan,
            "physical-transport",
            SessionEvent::StreamEnded,
        ),
        now,
    );
    persist(&mut policy, &mut monitors, now);
    call(
        &mut policy,
        &mut monitors,
        observe(
            "client",
            &plan,
            "physical-transport",
            SessionEvent::ExplicitDisconnect,
        ),
        now,
    );
    persist(&mut policy, &mut monitors, now);
    assert!(
        monitors
            .snapshot("client")
            .unwrap()
            .source_reference_released
    );
    assert!(policy.owner("client").is_none());
    let input = auxiliary(
        &mut policy,
        &mut monitors,
        "client",
        "input",
        Control::Input,
        now,
    );
    assert_eq!(input.capture, CaptureSource::SyntheticBlack);
}
