//! Observed-mode transactions preserve the owned allocation and frozen profile.
use super::*;

impl Monitor {
    pub(super) fn new_mode_operation(&self, desired: &MonitorRequest) -> String {
        let mut digest = Sha256::new();
        digest.update(b"amaterasu.remote-monitor.mode-operation.v1\0");
        digest.update(self.identity.as_bytes());
        digest.update(self.generation.to_be_bytes());
        digest.update(self.mode_revision.to_be_bytes());
        digest.update(desired.width.to_be_bytes());
        digest.update(desired.height.to_be_bytes());
        digest.update(desired.refresh_millihz.to_be_bytes());
        digest.update([u8::from(desired.hdr_enabled)]);
        format!("remote-mode-{:x}", digest.finalize())
    }
    pub(super) fn activate_queued_mode(&mut self, now: Instant) {
        if !self.ready
            || !self.source_reference_owned
            || !self.scope_reserved
            || self.release_requested
            || !self.captures.is_empty()
        {
            return;
        }
        let Some(desired) = self.queued_mode_request.take() else {
            return;
        };
        self.ready = false;
        self.source_ready = false;
        self.output = None;
        self.mode_operation = Some(self.new_mode_operation(&desired));
        self.desired_request = Some(desired);
        self.schedule(Step::PrepareMode, now, Duration::ZERO);
    }
    pub(super) fn reconcile_operation(&self) -> Result<Value, String> {
        amaterasu_module_sdk::display::ScopedOperation::ScopedReconcile {
            operation_id: format!(
                "{}-display",
                self.mode_operation
                    .as_deref()
                    .ok_or("retained_mode_operation_missing")?
            ),
            identity: self.identity.clone(),
            reference_id: self.source_reference.clone(),
        }
        .payload()
    }

    pub(super) fn restore_mode_intent(&mut self, monitor: &Value, now: Instant) -> bool {
        if self.desired_request.is_some() || self.release_requested {
            return false;
        }
        let intent = &monitor["retained_mode"];
        if intent["state"] == "pending" {
            self.ready = false;
            self.source_ready = false;
            self.output = None;
            self.error = Some("retained_mode_pending".into());
            self.schedule(Step::InspectSource, now, RETRY_INTERVAL);
            return true;
        }
        if intent["state"] != "prepared" {
            return false;
        }
        let requested = &self.request;
        let desired = &intent["specification"];
        let expected = BTreeSet::from([self.reference.clone(), self.source_reference.clone()]);
        let observed = intent["expected_reference_ids"]
            .as_array()
            .map(|references| {
                references
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<BTreeSet<_>>()
            });
        let operation = intent["operation_id"]
            .as_str()
            .filter(|operation| valid_identity(operation));
        let bound = intent["reference_id"] == self.source_reference
            && observed.as_ref() == Some(&expected)
            && intent["expected_mode_revision"] == self.mode_revision
            && desired["identity"] == self.identity
            && desired["name"] == "Remote Monitor"
            && desired["width"] == requested.width
            && desired["height"] == requested.height
            && desired["refresh_millihz"] == requested.refresh_millihz
            && desired["hdr_enabled"] == requested.hdr_enabled;
        let Some(operation) = operation.filter(|_| bound) else {
            self.reject_status("retained_mode_intent_mismatch", now);
            return true;
        };
        let previous = &monitor["specification"];
        let old_mode = || -> Option<(u32, u32, u32, bool)> {
            Some((
                u32::try_from(previous["width"].as_u64()?).ok()?,
                u32::try_from(previous["height"].as_u64()?).ok()?,
                u32::try_from(previous["refresh_millihz"].as_u64()?).ok()?,
                previous["hdr_enabled"].as_bool()?,
            ))
        };
        let Some((width, height, refresh, hdr)) = old_mode() else {
            self.reject_status("invalid_retained_mode_baseline", now);
            return true;
        };
        if previous["identity"] != self.identity || previous["name"] != "Remote Monitor" {
            self.reject_status("invalid_retained_mode_baseline", now);
            return true;
        }
        self.desired_request = Some(self.request.clone());
        self.request.width = width;
        self.request.height = height;
        self.request.refresh_millihz = refresh;
        self.request.hdr_enabled = hdr;
        self.mode_operation = Some(operation.into());
        self.display_target = monitor["ready"]["device_path"]
            .as_str()
            .filter(|target| {
                !target.is_empty() && target.len() <= 4096 && !target.chars().any(char::is_control)
            })
            .map(str::to_owned);
        self.ready = false;
        self.source_ready = false;
        self.output = None;
        self.schedule(Step::ModeReconcile, now, Duration::ZERO);
        true
    }

