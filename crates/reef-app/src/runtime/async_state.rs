#[derive(Debug, Default)]
pub struct AsyncState {
    pub generation: u64,
    pub loading: bool,
    pub stale: bool,
    pub error: Option<String>,
}

impl AsyncState {
    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    pub fn begin(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.loading = true;
        self.stale = false;
        self.error = None;
        self.generation
    }

    /// Cancel any in-flight load and reset the state to idle with a fresh
    /// generation. Late worker results carrying the previous generation are
    /// discarded by `complete_ok` / `complete_err`.
    pub fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.loading = false;
        self.stale = false;
        self.error = None;
    }

    pub fn complete_ok(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.loading = false;
        self.stale = false;
        self.error = None;
        true
    }

    pub fn complete_err(&mut self, generation: u64, error: String) -> bool {
        if generation != self.generation {
            return false;
        }
        self.loading = false;
        self.stale = true;
        self.error = Some(error);
        true
    }

    pub fn should_request(&self) -> bool {
        self.stale && !self.loading
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_stale_generations() {
        let mut state = AsyncState::default();

        let first = state.begin();
        let second = state.begin();

        assert_ne!(first, second);
        assert!(!state.complete_ok(first));
        assert!(state.loading);
        assert!(state.complete_ok(second));
        assert!(!state.loading);
    }

    #[test]
    fn error_marks_stale_for_retry() {
        let mut state = AsyncState::default();
        let generation = state.begin();

        assert!(state.complete_err(generation, "boom".to_string()));
        assert!(!state.loading);
        assert!(state.stale);
        assert!(state.should_request());
        assert_eq!(state.error.as_deref(), Some("boom"));
    }
}
