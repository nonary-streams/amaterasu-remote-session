//! Read-only references to an explicitly selected pre-existing capture target.
use super::*;
use amaterasu_module_sdk::display::targets::{self, Binding, Operation as TargetOperation};

#[derive(Clone)]
pub(super) struct ExistingTarget {
    pub selector: String,
    pub role_owned: bool,
    pub binding: Option<Binding>,
    pub preflight_ready: bool,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Selection {
    Owned,
    Existing { selector: String },
}

pub(super) fn selection(settings: &Value) -> Result<Option<ExistingTarget>, String> {
    let Some(value) = settings.get("monitor_target") else {
        return Ok(None);
    };
    match serde_json::from_value::<Selection>(value.clone())
        .map_err(|_| "invalid_monitor_target")?
    {
        Selection::Owned => Ok(None),
        Selection::Existing { selector } => {
            if selector.is_empty()
                || selector.len() > 256
                || !selector.bytes().all(|byte| byte.is_ascii_graphic())
            {
                return Err("invalid_monitor_target".into());
            }
            Ok(Some(ExistingTarget {
                selector,
                role_owned: false,
                binding: None,
                preflight_ready: false,
            }))
        }
    }
}

impl Monitor {
    fn existing_preflight_request(
        &self,
    ) -> Result<amaterasu_module_sdk::capture::SourcePreflightRequest, String> {
        let binding = self
            .existing
            .as_ref()
            .and_then(|target| target.binding.as_ref())
            .ok_or("capture_target_unresolved")?;
        Ok(amaterasu_module_sdk::capture::SourcePreflightRequest {
            source_class: amaterasu_module_sdk::capture::SourceClass::DesktopOutput,
            route: Some(binding.output.clone()),
            mode: amaterasu_module_sdk::capture::SourceMode {
                width: binding.mode.width,
                height: binding.mode.height,
                refresh_millihz: binding.mode.refresh_millihz,
                hdr: binding.mode.hdr_enabled,
                bit_depth: None,
            },
        })
    }

    pub(super) fn existing_preflight_payload(&self) -> Result<Value, String> {
        let binding = self
            .existing
            .as_ref()
            .and_then(|target| target.binding.as_ref())
            .ok_or("capture_target_unresolved")?;
        amaterasu_module_sdk::capture::source_preflight_payload(
            &binding.route_key,
            &self.existing_preflight_request()?,
        )
    }

    pub(super) fn existing_operation(&self) -> Result<Value, String> {
        let target = self
            .existing
            .as_ref()
            .ok_or("existing_target_unavailable")?;
        let reference = if self.source_step() {
            &self.source_reference
        } else {
            &self.reference
        };
        let operation_id = format!(
            "remote-existing-{}-{}-{:?}",
            self.identity, self.generation, self.step
        );
        match self.step {
            Step::Create | Step::CreateSource => TargetOperation::BorrowTarget {
                operation_id,
                selector: target.selector.clone(),
                reference_id: reference.clone(),
            },
            Step::Remove | Step::RemoveSource => TargetOperation::ReleaseTarget {
                operation_id,
                selector: target.selector.clone(),
                reference_id: reference.clone(),
            },
            _ => TargetOperation::ResolveCaptureTarget {
                selector: target.selector.clone(),
            },
        }
        .payload()
    }