    pub(super) fn mode_operation(&self) -> Result<Value, String> {
        let desired = self
            .desired_request
            .as_ref()
            .ok_or("retained_mode_request_missing")?;
        let operation_id = self
            .mode_operation
            .as_deref()
            .ok_or("retained_mode_operation_missing")?;
        let payload: Result<Value, String> = match self.step {
            Step::PrepareMode => Ok(json!({
                "action":"prepare_retained_mode","operation_id":operation_id,
                "identity":self.identity,"reference_id":self.source_reference,
                "expected_reference_ids":[self.reference,self.source_reference],
                "expected_mode_revision":self.mode_revision,
                "width":desired.width,"height":desired.height,
                "refresh_millihz":desired.refresh_millihz,"hdr_enabled":desired.hdr_enabled,
            })),
            Step::CommitMode => Ok(
                json!({"action":"commit_retained_mode","operation_id":operation_id,"identity":self.identity,"reference_id":self.source_reference}),
            ),
            Step::AbortMode => Ok(
                json!({"action":"abort_retained_mode","operation_id":operation_id,"identity":self.identity,"reference_id":self.source_reference}),
            ),
            _ => Err("invalid_retained_mode_phase".into()),
        };
        let payload = payload?;
        let operation: Operation = serde_json::from_value(payload)
            .map_err(|_| "invalid_public_retained_mode_operation")?;
        serde_json::to_value(operation).map_err(|error| error.to_string())
    }

