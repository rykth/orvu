//! Where a team's simple notes are kept, and moving them there.
//!
//! A team's simple notes are kept in a map that keeps values written at once
//! (`orvu/<team>/notes-mv`), so that two agents writing one note at the same
//! time both keep what they wrote until somebody settles it. Each value is the
//! writer's name, a separator, and the note's text; a program that gave no
//! name writes an empty one.
//!
//! Notes used to be kept in a plain map (`orvu/<team>/notes`), with who wrote
//! each one in a second map (`orvu/<team>/notes-by`). The first time a team's
//! notes are used, the sidecar moves the old ones over, each with the name of
//! the agent that wrote it. The sidecar does this, and not the programs,
//! because an agent may only ever write its own name on a note. The old maps
//! are left as they are, and nothing uses them after that.
//!
//! When it is done, the move writes a value under the empty name, which no
//! note can have, so that it is not done again. Two machines that both move
//! the same notes write the same values, which read as one.

use orvu_sdk::{Client, SdkError};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Mutex;

/// What comes between the writer's name and the note's text.
pub const SEPARATOR: char = '\u{1f}';

/// The name the move writes under when it is done. No note can have it.
pub const MOVED: &str = "";

/// The object a team's simple notes are kept in.
pub fn object(team: &str) -> String {
    format!("orvu/{team}/notes-mv")
}

/// The team whose notes `object` is, if it is a team's notes.
pub fn team_of(object: &str) -> Option<&str> {
    crate::rules::team_and_rest(object).and_then(|(team, rest)| (rest == "notes-mv").then_some(team))
}

/// A value as it is kept: the writer's name and the note's text.
pub fn kept(by: &str, text: &str) -> String {
    format!("{by}{SEPARATOR}{text}")
}

/// The writer's name and the note's text of a value as it is kept. A value
/// with no separator is all text.
pub fn split(value: &str) -> (&str, &str) {
    value.split_once(SEPARATOR).unwrap_or(("", value))
}

/// Moves teams' old notes over, once each.
#[derive(Default)]
pub struct Moves {
    /// Teams whose notes are known to have been moved, or to need no move.
    done: Mutex<HashSet<String>>,
    /// Held while a move runs, so that a request waits for it, and two moves
    /// of one team do not run side by side.
    moving: tokio::sync::Mutex<()>,
}

impl Moves {
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves the old notes over first, if `object` is a team's notes and its
    /// notes have not been moved yet. Says why if the move failed, and then
    /// the request has to be turned away: changing the notes before the move
    /// is whole could undo the change when it is tried again.
    pub async fn before(&self, client: &Client, object: &str) -> Result<(), String> {
        match team_of(object) {
            Some(team) => self.team(client, team).await,
            None => Ok(()),
        }
    }

    /// Moves `team`'s old notes over, if they have not been moved yet.
    pub async fn team(&self, client: &Client, team: &str) -> Result<(), String> {
        if self.done.lock().unwrap_or_else(|e| e.into_inner()).contains(team) {
            return Ok(());
        }
        let _moving = self.moving.lock().await;
        if self.done.lock().unwrap_or_else(|e| e.into_inner()).contains(team) {
            return Ok(());
        }
        match move_over(client, team).await {
            Ok(true) => {
                self.done.lock().unwrap_or_else(|e| e.into_inner()).insert(team.to_string());
                Ok(())
            }
            // No old notes to move yet. They could still come from another
            // machine, so this is looked at again next time.
            Ok(false) => Ok(()),
            Err(e) => {
                tracing::warn!(team, "the team's old notes could not be moved over: {e}");
                Err(format!(
                    "the team's notes kept the old way could not be moved over, and its notes cannot be used until they are: {e}"
                ))
            }
        }
    }
}

/// Moves `team`'s old notes over, unless that was done, and marks it done.
/// True if it is done now; false if there are no old notes, in which case
/// nothing was written. A team with no old notes is not marked: another
/// machine may hold some it has not sent yet, and the mark, once there,
/// would keep them from being moved over. No agent with rules may write the
/// old maps, so what comes there later is what was there before.
async fn move_over(client: &Client, team: &str) -> Result<bool, SdkError> {
    let new = object(team);
    if client.exists(&new).await {
        match client.mv_map(&new).await {
            Ok(notes) if !notes.get(MOVED).await?.is_empty() => return Ok(true),
            Ok(_) => {}
            // Something else under the name. Nothing can be moved into it.
            Err(SdkError::TypeMismatch { .. }) => return Ok(true),
            Err(e) => return Err(e),
        }
    }
    let old = plain(client, &format!("orvu/{team}/notes")).await?;
    if old.is_empty() {
        return Ok(false);
    }
    let by = plain(client, &format!("orvu/{team}/notes-by")).await?;
    let notes = client.mv_map(&new).await?;
    // A note written since keeps what was written, and the old one is not
    // put beside it.
    let have: BTreeSet<String> = notes.keys().await?.into_iter().collect();
    for (name, text) in old {
        if name != MOVED && !have.contains(&name) {
            let by = by.get(&name).map(String::as_str).unwrap_or("");
            notes.put(&name, &kept(by, &text)).await?;
        }
    }
    // The team's list says what it has, and search finds notes by it. It is
    // written before the mark, so that a move that fails here is tried again.
    client.map(&format!("orvu/{team}/index")).await?.put("notes-mv", "mvmap").await?;
    notes.put(MOVED, &kept("", "moved")).await?;
    Ok(true)
}

/// What the plain map `object` holds, without making it if it is not there.
/// Something else under the name holds nothing.
async fn plain(client: &Client, object: &str) -> Result<BTreeMap<String, String>, SdkError> {
    if !client.exists(object).await {
        return Ok(BTreeMap::new());
    }
    match client.map(object).await {
        Ok(map) => Ok(map.entries().await?.into_iter().collect()),
        Err(SdkError::TypeMismatch { .. }) => Ok(BTreeMap::new()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_teams_notes_are_known_by_their_name() {
        assert_eq!(team_of("orvu/crew/notes-mv"), Some("crew"));
        for other in ["orvu/crew/notes", "orvu//notes-mv", "orvu/a/b/notes-mv", "crew/notes-mv", "orvu/crew/notes-mv/x"] {
            assert_eq!(team_of(other), None, "{other}");
        }
    }

    #[test]
    fn a_value_is_its_writer_and_its_text() {
        assert_eq!(split(&kept("alice", "Friday")), ("alice", "Friday"));
        assert_eq!(split(&kept("", "Friday")), ("", "Friday"));
        // The text may hold the separator; the name cannot.
        assert_eq!(split(&kept("alice", "a\u{1f}b")), ("alice", "a\u{1f}b"));
        assert_eq!(split("no name"), ("", "no name"));
    }
}