    pub(super) fn accept_existing(&mut self, result: Result<Value, String>, now: Instant) {
        let payload = match result {
            Ok(payload) if payload["ok"] == true => payload,
            Ok(payload) => {
                self.existing_failure(provider_error_code(&payload), now);
                return;
            }
            Err(error) => {
                self.existing_failure(&error, now);
                return;
            }
        };
        let target = self.existing.as_ref().expect("existing target acceptance");
        if self.step == Step::ExistingPreflight {
            let response = self.existing_preflight_request().and_then(|request| {
                amaterasu_module_sdk::capture::decode_video_source_preflight(&payload, &request)
            });
            match response {
                Ok(response) if response.is_ready() => {
                    self.existing
                        .as_mut()
                        .expect("existing target")
                        .preflight_ready = true;
                    self.error = None;
                    self.schedule(Step::Create, now, Duration::ZERO);
                }
                Ok(response) => {
                    self.effect_stalled = response.state
                        == amaterasu_module_sdk::capture::SourceReadiness::Unsupported;
                    self.existing_failure(
                        response
                            .reason
                            .as_deref()
                            .unwrap_or("capture_source_unavailable"),
                        now,
                    );
                }
                Err(error) => self.existing_failure(&error, now),
            }
            return;
        }
        let selector = target.selector.clone();
        let reference = if self.source_step() {
            self.source_reference.clone()
        } else {
            self.reference.clone()
        };
        match self.step {
            Step::Remove | Step::RemoveSource => {
                if !targets::released_from_reply(&payload, &selector, &reference) {
                    self.existing_failure("capture_reference_release_unconfirmed", now);
                    return;
                }
                if self.step == Step::RemoveSource {
                    self.source_reference_owned = false;
                    self.source_reference_released = true;
                    self.source_ready = false;
                    self.output = None;
                    self.scope_reserved = false;
                    self.error = None;
                } else {
                    self.reference_released = true;
                    self.existing.as_mut().expect("existing target").role_owned = false;
                    self.schedule(
                        if self.captures.is_empty() {
                            Step::RemoveSource
                        } else {
                            Step::InspectSource
                        },
                        now,
                        Duration::ZERO,
                    );
                }
            }
            Step::Create | Step::CreateSource => {
                let binding = match targets::borrowed_from_reply(payload, &selector, &reference) {
                    Ok(binding) => binding,
                    Err(error) => {
                        self.existing_failure(&error, now);
                        return;
                    }
                };
                if !self.accept_existing_binding(binding, now) {
                    return;
                }
                if self.step == Step::Create {
                    self.existing.as_mut().expect("existing target").role_owned = true;
                    self.schedule(Step::CreateSource, now, Duration::ZERO);
                } else {
                    self.source_reference_owned = true;
                    self.source_reference_released = false;
                    self.ready = !self.release_requested;
                    self.source_ready = true;
                    self.scope_reserved = true;
                    self.composition_dirty = false;
                    self.error = None;
                    self.schedule(
                        if self.release_requested {
                            Step::Remove
                        } else {
                            Step::InspectSource
                        },
                        now,
                        HEALTH_INTERVAL,
                    );
                }
            }
            _ => {
                let binding = match targets::resolved_from_reply(payload, &selector) {
                    Ok(binding) => binding,
                    Err(error) => {
                        self.existing_failure(&error, now);
                        return;
                    }
                };
                if !self.accept_existing_binding(binding, now) {
                    return;
                }
                if self.release_requested {
                    if !self.reference_released {
                        self.schedule(Step::Remove, now, Duration::ZERO);
                    } else if self.captures.is_empty() {
                        self.schedule(Step::RemoveSource, now, Duration::ZERO);
                    } else {
                        self.source_ready = self.source_reference_owned;
                        self.schedule(Step::InspectSource, now, HEALTH_INTERVAL);
                    }
                } else if !self
                    .existing
                    .as_ref()
                    .expect("existing target")
                    .preflight_ready
                {
                    self.schedule(Step::ExistingPreflight, now, Duration::ZERO);
                } else if !self.existing.as_ref().expect("existing target").role_owned {
                    self.schedule(Step::Create, now, Duration::ZERO);
                } else if !self.source_reference_owned {
                    self.schedule(Step::CreateSource, now, Duration::ZERO);
                } else {
                    self.ready = true;
                    self.source_ready = true;
                    self.error = None;
                    self.schedule(Step::InspectSource, now, HEALTH_INTERVAL);
                }
            }
        }
    }

    fn accept_existing_binding(&mut self, binding: Binding, now: Instant) -> bool {
        let target = self.existing.as_mut().expect("existing target binding");
        if target
            .binding
            .as_ref()
            .is_some_and(|frozen| frozen != &binding)
        {
            self.existing_failure("capture_target_binding_changed", now);
            return false;
        }
        self.output = Some(binding.output.clone());
        target.binding = Some(binding);
        true
    }

    fn existing_failure(&mut self, error: &str, now: Instant) {
        self.ready = false;
        self.source_ready = false;
        self.output = None;
        self.error = Some(error.into());
        self.next_poll = now + RETRY_INTERVAL;
    }
}