    pub(super) fn accept_mode(&mut self, result: Result<Value, String>, now: Instant) {
        let payload = match result {
            Ok(payload) if payload["ok"] == true => payload,
            Ok(payload) => {
                let code = provider_error_code(&payload);
                self.error = Some(code.into());
                if self.step == Step::ModeReconcile && code == "scoped_operation_not_found" {
                    self.schedule(Step::ModeSnapshot, now, Duration::ZERO);
                } else if matches!(code, "topology_changed" | "revision_conflict")
                    && self.step == Step::ModeApply
                {
                    self.schedule(Step::ModeSnapshot, now, RETRY_INTERVAL);
                } else if self.step == Step::ModeApply
                    && matches!(
                        code,
                        "unsupported_mode"
                            | "unsupported_hdr"
                            | "unsupported_primary"
                            | "mode_not_available"
                            | "invalid_request"
                            | "unsupported_topology"
                            | "unsupported_display"
                    )
                {
                    self.mode_failure = Some(code.into());
                    self.schedule(Step::AbortMode, now, Duration::ZERO);
                } else {
                    self.next_poll = now + RETRY_INTERVAL;
                }
                return;
            }
            Err(error) => {
                self.error = Some(error);
                self.next_poll = now + RETRY_INTERVAL;
                return;
            }
        };
        if matches!(
            self.step,
            Step::PrepareMode | Step::CommitMode | Step::AbortMode
        ) && payload["state"] == "pending"
        {
            if payload["operation_id"] != self.mode_operation.as_deref().unwrap_or_default() {
                self.error = Some("retained_mode_pending_identity_mismatch".into());
                self.effect_stalled = true;
            } else {
                self.next_poll = now + RETRY_INTERVAL;
            }
            return;
        }
        match self.step {
            Step::AbortMode => {
                let specification = &payload["specification"];
                let bound = payload["operation_id"]
                    == self.mode_operation.as_deref().unwrap_or_default()
                    && payload["state"] == "aborted"
                    && specification["identity"] == self.identity
                    && specification["name"] == "Remote Monitor"
                    && specification["width"] == self.request.width
                    && specification["height"] == self.request.height
                    && specification["refresh_millihz"] == self.request.refresh_millihz
                    && specification["hdr_enabled"] == self.request.hdr_enabled;
                if !bound {
                    self.error = Some("retained_mode_abort_unconfirmed".into());
                    self.next_poll = now + RETRY_INTERVAL;
                    return;
                }
                self.failed_mode_request = self.desired_request.take();
                self.mode_operation = None;
                self.mode_revision = payload["mode_revision"]
                    .as_u64()
                    .unwrap_or(self.mode_revision);
                self.error = None;
                self.schedule(Step::InspectSource, now, Duration::ZERO);
            }
            Step::ModeReconcile => {
                let expected = format!(
                    "{}-display",
                    self.mode_operation
                        .as_deref()
                        .expect("recovered retained mode")
                );
                if payload["operation_id"] != expected
                    || payload["mode_operation_id"]
                        != self.mode_operation.as_deref().unwrap_or_default()
                {
                    self.error = Some("retained_mode_reconciliation_mismatch".into());
                    self.effect_stalled = true;
                    return;
                }
                match payload["state"].as_str() {
                    Some("succeeded") => self.schedule(Step::CommitMode, now, Duration::ZERO),
                    Some("not_applied") => self.schedule(Step::ModeSnapshot, now, Duration::ZERO),
                    _ => {
                        self.error = Some("retained_mode_reconciliation_unconfirmed".into());
                        self.next_poll = now + RETRY_INTERVAL;
                    }
                }
            }
            Step::PrepareMode => {
                if !self.mode_response_matches(&payload)
                    || !matches!(payload["state"].as_str(), Some("prepared" | "succeeded"))
                {
                    self.error = Some("invalid_retained_mode_prepare".into());
                    self.effect_stalled = true;
                    return;
                }
                self.schedule(
                    if payload["state"] == "succeeded" {
                        Step::CommitMode
                    } else {
                        Step::ModeSnapshot
                    },
                    now,
                    Duration::ZERO,
                );
            }
            Step::ModeSnapshot => {
                let desired = self
                    .desired_request
                    .as_ref()
                    .expect("retained mode snapshot");
                let Some(target_id) = self.display_target.as_deref() else {
                    self.error = Some("monitor_target_unavailable".into());
                    self.effect_stalled = true;
                    return;
                };
                let mode_operation = self
                    .mode_operation
                    .as_deref()
                    .expect("retained mode operation");
                let operation_id = format!("{mode_operation}-display");
                match composition::prepare(
                    composition::TargetRequest {
                        client: &self.client_id,
                        identity: &self.identity,
                        reference: &self.source_reference,
                        target_id,
                        width: desired.width,
                        height: desired.height,
                        refresh_millihz: desired.refresh_millihz,
                        hdr: desired.hdr_enabled,
                        layout: &self.layout_rules,
                        known_targets: &self.known_targets,
                    },
                    payload,
                    &operation_id,
                    false,
                ) {
                    Ok(mut plan) => {
                        if self.peer_captures && composition::translates_origin(&plan.payload) {
                            self.schedule(Step::ModeSnapshot, now, HEALTH_INTERVAL);
                            return;
                        }
                        plan.payload["action"] = json!("scoped_reconfigure");
                        plan.payload["mode_operation_id"] = json!(mode_operation);
                        self.display_payload = match composition::checked_operation(plan.payload) {
                            Ok(payload) => Some(payload),
                            Err(error) => {
                                self.error = Some(error);
                                self.effect_stalled = true;
                                return;
                            }
                        };
                        self.schedule(Step::ModeApply, now, Duration::ZERO);
                    }
                    Err(error) => {
                        self.error = Some(error);
                        self.next_poll = now + RETRY_INTERVAL;
                    }
                }
            }
            Step::ModeApply => {
                if (payload["applied"] != true && payload["reconciled"] != true)
                    || self.display_payload.as_ref().is_none_or(|plan| {
                        payload["operation_id"] != plan["operation_id"]
                            || payload["target_id"] != plan["target"]["id"]
                    })
                {
                    self.error = Some("retained_mode_apply_unconfirmed".into());
                    self.effect_stalled = true;
                    return;
                }
                self.schedule(Step::CommitMode, now, Duration::ZERO);
            }
            Step::CommitMode => {
                if !self.mode_response_matches(&payload) || payload["state"] != "succeeded" {
                    self.error = Some("retained_mode_commit_unconfirmed".into());
                    self.next_poll = now + RETRY_INTERVAL;
                    return;
                }
                self.request = self
                    .desired_request
                    .take()
                    .expect("committed retained request");
                self.mode_operation = None;
                self.scope_reserved = true;
                self.composition_dirty = false;
                self.error = None;
                self.schedule(Step::InspectSource, now, Duration::ZERO);
            }
            _ => unreachable!("retained mode phases only"),
        }
    }

    fn mode_response_matches(&self, payload: &Value) -> bool {
        let Some(desired) = &self.desired_request else {
            return false;
        };
        let Some(operation) = &self.mode_operation else {
            return false;
        };
        let specification = &payload["specification"];
        payload["operation_id"] == *operation
            && specification["identity"] == self.identity
            && specification["width"] == desired.width
            && specification["height"] == desired.height
            && specification["refresh_millihz"] == desired.refresh_millihz
            && specification["hdr_enabled"] == desired.hdr_enabled
            && specification["name"] == "Remote Monitor"
    }
}
