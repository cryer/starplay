//! Library filtering keeps stable indices into the playback library.
use crate::library::Track;

#[derive(Default)]
pub struct Browser {
    pub query: String,
    pub editing: bool,
    pub queue: bool,
    pub cursor: usize,
}

impl Browser {
    pub fn indices(&self, tracks: &[Track], queue: &[usize]) -> Vec<usize> {
        if self.queue {
            return queue.to_vec();
        }
        let query = self.query.to_lowercase();
        tracks
            .iter()
            .enumerate()
            .filter_map(|(i, track)| track.title.to_lowercase().contains(&query).then_some(i))
            .collect()
    }

    pub fn clamp(&mut self, count: usize) {
        self.cursor = self.cursor.min(count.saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filtering_preserves_indices_and_queue_duplicates() {
        let tracks: Vec<_> = ["Alpha", "歌手 - 如愿", "ALBUM"]
            .iter()
            .map(|s| Track {
                path: s.into(),
                title: s.to_string(),
            })
            .collect();
        let mut browser = Browser {
            query: "al".into(),
            ..Browser::default()
        };
        assert_eq!(browser.indices(&tracks, &[]), [0, 2]);
        browser.query = "如愿".into();
        assert_eq!(browser.indices(&tracks, &[]), [1]);
        browser.query = "missing".into();
        assert!(browser.indices(&tracks, &[]).is_empty());
        browser.cursor = 99;
        browser.clamp(0);
        assert_eq!(browser.cursor, 0);
        browser.queue = true;
        assert_eq!(browser.indices(&tracks, &[2, 1, 2]), [2, 1, 2]);
    }
}
