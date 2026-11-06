//! Bounded per-client resolution of previously advertised running-game tiles.
use crate::identity::{Control, identify, occupied};
use amaterasu_module_sdk::session_presentation::{Application, Selection};
use std::collections::{BTreeMap, VecDeque};

const MAX_CLIENTS: usize = 32;
const MAX_TILES_PER_CLIENT: usize = 1024;

#[derive(Clone)]
struct Tile {
    id: String,
    target: Application,
}

#[derive(Default)]
pub(crate) struct TileCache {
    clients: BTreeMap<String, VecDeque<Tile>>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resolution {
    Unknown,
    Target(String),
    Unavailable,
}

impl TileCache {
    pub(crate) fn remember(
        &mut self,
        client: &str,
        tile: &Application,
        target: &Application,
    ) -> Result<(), &'static str> {
        if !self.clients.contains_key(client) && self.clients.len() == MAX_CLIENTS {
            return Err("client_capacity_exhausted");
        }
        let mut target = target.clone();
        target.properties = serde_json::json!({});
        target.aliases.clear();
        let entries = self.clients.entry(client.to_owned()).or_default();
        if let Some(previous) = entries
            .iter_mut()
            .find(|previous| previous.id == tile.id && same_target(&previous.target, &target))
        {
            previous.target = target.clone();
            return Ok(());
        }
        if entries.len() == MAX_TILES_PER_CLIENT {
            entries.pop_front();
        }
        entries.push_back(Tile {
            id: tile.id.clone(),
            target: target.clone(),
        });
        Ok(())
    }

    pub(crate) fn resolve(
        &self,
        client: &str,
        selection: &Selection,
        applications: &[Application],
    ) -> Resolution {
        let explicit_running = selection
            .uuid
            .as_deref()
            .is_some_and(|uuid| uuid.eq_ignore_ascii_case(&Control::RunningGame.uuid()));
        if selection
            .uuid
            .as_deref()
            .is_some_and(|uuid| !uuid.is_empty())
            && !explicit_running
        {
            return Resolution::Unknown;
        }
        if let Some(control) = identify(selection)
            && control != Control::RunningGame
        {
            return Resolution::Unknown;
        }
        if !selection.id.is_empty() && occupied(&selection.id, applications) {
            return if explicit_running {
                Resolution::Unavailable
            } else {
                Resolution::Unknown
            };
        }
        let Some(entries) = self.clients.get(client) else {
            return Resolution::Unknown;
        };
        let mut matches = entries
            .iter()
            .filter(|entry| selection.id.is_empty() || entry.id == selection.id);
        let Some(first) = matches.next() else {
            return Resolution::Unknown;
        };
        if matches.any(|entry| !same_target(&entry.target, &first.target)) {
            return Resolution::Unavailable;
        }
        let mut current = applications.iter().filter(|application| {
            same_target(application, &first.target) && !application.monitor_profile
        });
        match (current.next(), current.next()) {
            (Some(target), None) => Resolution::Target(target.id.clone()),
            _ => Resolution::Unavailable,
        }
    }

    pub(crate) fn forget(&mut self, client: &str) {
        self.clients.remove(client);
    }
}

fn same_target(left: &Application, right: &Application) -> bool {
    if !left.uuid.is_empty() {
        left.uuid == right.uuid
    } else {
        right.uuid.is_empty() && left.id == right.id && left.title == right.title
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, uuid: &str) -> Application {
        Application {
            id: id.into(),
            uuid: uuid.into(),
            title: "Game".into(),
            visible: true,
            aliases: Vec::new(),
            monitor_profile: false,
            properties: serde_json::json!({}),
        }
    }

    #[test]
    fn cached_tiles_survive_identity_changes_without_shadowing_new_configured_ids() {
        let mut cache = TileCache::default();
        let game = app("42", "stable-game");
        let tile = app("1000", &Control::RunningGame.uuid());
        cache.remember("client", &tile, &game).unwrap();
        let mut updated = game.clone();
        updated.id = "55".into();
        updated.title = "New cover name".into();
        let request = Selection {
            id: tile.id.clone(),
            uuid: None,
        };
        assert_eq!(
            cache.resolve("client", &request, &[updated.clone()]),
            Resolution::Target("55".into())
        );
        assert_eq!(
            cache.resolve("other", &request, &[updated.clone()]),
            Resolution::Unknown
        );
        let occupied = app("1000", "different");
        assert_eq!(
            cache.resolve("client", &request, &[updated.clone(), occupied.clone()]),
            Resolution::Unknown
        );
        assert_eq!(
            cache.resolve(
                "client",
                &Selection {
                    id: tile.id,
                    uuid: Some(Control::RunningGame.uuid())
                },
                &[updated, occupied]
            ),
            Resolution::Unavailable
        );
    }

    #[test]
    fn deleted_or_ambiguous_cached_targets_never_retarget() {
        let mut cache = TileCache::default();
        let tile = app("1000", &Control::RunningGame.uuid());
        let game = app("42", "game");
        cache.remember("client", &tile, &game).unwrap();
        let request = Selection {
            id: "1000".into(),
            uuid: None,
        };
        assert_eq!(
            cache.resolve("client", &request, &[]),
            Resolution::Unavailable
        );
        assert_eq!(
            cache.resolve("client", &request, &[game.clone(), game]),
            Resolution::Unavailable
        );
    }
}
