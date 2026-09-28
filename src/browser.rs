//! Library filtering keeps stable indices into the playback library.

#[derive(Default)]
pub struct Browser {
    pub query: String,
    pub editing: bool,
    pub queue: bool,
    pub cursor: usize,
}

impl Browser {
    /// `lower_titles` holds the precomputed lowercase title of each track, parallel to it.
    pub fn indices(&self, lower_titles: &[String], queue: &[usize]) -> Vec<usize> {
        if self.queue {
            return queue.to_vec();
        }
        let query = self.query.to_lowercase();
        lower_titles
            .iter()
            .enumerate()
            .filter_map(|(i, title)| title.contains(&query).then_some(i))
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
        let lower_titles: Vec<String> = ["alpha", "歌手 - 如愿", "album"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut browser = Browser {
            query: "al".into(),
            ..Browser::default()
        };
        assert_eq!(browser.indices(&lower_titles, &[]), [0, 2]);
        browser.query = "如愿".into();
        assert_eq!(browser.indices(&lower_titles, &[]), [1]);
        browser.query = "missing".into();
        assert!(browser.indices(&lower_titles, &[]).is_empty());
        browser.cursor = 99;
        browser.clamp(0);
        assert_eq!(browser.cursor, 0);
        browser.queue = true;
        assert_eq!(browser.indices(&lower_titles, &[2, 1, 2]), [2, 1, 2]);
    }
}
