//! Bounded, caller-bound catalog projection transactions.
use crate::{
    cache::TileCache,
    projection::{Role, project},
};
use amaterasu_module_sdk::session_presentation::{
    Application, Context, MAX_CATALOG_ENTRIES, MAX_PAGE_ENTRIES, MAX_PAYLOAD_BYTES,
    MAX_PROJECTIONS, PROJECTION_TIMEOUT_MS, Presentation, Reply,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, Instant},
};

const MAX_CATALOG_BYTES: usize = 4 * 1024 * 1024;

struct Projection {
    caller: String,
    context: Context,
    revision: String,
    deadline: Instant,
    byte_count: usize,
    source: Vec<Application>,
    result: Option<(Presentation, Vec<Application>)>,
}

pub(crate) struct ProjectionStore {
    nonce: Option<[u8; 32]>,
    sequence: u64,
    active: BTreeMap<String, Projection>,
    catalogs: BTreeMap<String, (String, Arc<Vec<Application>>)>,
}

impl Default for ProjectionStore {
    fn default() -> Self {
        let mut nonce = [0_u8; 32];
        let nonce = getrandom::fill(&mut nonce).ok().map(|()| nonce);
        Self {
            nonce,
            sequence: 0,
            active: BTreeMap::new(),
            catalogs: BTreeMap::new(),
        }
    }
}

impl ProjectionStore {
    pub(crate) fn expire(&mut self, now: Instant) {
        self.active
            .retain(|_, projection| now < projection.deadline);
    }

