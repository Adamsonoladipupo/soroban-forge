
use serde::Serialize;

/// Result of checking a contract storage entry's TTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TtlStatus {
    Healthy,
    BelowThreshold,
    Expired,
}

/// TTL information for one storage entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TtlEntry {
    pub key: String,
    pub live_until_ledger: u32,
    pub remaining_ledgers: u32,
    pub status: TtlStatus,
}

/// Evaluate an entry's remaining TTL against a minimum threshold.
pub fn audit_entry(
    key: impl Into<String>,
    current_ledger: u32,
    live_until_ledger: u32,
    minimum_ttl: u32,
) -> TtlEntry {
    let remaining = live_until_ledger.saturating_sub(current_ledger);

    let status = if remaining == 0 {
        TtlStatus::Expired
    } else if remaining < minimum_ttl {
        TtlStatus::BelowThreshold
    } else {
        TtlStatus::Healthy
    };

    TtlEntry {
        key: key.into(),
        live_until_ledger,
        remaining_ledgers: remaining,
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_healthy_entry() {
        let result = audit_entry("ContractInstance", 100, 500, 50);
        assert_eq!(result.status, TtlStatus::Healthy);
        assert_eq!(result.remaining_ledgers, 400);
    }

    #[test]
    fn identifies_entry_below_threshold() {
        let result = audit_entry("PersistentData", 100, 120, 50);
        assert_eq!(result.status, TtlStatus::BelowThreshold);
    }

    #[test]
    fn identifies_expired_entry() {
        let result = audit_entry("ArchivedData", 100, 100, 50);
        assert_eq!(result.status, TtlStatus::Expired);
    }
}
