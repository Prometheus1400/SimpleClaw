//! Domain filtering logic for proxy servers.

use crate::config::{matches_domain_pattern, NetworkConfig};

/// Filter decision for a domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterDecision {
    /// Allow the connection.
    Allow,
    /// Deny the connection.
    Deny,
    /// Route through MITM proxy.
    Mitm,
}

/// Domain filter for proxy connections.
#[derive(Debug, Clone)]
pub struct DomainFilter {
    allowed_domains: Vec<String>,
    denied_domains: Vec<String>,
    mitm_domains: Vec<String>,
}

impl DomainFilter {
    /// Create a new domain filter from network config.
    pub fn from_config(config: &NetworkConfig) -> Self {
        let mitm_domains = config
            .mitm_proxy
            .as_ref()
            .map(|m| m.domains.clone())
            .unwrap_or_default();

        Self {
            allowed_domains: config.allowed_domains.clone(),
            denied_domains: config.denied_domains.clone(),
            mitm_domains,
        }
    }

    /// Create an allow-all filter.
    pub fn allow_all() -> Self {
        Self {
            allowed_domains: vec![],
            denied_domains: vec![],
            mitm_domains: vec![],
        }
    }

    /// Check if a domain should be allowed, denied, or routed through MITM.
    pub fn check(&self, hostname: &str, _port: u16) -> FilterDecision {
        // Check denied list first (highest priority)
        for pattern in &self.denied_domains {
            if matches_domain_pattern(hostname, pattern) {
                return FilterDecision::Deny;
            }
        }

        // Check MITM list
        for pattern in &self.mitm_domains {
            if matches_domain_pattern(hostname, pattern) {
                return FilterDecision::Mitm;
            }
        }

        // If we have an allow list, check against it
        if !self.allowed_domains.is_empty() {
            for pattern in &self.allowed_domains {
                if matches_domain_pattern(hostname, pattern) {
                    return FilterDecision::Allow;
                }
            }
            // Not in allow list = denied
            return FilterDecision::Deny;
        }

        // No allow list = allow all (except denied)
        FilterDecision::Allow
    }

    /// Check if a domain is allowed.
    pub fn is_allowed(&self, hostname: &str, port: u16) -> bool {
        matches!(self.check(hostname, port), FilterDecision::Allow | FilterDecision::Mitm)
    }

    /// Check if a domain should be routed through MITM.
    pub fn should_mitm(&self, hostname: &str) -> bool {
        for pattern in &self.mitm_domains {
            if matches_domain_pattern(hostname, pattern) {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_domain_filter_allow_all() {
        let filter = DomainFilter::allow_all();
        assert_eq!(filter.check("example.com", 443), FilterDecision::Allow);
        assert_eq!(filter.check("evil.com", 443), FilterDecision::Allow);
    }

    #[test]
    fn test_domain_filter_with_allowed() {
        let filter = DomainFilter {
            allowed_domains: vec!["github.com".to_string(), "*.npmjs.org".to_string()],
            denied_domains: vec![],
            mitm_domains: vec![],
        };

        assert_eq!(filter.check("github.com", 443), FilterDecision::Allow);
        assert_eq!(filter.check("registry.npmjs.org", 443), FilterDecision::Allow);
        assert_eq!(filter.check("evil.com", 443), FilterDecision::Deny);
    }

    #[test]
    fn test_domain_filter_with_denied() {
        let filter = DomainFilter {
            allowed_domains: vec!["*.example.com".to_string()],
            denied_domains: vec!["evil.example.com".to_string()],
            mitm_domains: vec![],
        };

        assert_eq!(filter.check("api.example.com", 443), FilterDecision::Allow);
        assert_eq!(filter.check("evil.example.com", 443), FilterDecision::Deny);
    }

    #[test]
    fn test_domain_filter_with_mitm() {
        let filter = DomainFilter {
            allowed_domains: vec!["*.example.com".to_string()],
            denied_domains: vec![],
            mitm_domains: vec!["api.example.com".to_string()],
        };

        assert_eq!(filter.check("api.example.com", 443), FilterDecision::Mitm);
        assert_eq!(filter.check("other.example.com", 443), FilterDecision::Allow);
    }
}
