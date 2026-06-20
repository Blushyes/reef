#[derive(Debug, Clone)]
pub struct History<T> {
    back: Vec<T>,
    forward: Vec<T>,
    cap: usize,
}

impl<T> History<T> {
    pub fn new(cap: usize) -> Self {
        Self {
            back: Vec::new(),
            forward: Vec::new(),
            cap,
        }
    }

    pub fn len(&self) -> usize {
        self.back.len()
    }

    pub fn is_empty(&self) -> bool {
        self.back.is_empty()
    }

    pub fn forward_len(&self) -> usize {
        self.forward.len()
    }

    pub fn forward_is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    pub fn push(&mut self, item: T)
    where
        T: PartialEq,
    {
        if self.back.last() == Some(&item) {
            self.forward.clear();
            return;
        }
        self.back.push(item);
        self.trim_back();
        self.forward.clear();
    }

    pub fn back(&mut self, current: Option<T>) -> Option<T> {
        let target = self.back.pop()?;
        if let Some(current) = current {
            self.forward.push(current);
            self.trim_forward();
        }
        Some(target)
    }

    pub fn forward(&mut self, current: Option<T>) -> Option<T> {
        let target = self.forward.pop()?;
        if let Some(current) = current {
            self.back.push(current);
            self.trim_back();
        }
        Some(target)
    }

    pub fn back_items(&self) -> &[T] {
        &self.back
    }

    fn trim_back(&mut self) {
        if self.cap == 0 {
            self.back.clear();
        } else if self.back.len() > self.cap {
            let overflow = self.back.len() - self.cap;
            self.back.drain(0..overflow);
        }
    }

    fn trim_forward(&mut self) {
        if self.cap == 0 {
            self.forward.clear();
        } else if self.forward.len() > self.cap {
            let overflow = self.forward.len() - self.cap;
            self.forward.drain(0..overflow);
        }
    }
}

impl<T> Default for History<T> {
    fn default() -> Self {
        Self::new(64)
    }
}

#[cfg(test)]
mod tests {
    use super::History;

    #[test]
    fn push_adds_back_entries_and_clears_forward() {
        let mut history = History::new(4);
        history.push("a");
        assert_eq!(history.back(Some("b")), Some("a"));
        assert_eq!(history.forward_len(), 1);

        history.push("c");

        assert_eq!(history.back_items(), &["c"]);
        assert!(history.forward_is_empty());
    }

    #[test]
    fn push_dedupes_adjacent_entries() {
        let mut history = History::new(4);
        history.push("a");
        history.push("a");

        assert_eq!(history.back_items(), &["a"]);
    }

    #[test]
    fn back_moves_current_to_forward() {
        let mut history = History::new(4);
        history.push("a");
        history.push("b");

        assert_eq!(history.back(Some("c")), Some("b"));

        assert_eq!(history.back_items(), &["a"]);
        assert_eq!(history.forward(Some("b")), Some("c"));
    }

    #[test]
    fn forward_moves_current_to_back() {
        let mut history = History::new(4);
        history.push("a");
        assert_eq!(history.back(Some("b")), Some("a"));

        assert_eq!(history.forward(Some("a")), Some("b"));

        assert_eq!(history.back_items(), &["a"]);
        assert!(history.forward_is_empty());
    }

    #[test]
    fn cap_drops_oldest_back_entries() {
        let mut history = History::new(2);
        history.push(1);
        history.push(2);
        history.push(3);

        assert_eq!(history.back_items(), &[2, 3]);
    }

    #[test]
    fn zero_cap_keeps_no_entries() {
        let mut history = History::new(0);
        history.push(1);

        assert!(history.is_empty());
    }
}
