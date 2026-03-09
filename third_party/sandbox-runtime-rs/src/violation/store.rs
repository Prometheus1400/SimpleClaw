//! In-memory violation store.

use std::sync::Arc;

use parking_lot::RwLock;

/// Maximum number of violations to store.
const MAX_VIOLATIONS: usize = 100;

/// A sandbox violation event.
#[derive(Debug, Clone)]
pub struct SandboxViolationEvent {
    /// The full violation line from the log.
    pub line: String,
    /// The original command that triggered the violation.
    pub command: Option<String>,
    /// The base64-encoded command identifier.
    pub encoded_command: Option<String>,
    /// When the violation occurred.
    pub timestamp: std::time::SystemTime,
}

impl SandboxViolationEvent {
    /// Create a new violation event.
    pub fn new(line: String) -> Self {
        Self {
            line,
            command: None,
            encoded_command: None,
            timestamp: std::time::SystemTime::now(),
        }
    }

    /// Create a new violation event with command info.
    pub fn with_command(line: String, command: Option<String>, encoded: Option<String>) -> Self {
        Self {
            line,
            command,
            encoded_command: encoded,
            timestamp: std::time::SystemTime::now(),
        }
    }
}

/// Type for violation listeners.
pub type ViolationListener = Box<dyn Fn(&SandboxViolationEvent) + Send + Sync>;

/// In-memory store for sandbox violations.
pub struct SandboxViolationStore {
    violations: RwLock<Vec<SandboxViolationEvent>>,
    total_count: RwLock<usize>,
    listeners: RwLock<Vec<Arc<ViolationListener>>>,
}

impl Default for SandboxViolationStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxViolationStore {
    /// Create a new violation store.
    pub fn new() -> Self {
        Self {
            violations: RwLock::new(Vec::new()),
            total_count: RwLock::new(0),
            listeners: RwLock::new(Vec::new()),
        }
    }

    /// Add a violation to the store.
    pub fn add_violation(&self, violation: SandboxViolationEvent) {
        // Notify listeners
        let listeners = self.listeners.read();
        for listener in listeners.iter() {
            listener(&violation);
        }
        drop(listeners);

        // Add to store
        let mut violations = self.violations.write();
        let mut total = self.total_count.write();

        violations.push(violation);
        *total += 1;

        // Trim to max size
        if violations.len() > MAX_VIOLATIONS {
            violations.remove(0);
        }
    }

    /// Get all violations (up to a limit).
    pub fn get_violations(&self, limit: Option<usize>) -> Vec<SandboxViolationEvent> {
        let violations = self.violations.read();
        let limit = limit.unwrap_or(violations.len());
        violations.iter().take(limit).cloned().collect()
    }

    /// Get the current count of stored violations.
    pub fn get_count(&self) -> usize {
        self.violations.read().len()
    }

    /// Get the total count of all violations (including trimmed).
    pub fn get_total_count(&self) -> usize {
        *self.total_count.read()
    }

    /// Get violations for a specific command.
    pub fn get_violations_for_command(&self, command: &str) -> Vec<SandboxViolationEvent> {
        let violations = self.violations.read();
        violations
            .iter()
            .filter(|v| v.command.as_ref().map(|c| c == command).unwrap_or(false))
            .cloned()
            .collect()
    }

    /// Clear all violations.
    pub fn clear(&self) {
        let mut violations = self.violations.write();
        let mut total = self.total_count.write();
        violations.clear();
        *total = 0;
    }

    /// Subscribe to new violations.
    /// Returns a function to unsubscribe.
    pub fn subscribe(&self, listener: ViolationListener) -> usize {
        let mut listeners = self.listeners.write();
        let id = listeners.len();
        listeners.push(Arc::new(listener));
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_get_violations() {
        let store = SandboxViolationStore::new();

        store.add_violation(SandboxViolationEvent::new("violation 1".to_string()));
        store.add_violation(SandboxViolationEvent::new("violation 2".to_string()));

        assert_eq!(store.get_count(), 2);
        assert_eq!(store.get_total_count(), 2);

        let violations = store.get_violations(None);
        assert_eq!(violations.len(), 2);
        assert_eq!(violations[0].line, "violation 1");
        assert_eq!(violations[1].line, "violation 2");
    }

    #[test]
    fn test_max_violations() {
        let store = SandboxViolationStore::new();

        // Add more than MAX_VIOLATIONS
        for i in 0..(MAX_VIOLATIONS + 10) {
            store.add_violation(SandboxViolationEvent::new(format!("violation {}", i)));
        }

        assert_eq!(store.get_count(), MAX_VIOLATIONS);
        assert_eq!(store.get_total_count(), MAX_VIOLATIONS + 10);
    }

    #[test]
    fn test_clear() {
        let store = SandboxViolationStore::new();

        store.add_violation(SandboxViolationEvent::new("violation".to_string()));
        assert_eq!(store.get_count(), 1);

        store.clear();
        assert_eq!(store.get_count(), 0);
        assert_eq!(store.get_total_count(), 0);
    }
}
