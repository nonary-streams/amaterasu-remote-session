//! Permission-checked redirects and original control artwork.
use super::*;

impl SessionPolicy {
    pub(super) fn artwork(&self, caller: &str, context: &Context, selection: &Selection) -> Reply {
        if !context.client.paired || !context.client.may_list {
            return error("forbidden", "Artwork permission denied");
        }
        let applications = self.projections.catalog(caller);
        let applications = applications.as_deref().map(Vec::as_slice).unwrap_or(&[]);
        match self
            .cache
            .resolve(&context.client.id, selection, applications)
        {
            Resolution::Target(application_id) => {
                return Reply::Artwork {
                    artwork: Artwork::Application { application_id },
                };
            }
            Resolution::Unavailable => {
                return Reply::Artwork {
                    artwork: Artwork::Unavailable,
                };
            }
            Resolution::Unknown => {}
        }
        if let Some(control) = identify(selection) {
            if control == Control::RunningGame {
                if let Some(game) = &context.game {
                    return Reply::Artwork {
                        artwork: Artwork::Application {
                            application_id: game.application.id.clone(),
                        },
                    };
                }
                return Reply::Artwork {
                    artwork: Artwork::Unavailable,
                };
            }
            return Reply::Artwork {
                artwork: Artwork::InlinePng {
                    bytes: crate::artwork::png(control).to_vec(),
                },
            };
        }
        let artwork = applications
            .iter()
            .find(|application| {
                application.id == selection.id || application.aliases.contains(&selection.id)
            })
            .map_or(Artwork::Unavailable, |application| Artwork::Application {
                application_id: application.id.clone(),
            });
        Reply::Artwork { artwork }
    }
}