    pub(crate) fn begin(
        &mut self,
        caller: &str,
        context: Context,
        revision: String,
        now: Instant,
    ) -> Reply {
        self.expire(now);
        let Some(nonce) = self.nonce else {
            return error(
                "projection_entropy_unavailable",
                "Application list identity cannot be prepared safely",
            );
        };
        if !context.client.paired {
            return error("forbidden", "Application list permission denied");
        }
        if self.active.len() >= MAX_PROJECTIONS {
            return error(
                "projection_capacity_exhausted",
                "Too many application lists are being prepared",
            );
        }
        let Some(sequence) = self.sequence.checked_add(1) else {
            return error(
                "projection_id_exhausted",
                "Application list identity is exhausted",
            );
        };
        self.sequence = sequence;
        let mut digest = Sha256::new();
        digest.update(b"remote-session-projection-v1\0");
        digest.update(nonce);
        digest.update(caller.as_bytes());
        digest.update([0]);
        digest.update(context.client.id.as_bytes());
        digest.update(sequence.to_le_bytes());
        let id = format!("projection-{:x}", digest.finalize());
        self.active.insert(
            id.clone(),
            Projection {
                caller: caller.into(),
                context,
                revision,
                deadline: now + Duration::from_millis(PROJECTION_TIMEOUT_MS),
                byte_count: 0,
                source: Vec::new(),
                result: None,
            },
        );
        Reply::Started { projection_id: id }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn append(
        &mut self,
        caller: &str,
        client: &str,
        id: &str,
        entries: Vec<Application>,
        final_page: bool,
        role: Option<Role>,
        show_input: bool,
        show_monitor: bool,
        cache: &mut TileCache,
    ) -> Reply {
        let Some(projection) = self.active.get_mut(id) else {
            return error("projection_not_found", "Application list is unavailable");
        };
        if projection.caller != caller || projection.context.client.id != client {
            return error("forbidden", "Application list belongs to another client");
        }
        if projection.result.is_some() {
            return error(
                "projection_complete",
                "Application list is already complete",
            );
        }
        if entries.len() > MAX_PAGE_ENTRIES || (!final_page && entries.is_empty()) {
            return error(
                "invalid_projection_page",
                "Application list page did not advance",
            );
        }
        let bytes = match serde_json::to_vec(&entries) {
            Ok(bytes) => bytes.len(),
            Err(_) => {
                return error(
                    "invalid_projection_page",
                    "Application list page could not be encoded",
                );
            }
        };
        if projection.source.len() + entries.len() > MAX_CATALOG_ENTRIES
            || projection.byte_count + bytes > MAX_CATALOG_BYTES
        {
            return error(
                "catalog_capacity_exhausted",
                "Application list exceeds the retained catalog bound",
            );
        }
        projection.byte_count += bytes;
        projection.source.extend(entries);
        if !final_page {
            return Reply::Appended { complete: false };
        }
        let mut occupied = BTreeSet::new();
        if projection
            .source
            .iter()
            .any(|entry| !occupied.insert(entry.id.as_str()))
        {
            return error(
                "catalog_identity_conflict",
                "Application list has duplicate configured identities",
            );
        }
        let projected = match project(
            &projection.context,
            &projection.source,
            role,
            show_input,
            show_monitor,
        ) {
            Ok(projected) => projected,
            Err(code) => return error(code, "Application list cannot be projected safely"),
        };
        if let Some(tile) = &projected.running_tile
            && let Some(game) = &projection.context.game
            && let Err(code) = cache.remember(client, tile, &game.application)
        {
            return error(code, "Cached application tile capacity is exhausted");
        }
        // A trusted protocol adapter replaces one complete inventory atomically.
        // No partial catalog can participate in launch or collision decisions.
        let complete = Arc::new(std::mem::take(&mut projection.source));
        self.catalogs
            .insert(caller.into(), (projection.revision.clone(), complete));
        projection.result = Some((projected.presentation, projected.entries));
        Reply::Appended { complete: true }
    }

    pub(crate) fn read(
        &self,
        caller: &str,
        client: &str,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> Reply {
        let Some(projection) = self.active.get(id) else {
            return error("projection_not_found", "Application list is unavailable");
        };
        if projection.caller != caller || projection.context.client.id != client {
            return error("forbidden", "Application list belongs to another client");
        }
        if !projection.context.client.may_list {
            return error("forbidden", "Application list permission denied");
        }
        let Some((presentation, entries)) = &projection.result else {
            return error(
                "projection_incomplete",
                "Complete catalog inventory is required",
            );
        };
        if offset > entries.len() || !(1..=MAX_PAGE_ENTRIES).contains(&limit) {
            return error("invalid_projection_read", "Invalid application list range");
        }
        let maximum = (offset + limit).min(entries.len());
        let mut end = offset;
        while end < maximum {
            let candidate = Reply::Projected {
                presentation: presentation.clone(),
                entries: entries[offset..=end].to_vec(),
                next_offset: (end + 1 < entries.len()).then_some(end + 1),
            };
            if !serde_json::to_vec(&candidate)
                .is_ok_and(|encoded| encoded.len() <= MAX_PAYLOAD_BYTES)
            {
                break;
            }
            end += 1;
        }
        if end == offset && offset < entries.len() {
            return error(
                "projection_entry_too_large",
                "Application list entry exceeds the reply bound",
            );
        }
        Reply::Projected {
            presentation: presentation.clone(),
            entries: entries[offset..end].to_vec(),
            next_offset: (end < entries.len()).then_some(end),
        }
    }

    pub(crate) fn release(&mut self, caller: &str, client: &str, id: &str) -> Reply {
        if let Some(projection) = self.active.get(id)
            && (projection.caller != caller || projection.context.client.id != client)
        {
            return error("forbidden", "Application list belongs to another client");
        }
        self.active.remove(id);
        Reply::Acknowledged {}
    }

    pub(crate) fn catalog(&self, caller: &str) -> Option<Arc<Vec<Application>>> {
        self.catalogs
            .get(caller)
            .map(|(_, entries)| Arc::clone(entries))
    }

    pub(crate) fn forget(&mut self, client: &str) {
        self.active
            .retain(|_, projection| projection.context.client.id != client);
    }
}

pub(crate) fn error(code: &str, message: &str) -> Reply {
    Reply::Error {
        code: code.into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amaterasu_module_sdk::session_presentation::Client;

    fn context(client: &str) -> Context {
        Context {
            client: Client {
                id: client.into(),
                paired: true,
                may_list: true,
                may_view: true,
                may_launch: true,
                may_terminate: true,
            },
            game: None,
        }
    }
    fn entry(id: &str) -> Application {
        Application {
            id: id.into(),
            uuid: format!("uuid-{id}"),
            title: "Game".into(),
            aliases: vec![],
            visible: true,
            monitor_profile: false,
            properties: serde_json::json!({}),
        }
    }

    #[test]
    fn projection_is_bound_to_broker_caller_client_and_complete_inventory() {
        let mut store = ProjectionStore::default();
        let mut cache = TileCache::default();
        let now = Instant::now();
        let Reply::Started { projection_id } =
            store.begin("adapter", context("one"), "7".into(), now)
        else {
            panic!("begin")
        };
        assert!(
            matches!(store.append("other","one",&projection_id,vec![],true,None,true,true,&mut cache),Reply::Error{code,..} if code=="forbidden")
        );
        assert!(
            matches!(store.read("adapter","two",&projection_id,0,32),Reply::Error{code,..} if code=="forbidden")
        );
        assert!(store.catalog("adapter").is_none());
        assert!(matches!(
            store.append(
                "adapter",
                "one",
                &projection_id,
                vec![entry("42")],
                false,
                None,
                true,
                true,
                &mut cache
            ),
            Reply::Appended { complete: false }
        ));
        assert!(store.catalog("adapter").is_none());
        assert!(matches!(
            store.append(
                "adapter",
                "one",
                &projection_id,
                vec![],
                true,
                None,
                true,
                true,
                &mut cache
            ),
            Reply::Appended { complete: true }
        ));
        assert_eq!(store.catalog("adapter").unwrap().len(), 1);
        assert!(matches!(
            store.release("adapter", "two", &projection_id),
            Reply::Error { .. }
        ));
        assert!(matches!(
            store.release("adapter", "one", &projection_id),
            Reply::Acknowledged {}
        ));
        assert!(matches!(
            store.release("adapter", "one", &projection_id),
            Reply::Acknowledged {}
        ));
    }

    #[test]
    fn abandoned_projections_expire_without_changing_current_catalog() {
        let mut store = ProjectionStore::default();
        let now = Instant::now();
        for _ in 0..MAX_PROJECTIONS {
            assert!(matches!(
                store.begin("adapter", context("one"), "7".into(), now),
                Reply::Started { .. }
            ));
        }
        assert!(
            matches!(store.begin("adapter",context("one"),"7".into(),now),Reply::Error{code,..} if code=="projection_capacity_exhausted")
        );
        store.expire(now + Duration::from_millis(PROJECTION_TIMEOUT_MS));
        assert!(matches!(
            store.begin("adapter", context("one"), "7".into(), now),
            Reply::Started { .. }
        ));
        assert!(store.catalog("adapter").is_none());
    }
    #[test]
    fn projection_tokens_do_not_repeat_when_a_provider_restarts() {
        let mut first = ProjectionStore::default();
        let mut second = ProjectionStore::default();
        let now = Instant::now();
        let Reply::Started {
            projection_id: first,
        } = first.begin("adapter", context("one"), "7".into(), now)
        else {
            panic!("first")
        };
        let Reply::Started {
            projection_id: second,
        } = second.begin("adapter", context("one"), "7".into(), now)
        else {
            panic!("second")
        };
        assert_ne!(first, second);
    }
}
