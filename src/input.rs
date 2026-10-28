//! A small multi-line line editor with history.

use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub struct Input {
    text: String,
    /// Cursor position as a byte offset into `text` (always on a char boundary).
    cursor: usize,
    history: Vec<String>,
    browsing: Option<usize>,
    draft: String,
}

impl Input {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.browsing = None;
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        text
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.browsing = None;
    }

    pub fn insert(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    pub fn backspace(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.text.drain(prev..self.cursor);
            self.cursor = prev;
        }
    }

    pub fn delete(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.text.drain(self.cursor..next);
        }
    }

    pub fn left(&mut self) {
        if let Some(p) = self.prev_boundary() {
            self.cursor = p;
        }
    }

    pub fn right(&mut self) {
        if let Some(n) = self.next_boundary() {
            self.cursor = n;
        }
    }

    pub fn home(&mut self) {
        self.cursor = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
    }

    pub fn end(&mut self) {
        self.cursor = self.text[self.cursor..].find('\n').map_or(self.text.len(), |i| self.cursor + i);
    }

    pub fn delete_word(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end();
        let start = trimmed.rfind(|c: char| c.is_whitespace()).map_or(0, |i| i + 1);
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }

    pub fn kill_line(&mut self) {
        let start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }

    pub fn is_multiline(&self) -> bool {
        self.text.contains('\n')
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.browsing {
            None => {
                self.draft = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.browsing = Some(idx);
        self.text = self.history[idx].clone();
        self.cursor = self.text.len();
    }

    pub fn history_next(&mut self) {
        let Some(i) = self.browsing else { return };
        if i + 1 < self.history.len() {
            self.browsing = Some(i + 1);
            self.text = self.history[i + 1].clone();
        } else {
            self.browsing = None;
            self.text = std::mem::take(&mut self.draft);
        }
        self.cursor = self.text.len();
    }

    /// Line index and display column of the cursor.
    pub fn cursor_pos(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let row = before.matches('\n').count();
        let col = before.rsplit('\n').next().unwrap_or("").width();
        (row, col)
    }

    fn prev_boundary(&self) -> Option<usize> {
        self.text[..self.cursor].char_indices().next_back().map(|(i, _)| i)
    }

    fn next_boundary(&self) -> Option<usize> {
        self.text[self.cursor..].chars().next().map(|c| self.cursor + c.len_utf8())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_and_history() {
        let mut i = Input::default();
        i.insert("héllo wörld");
        i.delete_word();
        assert_eq!(i.text(), "héllo ");
        i.backspace();
        i.left();
        i.insert("X");
        assert_eq!(i.text(), "héllXo");
        assert_eq!(i.take(), "héllXo");
        i.insert("draft");
        i.history_prev();
        assert_eq!(i.text(), "héllXo");
        i.history_next();
        assert_eq!(i.text(), "draft");
    }
}
