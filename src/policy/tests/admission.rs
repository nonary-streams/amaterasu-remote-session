use super::*;

fn selected_monitor(mode: Mode, operation: &str, selection: Selection) -> Request {
    Request::Dispatch {
        context: context("client", false),
        operation_id: operation.into(),
        intent: DispatchIntent::Launch,
        selection: Some(selection),
        mode: Some(mode),
    }
}

fn small_encoded_mode() -> Mode {
    Mode {
        width: 64,
        height: 48,
        ..mode()
    }
}

fn configured_profile(properties: Value) -> Application {
    let mut profile = app("8", "Selected display");
    profile.uuid = "a1234567-1234-4234-8234-123456789abc".into();
    profile.monitor_profile = true;
    profile.properties = properties;
    profile
}

#[test]
fn impossible_owned_source_modes_do_not_consume_durable_ownership_or_generation() {
    for (width, height) in [(64, 48), (319, 200), (320, 199), (7681, 4320), (7680, 4321)] {
        let now = Instant::now();
        let mut policy = SessionPolicy::new(ProductSettings::default());
        let mut monitors = TestMonitors::default();
        let before = policy.desired_document();
        let result = call(
            &mut policy,
            &mut monitors,
            selected_monitor(
                Mode {
                    width,
                    height,
                    ..mode()
                },
                "invalid-source",
                control(Control::Monitor),
            ),
            now,
        );
        assert!(
            matches!(
                result,
                Reply::Dispatched {
                    decision: Decision::Reject {
                        status_code: 400,
                        ..
                    }
                }
            ),
            "{width}x{height} returned {result:?}"
        );
        assert_eq!(policy.desired_document(), before);
        assert!(!policy.needs_persistence());
        assert_eq!(monitors.begins, 0);
        assert_eq!(monitors.releases, 0);
    }
}

#[test]
fn invalid_retained_mode_change_preserves_the_accepted_source() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let plan = auxiliary(
        &mut policy,
        &mut monitors,
        "client",
        "original",
        Control::Monitor,
        now,
    );
    bind(
        &mut policy,
        &mut monitors,
        "client",
        &plan,
        "original-transport",
        now,
    );
    for event in [SessionEvent::StreamStarted, SessionEvent::StreamEnded] {
        call(
            &mut policy,
            &mut monitors,
            observe("client", &plan, "original-transport", event),
            now,
        );
        persist(&mut policy, &mut monitors, now);
    }
    let before = policy.desired_document();
    let source_before = monitors.snapshot("client").unwrap().source_mode;
    let result = call(
        &mut policy,
        &mut monitors,
        selected_monitor(
            small_encoded_mode(),
            "invalid-resize",
            control(Control::Monitor),
        ),
        now,
    );
    assert!(matches!(
        result,
        Reply::Dispatched {
            decision: Decision::Reject {
                status_code: 400,
                ..
            }
        }
    ));
    assert_eq!(policy.desired_document(), before);
    assert_eq!(
        monitors.snapshot("client").unwrap().source_mode,
        source_before
    );
    assert!(!policy.needs_persistence());
}

#[test]
fn invalid_profile_source_does_not_borrow_the_valid_encoded_dimensions() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let profile = configured_profile(json!({"remote_monitor":{
        "width":64,"height":48,"refresh_millihz":60000,"hdr_enabled":false
    }}));
    inventory(
        &mut policy,
        &mut monitors,
        context("client", false),
        vec![profile.clone()],
        now,
    );
    let before = policy.desired_document();
    let result = call(
        &mut policy,
        &mut monitors,
        selected_monitor(
            mode(),
            "invalid-profile",
            Selection {
                id: profile.id,
                uuid: Some(profile.uuid),
            },
        ),
        now,
    );
    assert!(matches!(
        result,
        Reply::Dispatched {
            decision: Decision::Reject {
                status_code: 400,
                ..
            }
        }
    ));
    assert_eq!(policy.desired_document(), before);
    assert!(!policy.needs_persistence());
    assert_eq!(monitors.begins, 0);
}

#[test]
fn small_synthetic_input_does_not_need_managed_monitor_dimensions() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let request = selected_monitor(small_encoded_mode(), "small-input", control(Control::Input));
    assert!(matches!(
        call(&mut policy, &mut monitors, request.clone(), now),
        Reply::Pending { .. }
    ));
    persist(&mut policy, &mut monitors, now);
    let Reply::Dispatched {
        decision: Decision::Auxiliary { plan },
    } = call(&mut policy, &mut monitors, request, now)
    else {
        panic!("synthetic input incorrectly required a native monitor mode")
    };
    assert_eq!(plan.mode, small_encoded_mode());
    assert_eq!(plan.source_mode, None);
    assert_eq!(plan.capture, CaptureSource::SyntheticBlack);
    assert_eq!(monitors.begins, 0);
}

#[test]
fn profile_source_mode_is_validated_independently_of_the_encoded_mode() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let profile = configured_profile(json!({"remote_monitor":{
        "width":640,"height":480,"refresh_millihz":60000,"hdr_enabled":false
    }}));
    inventory(
        &mut policy,
        &mut monitors,
        context("client", false),
        vec![profile.clone()],
        now,
    );
    let request = selected_monitor(
        small_encoded_mode(),
        "configured",
        Selection {
            id: profile.id,
            uuid: Some(profile.uuid),
        },
    );
    assert!(matches!(
        call(&mut policy, &mut monitors, request.clone(), now),
        Reply::Pending { .. }
    ));
    persist(&mut policy, &mut monitors, now);
    let Reply::Dispatched {
        decision: Decision::Auxiliary { plan },
    } = call(&mut policy, &mut monitors, request, now)
    else {
        panic!("valid configured source was rejected")
    };
    assert_eq!(plan.mode, small_encoded_mode());
    assert_eq!(
        plan.source_mode,
        Some(Mode {
            width: 640,
            height: 480,
            ..mode()
        })
    );
}

#[test]
fn existing_source_keeps_its_observed_mode_with_a_small_encoded_request() {
    let now = Instant::now();
    let mut policy = SessionPolicy::new(ProductSettings::default());
    let mut monitors = TestMonitors::default();
    let profile = configured_profile(json!({"monitor_target":{
        "kind":"existing","selector":"approved-physical-target"
    }}));
    inventory(
        &mut policy,
        &mut monitors,
        context("client", false),
        vec![profile.clone()],
        now,
    );
    let request = selected_monitor(
        small_encoded_mode(),
        "borrowed",
        Selection {
            id: profile.id,
            uuid: Some(profile.uuid),
        },
    );
    assert!(matches!(
        call(&mut policy, &mut monitors, request.clone(), now),
        Reply::Pending { .. }
    ));
    persist(&mut policy, &mut monitors, now);
    let observed = Mode {
        width: 2560,
        height: 1440,
        ..mode()
    };
    let source = monitors.states.get_mut("client").unwrap();
    source.source_mode = Some(observed);
    source.source_route = "desktop_output".into();
    let Reply::Dispatched {
        decision: Decision::Auxiliary { plan },
    } = call(&mut policy, &mut monitors, request, now)
    else {
        panic!("existing source was rejected because of its encoded mode")
    };
    assert_eq!(plan.mode, small_encoded_mode());
    assert_eq!(plan.source_mode, Some(observed));
    assert!(
        matches!(plan.capture, CaptureSource::ExactOutput { ref route_key, .. }
        if route_key == "desktop_output")
    );
}
