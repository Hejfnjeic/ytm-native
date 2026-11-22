//! Queue editing rules (Play next / Add to queue / Remove), kept free of UI and network code
//! so they can be tested.
//!
//! The queue is: played songs, the current one, then a "manual" section of songs added by hand,
//! then the radio/list songs. `manual_end` is the exclusive end of the manual section.

use std::collections::HashSet;

use crate::yt::Song;

/// Insert `songs` right after `current` (`next`), or after the other hand-added songs.
/// Songs that are already coming up are moved, not duplicated; the current song is left alone
/// (only songs after it move, so `current` stays valid).
pub fn insert(queue: &mut Vec<Song>, current: usize, manual_end: &mut usize, songs: Vec<Song>, next: bool) {
    let current_id = queue[current].id.clone();
    let songs: Vec<Song> = songs.into_iter().filter(|s| s.id != current_id).collect();
    if songs.is_empty() {
        return;
    }
    *manual_end = (*manual_end).max(current + 1).min(queue.len());
    // Take out upcoming copies of the songs being added.
    let ids: HashSet<&str> = songs.iter().map(|s| s.id.as_str()).collect();
    let mut kept = Vec::with_capacity(queue.len() + songs.len());
    for (i, s) in queue.drain(..).enumerate() {
        if i > current && ids.contains(s.id.as_str()) {
            if i < *manual_end {
                *manual_end -= 1;
            }
        } else {
            kept.push(s);
        }
    }
    *queue = kept;
    let n = songs.len();
    let at = if next { current + 1 } else { *manual_end };
    queue.splice(at..at, songs);
    *manual_end += n;
}

/// Remove the song at `i` (never the current one). Returns the new `current`.
pub fn remove(queue: &mut Vec<Song>, current: usize, manual_end: &mut usize, i: usize) -> usize {
    if i == current || i >= queue.len() {
        return current;
    }
    queue.remove(i);
    if i < *manual_end {
        *manual_end -= 1;
    }
    if i < current { current - 1 } else { current }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str) -> Song {
        Song { id: id.into(), title: format!("Title {id}"), artist: "Someone".into(), duration: "3:00".into(), ..Default::default() }
    }
    fn q(ids: &[&str]) -> Vec<Song> {
        ids.iter().map(|i| song(i)).collect()
    }
    fn ids(queue: &[Song]) -> Vec<&str> {
        queue.iter().map(|s| s.id.as_str()).collect()
    }

    #[test]
    fn play_next_goes_right_after_current() {
        // played a, current b, radio c d
        let mut queue = q(&["a", "b", "c", "d"]);
        let mut end = 2;
        insert(&mut queue, 1, &mut end, q(&["x"]), true);
        assert_eq!(ids(&queue), ["a", "b", "x", "c", "d"]);
        assert_eq!(end, 3);
        insert(&mut queue, 1, &mut end, q(&["y"]), true);
        assert_eq!(ids(&queue), ["a", "b", "y", "x", "c", "d"]);
        assert_eq!(end, 4);
    }

    #[test]
    fn add_to_queue_goes_after_added_songs_before_radio() {
        let mut queue = q(&["a", "b", "c", "d"]);
        let mut end = 2;
        insert(&mut queue, 1, &mut end, q(&["x"]), false);
        insert(&mut queue, 1, &mut end, q(&["y"]), false);
        assert_eq!(ids(&queue), ["a", "b", "x", "y", "c", "d"]);
        // Play next still jumps ahead of them.
        insert(&mut queue, 1, &mut end, q(&["z"]), true);
        assert_eq!(ids(&queue), ["a", "b", "z", "x", "y", "c", "d"]);
        assert_eq!(end, 5);
    }

    #[test]
    fn upcoming_song_is_moved_not_duplicated() {
        let mut queue = q(&["a", "b", "c", "d", "e"]);
        let mut end = 2;
        insert(&mut queue, 1, &mut end, q(&["e"]), true);
        assert_eq!(ids(&queue), ["a", "b", "e", "c", "d"]);
        // Moving a hand-added song to the end of the hand-added part.
        insert(&mut queue, 1, &mut end, q(&["x"]), false);
        insert(&mut queue, 1, &mut end, q(&["e"]), false);
        assert_eq!(ids(&queue), ["a", "b", "x", "e", "c", "d"]);
        assert_eq!(end, 4);
    }

    #[test]
    fn played_song_is_added_again_and_current_is_ignored() {
        let mut queue = q(&["a", "b", "c"]);
        let mut end = 2;
        insert(&mut queue, 1, &mut end, q(&["a", "b"]), true);
        assert_eq!(ids(&queue), ["a", "b", "a", "c"]);
        assert_eq!(end, 3);
    }

    #[test]
    fn several_songs_keep_their_order() {
        let mut queue = q(&["a", "b"]);
        let mut end = 1;
        insert(&mut queue, 0, &mut end, q(&["x", "y", "z"]), false);
        assert_eq!(ids(&queue), ["a", "x", "y", "z", "b"]);
        assert_eq!(end, 4);
    }

    #[test]
    fn at_the_end_of_the_queue() {
        let mut queue = q(&["a"]);
        let mut end = 1;
        insert(&mut queue, 0, &mut end, q(&["x"]), false);
        assert_eq!(ids(&queue), ["a", "x"]);
        // A stale manual_end past the queue's end is clamped.
        let mut end = 9;
        insert(&mut queue, 0, &mut end, q(&["y"]), false);
        assert_eq!(ids(&queue), ["a", "x", "y"]);
        assert_eq!(end, 3);
    }

    #[test]
    fn remove_keeps_current_and_manual_end_in_place() {
        let mut queue = q(&["a", "b", "x", "y", "c"]);
        let mut end = 4; // x y added by hand
        let cur = remove(&mut queue, 1, &mut end, 2);
        assert_eq!((ids(&queue), cur, end), (vec!["a", "b", "y", "c"], 1, 3));
        let cur = remove(&mut queue, cur, &mut end, 0);
        assert_eq!((ids(&queue), cur, end), (vec!["b", "y", "c"], 0, 2));
        let cur = remove(&mut queue, cur, &mut end, 0); // the current song: refused
        assert_eq!((ids(&queue), cur, end), (vec!["b", "y", "c"], 0, 2));
        let cur = remove(&mut queue, cur, &mut end, 2);
        assert_eq!((ids(&queue), cur, end), (vec!["b", "y"], 0, 2));
    }
}
