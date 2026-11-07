//! What listen along reads through the session: songs by uri, friend activity (Spotify's
//! `presence-view`, JSON) and a listen-along channel's header (its owner and description).

use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use reqwest::Method;

use super::{Library, build_tracks, convert, fetch, friendly, require_username};
use crate::listen::{self, Activity};
use crate::model::TrackInfo;

/// A listen-along channel as read: its owner's username and its description, as Spotify keeps it.
pub struct Channel {
    pub owner: String,
    pub description: String,
}

impl Library {
    /// One song's details by its uri.
    pub async fn track(&self, uri: &str) -> Result<TrackInfo> {
        self.tracks_by_uri(&[uri.to_owned()]).await?.into_iter().next().ok_or_else(|| anyhow!("Spotify didn't describe that song."))
    }

    /// Songs' details by uri, in the same order (one batched request).
    pub async fn tracks_by_uri(&self, uris: &[String]) -> Result<Vec<TrackInfo>> {
        let session = self.session();
        require_username(&session)?;
        build_tracks(&session, uris, &HashMap::new(), &|_: usize, _: Option<usize>| {})
            .await
            .map_err(|e| friendly("Couldn't look up the song.", e, &session))
    }

    /// Everyone in the user's friend activity who played something.
    pub async fn buddylist(&self) -> Result<Vec<Activity>> {
        let json = self.presence_json("/presence-view/v1/buddylist").await?;
        Ok(listen::parse_buddylist(&json))
    }

    /// One user's friend activity (`None`: Spotify shows nothing for them).
    pub async fn presence(&self, username: &str) -> Result<Option<Activity>> {
        let json = self.presence_json(&format!("/presence-view/v1/user/{username}")).await?;
        Ok(listen::parse_activity(&json))
    }

    async fn presence_json(&self, endpoint: &str) -> Result<serde_json::Value> {
        let session = self.session();
        require_username(&session)?;
        let bytes = fetch::retry("friend activity", || {
            let session = session.clone();
            async move { session.spclient().request(&Method::GET, endpoint, None, None).await }
        })
        .await
        .map_err(|e| friendly("Couldn't read friend activity.", e, &session))?;
        serde_json::from_slice(&bytes).map_err(|_| anyhow!("Spotify's friend activity answer wasn't readable."))
    }

    /// A channel playlist's owner and description.
    pub async fn channel(&self, uri: &str) -> Result<Channel> {
        let Some(id) = convert::playlist_id(uri) else { bail!("That isn't a listen-along link.") };
        let session = self.session();
        require_username(&session)?;
        let header = fetch::playlist_header(&session, &id).await.map_err(|e| friendly("Couldn't read the listen-along link.", e, &session))?;
        Ok(Channel { owner: header.owner_username().to_owned(), description: header.attributes.get_or_default().description().to_owned() })
    }

    /// A user's name and picture (`None`s when Spotify has none).
    pub async fn person(&self, username: &str) -> (Option<String>, Option<String>) {
        let session = self.session();
        match fetch::user_profile(&session, username).await {
            Ok(p) => (p.name, p.image_url),
            Err(e) => {
                log::debug!("profile of {username}: {e}");
                (None, None)
            }
        }
    }
}
